//! "Completed today" and SLA compliance: the JQL and the slow-path counts.

use chrono::{Datelike, NaiveDate, Weekday};
use jo_core::config::{self, Config};
use jo_core::jql;

use crate::util;

/// `(JQL date clause, label)` for the completed/SLA figures. With
/// `weekends_off`, Saturday and Sunday keep showing Friday and Monday takes
/// in the weekend.
pub fn working_day_window(today: NaiveDate, weekends_off: bool) -> (&'static str, &'static str) {
    if !weekends_off {
        return ("updated >= startOfDay()", "today");
    }
    match today.weekday() {
        Weekday::Sat => ("updated >= startOfDay(-1) AND updated < startOfDay()", "Fri"),
        Weekday::Sun => ("updated >= startOfDay(-2) AND updated < startOfDay(-1)", "Fri"),
        Weekday::Mon => ("updated >= startOfDay(-2)", "today"),
        _ => ("updated >= startOfDay()", "today"),
    }
}

/// `weekendsOff` is opt-in: only a JSON `true` counts.
pub fn weekends_off(cfg: &Config) -> bool {
    config::is_true(cfg, "weekendsOff")
}

pub fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

/// JQL behind the "Completed today" figure and its Jira link. A custom
/// filter is appended in parentheses, so an `OR` inside it cannot escape the
/// project/date scope (deliberate change from the Python build).
pub fn completed_today_jql(cfg: &Config, today: NaiveDate) -> String {
    let pk = config::get_str(cfg, "projectKey", "");
    let window = working_day_window(today, weekends_off(cfg)).0;
    let base = format!("project = {pk} AND statusCategory = Done AND {window}");
    let extra = config::get_str(cfg, "completedTodayFilter", "").trim();
    if extra.is_empty() { base } else { format!("{base} AND ({extra})") }
}

/// SLA field names: comma-separated; absent means the JSM default, present
/// but empty means "off".
pub fn sla_fields(cfg: &Config) -> Vec<String> {
    let raw = match cfg.get("slaField") {
        None => "Time to resolution",
        Some(v) => v.as_str().unwrap_or(""),
    };
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

/// Result of one slow-path pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    pub completed_today: u64,
    pub sla_compliance: Option<i64>,
}

/// Python `_fetch_completion_stats`. `count` is `Jira::count`;
/// `sla_unavailable` latches (for this session only) once Jira rejects the
/// breach query outright.
pub fn completion_stats(
    cfg: &Config,
    today: NaiveDate,
    sla_unavailable: &mut bool,
    count: &dyn Fn(&str) -> (Option<u64>, bool),
) -> Completion {
    let none = Completion { completed_today: 0, sla_compliance: None };
    if config::get_str(cfg, "projectKey", "").is_empty() {
        return none;
    }
    let base = completed_today_jql(cfg, today);
    let Some(total) = count(&base).0 else { return none };
    let fields = sla_fields(cfg);
    if !config::truthy(cfg, "slaEnabled", true) || fields.is_empty() || *sla_unavailable || total == 0 {
        return Completion { completed_today: total, sla_compliance: None };
    }
    // everBreached(): any cycle of any of the SLAs was missed, as Jira's
    // own compliance report counts it.
    let breach = fields.iter().map(|name| format!("{} = everBreached()", jql::quote(name))).collect::<Vec<_>>();
    let (breached, rejected) = count(&format!("{base} AND ({})", breach.join(" OR ")));
    let sla = match breached {
        None => {
            *sla_unavailable = *sla_unavailable || rejected;
            None
        }
        Some(b) => Some(util::py_round((total as f64 - b as f64) / total as f64 * 100.0)),
    };
    Completion { completed_today: total, sla_compliance: sla }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::cell::RefCell;

    fn cfg(v: Value) -> Config {
        v.as_object().unwrap().clone()
    }

    /// One completion-stats pass: (sla, queries issued, latch).
    fn stats(extra: Value, counts: Vec<(Option<u64>, bool)>) -> (Option<i64>, Vec<String>, bool) {
        let mut c = cfg(json!({"projectKey": "SD", "completedTodayFilter": ""}));
        c.extend(cfg(extra));
        let seen = RefCell::new(Vec::new());
        let answers = RefCell::new(counts.into_iter());
        let mut latch = false;
        let out = completion_stats(&c, NaiveDate::from_ymd_opt(2026, 11, 18).unwrap(), &mut latch, &|jql: &str| {
            seen.borrow_mut().push(jql.to_owned());
            answers.borrow_mut().next().unwrap()
        });
        (out.sla_compliance, seen.into_inner(), latch)
    }

    #[test]
    fn sla_switches_and_failures() {
        let (sla, q, _) = stats(json!({}), vec![(Some(10), false), (Some(2), false)]);
        assert!(sla == Some(80) && q.len() == 2, "SLA is measured by default");
        let (sla, q, _) = stats(json!({"slaEnabled": false}), vec![(Some(10), false)]);
        assert!(sla.is_none() && q.len() == 1, "switched off, Jira is not asked about SLA at all");
        let (sla, q, _) = stats(json!({"slaField": ""}), vec![(Some(10), false)]);
        assert!(sla.is_none() && q.len() == 1, "clearing the field name also turns it off");
        let (sla, q, latch) = stats(json!({}), vec![(Some(10), false), (None, true)]);
        assert!(sla.is_none() && q.len() == 2 && latch, "a missing SLA field is detected (session-only latch)");
        let (_, _, latch) = stats(json!({}), vec![(Some(10), false), (None, false)]);
        assert!(!latch, "a transient failure does not latch SLA off");
        let (_, q, _) = stats(json!({"slaEnabled": false}), vec![(Some(37), false)]);
        assert_eq!(q.len(), 1, "completed-today still counts with SLA switched off");
    }

    #[test]
    fn several_slas_any_breach_counts() {
        let (sla, q, _) = stats(
            json!({"slaField": "Time to first response, Time to Start,Time to resolution"}),
            vec![(Some(40), false), (Some(3), false)],
        );
        assert_eq!(sla, Some(92));
        assert!(q[1].ends_with(
            r#" AND ("Time to first response" = everBreached() OR "Time to Start" = everBreached() OR "Time to resolution" = everBreached())"#
        ));
        assert!(!q.iter().any(|j| j.contains("slaBreached")));
        let (_, q, _) = stats(json!({"slaField": "Odd \"name"}), vec![(Some(5), false), (Some(0), false)]);
        assert!(q[1].contains(r#""Odd \"name" = everBreached()"#), "SLA names are quoted safely");
    }

    #[test]
    fn no_project_or_failed_total() {
        let mut latch = false;
        let empty = cfg(json!({}));
        let out = completion_stats(&empty, today(), &mut latch, &|_| panic!("must not query"));
        assert_eq!(out, Completion { completed_today: 0, sla_compliance: None });
        let (sla, q, _) = stats(json!({}), vec![(None, false)]);
        assert!(sla.is_none() && q.len() == 1);
    }

    #[test]
    fn weekend_window() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let week = [
            (d(11, 16), ("updated >= startOfDay(-2)", "today")),
            (d(11, 18), ("updated >= startOfDay()", "today")),
            (d(11, 20), ("updated >= startOfDay()", "today")),
            (d(11, 21), ("updated >= startOfDay(-1) AND updated < startOfDay()", "Fri")),
            (d(11, 22), ("updated >= startOfDay(-2) AND updated < startOfDay(-1)", "Fri")),
        ];
        for (day, want) in week {
            assert_eq!(working_day_window(day, true), want);
            assert_eq!(working_day_window(day, false), ("updated >= startOfDay()", "today"));
        }
    }

    #[test]
    fn completed_jql_wraps_custom_filter() {
        let sat = NaiveDate::from_ymd_opt(2026, 11, 21).unwrap();
        let mut c = cfg(json!({"projectKey": "SD", "completedTodayFilter": "assignee = x"}));
        assert_eq!(
            completed_today_jql(&c, sat),
            "project = SD AND statusCategory = Done AND updated >= startOfDay() AND (assignee = x)",
            "weekend handling is off unless the desk opts in"
        );
        c.insert("weekendsOff".into(), json!(true));
        assert_eq!(
            completed_today_jql(&c, sat),
            "project = SD AND statusCategory = Done AND updated >= startOfDay(-1) AND updated < startOfDay() AND (assignee = x)"
        );
        // Deliberate change 3: an OR in the filter can no longer widen the scope.
        c.insert("completedTodayFilter".into(), json!("assignee = a OR assignee = b"));
        assert!(completed_today_jql(&c, sat).ends_with("AND (assignee = a OR assignee = b)"));
        c.insert("completedTodayFilter".into(), json!("   "));
        assert!(completed_today_jql(&c, sat).ends_with("updated < startOfDay()"));
        c.insert("weekendsOff".into(), json!("yes"));
        assert!(completed_today_jql(&c, sat).ends_with("updated >= startOfDay()"), "only JSON true opts in");
    }
}
