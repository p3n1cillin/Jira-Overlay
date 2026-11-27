//! "Completed today" and SLA compliance: the JQL and the slow-path counts.

use chrono::{Datelike, NaiveDate, Weekday};
use jo_core::config::{self, Config};
use jo_core::jql;

use crate::model::{AtRisk, SlaTrend};
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
    let sla = sla_pct(&base, total, &fields, sla_unavailable, count);
    Completion { completed_today: total, sla_compliance: sla }
}

/// `("A" = everBreached() OR "B" = everBreached())`: any cycle of any of the
/// SLAs was missed, as Jira's own compliance report counts it.
pub fn breach_clause(fields: &[String]) -> String {
    let breach = fields.iter().map(|name| format!("{} = everBreached()", jql::quote(name))).collect::<Vec<_>>();
    format!("({})", breach.join(" OR "))
}

/// SLA % of `total` tickets matching `base`: one breach count. A rejected
/// breach query latches SLA off for the session.
fn sla_pct(
    base: &str,
    total: u64,
    fields: &[String],
    sla_unavailable: &mut bool,
    count: &dyn Fn(&str) -> (Option<u64>, bool),
) -> Option<i64> {
    let (breached, rejected) = count(&format!("{base} AND {}", breach_clause(fields)));
    match breached {
        None => {
            *sla_unavailable = *sla_unavailable || rejected;
            None
        }
        Some(b) => Some(util::py_round((total as f64 - b.min(total) as f64) / total as f64 * 100.0)),
    }
}

/// Whether SLA figures are wanted at all (switch on, a field named).
pub fn sla_wanted(cfg: &Config) -> bool {
    config::truthy(cfg, "slaEnabled", true) && !sla_fields(cfg).is_empty()
}

/// Minutes left under which a running SLA counts as at risk; 0 = off.
pub const AT_RISK_DEFAULT_MINUTES: i64 = 30;

pub fn at_risk_minutes(cfg: &Config) -> i64 {
    util::cfg_int(cfg, "slaAtRiskMinutes", AT_RISK_DEFAULT_MINUTES).clamp(0, 7 * 24 * 60)
}

/// THE at-risk JQL: open tickets in the overlay's project (with the same
/// team filter as completed-today) where any configured SLA is running and
/// has under N minutes left. Per SLA field `F`:
/// `(F = running() AND F != breached() AND F < remaining("30m"))` (an
/// overdue SLA can keep running with negative time left; it is breached,
/// not at risk), the fields OR-ed together
/// (the shape checked against the live Jira). `= running()` limits it to
/// cycles that are running, not paused or completed. A running cycle that
/// has already breached has negative time left and still matches; adding
/// `AND F != breached()` per field would leave those out. `None` when SLA
/// is off, no project is known or the warning is switched off.
pub fn sla_at_risk_jql(cfg: &Config) -> Option<String> {
    let pk = config::get_str(cfg, "projectKey", "");
    let minutes = at_risk_minutes(cfg);
    if pk.is_empty() || minutes <= 0 || !sla_wanted(cfg) {
        return None;
    }
    let per_sla: Vec<String> = sla_fields(cfg)
        .iter()
        .map(|name| {
            let f = jql::quote(name);
            format!("({f} = running() AND {f} != breached() AND {f} < remaining(\"{minutes}m\"))")
        })
        .collect();
    let base = format!("project = {pk} AND statusCategory != Done AND ({})", per_sla.join(" OR "));
    Some(with_team_filter(cfg, base))
}

/// Append the completed-today team filter in parentheses, so an `OR`
/// inside it cannot escape the project scope.
fn with_team_filter(cfg: &Config, base: String) -> String {
    let extra = config::get_str(cfg, "completedTodayFilter", "").trim();
    if extra.is_empty() { base } else { format!("{base} AND ({extra})") }
}

/// The at-risk page size: one search request returns keys and, when the
/// page is complete, the count.
pub const AT_RISK_PAGE: usize = 50;

/// Parse one `/search/jql` page into the at-risk tickets. `more` is true
/// when Jira says there are further pages (the caller then counts).
pub fn parse_at_risk(body: &serde_json::Value) -> Option<(AtRisk, bool)> {
    let issues = body.get("issues")?.as_array()?;
    let tickets: Vec<(String, Option<String>)> = issues
        .iter()
        .filter_map(|i| {
            let key = i.get("key")?.as_str().filter(|k| !k.trim().is_empty())?.to_owned();
            let summary = i.get("fields").and_then(|f| f.get("summary")).and_then(|s| s.as_str()).map(str::to_owned);
            Some((key, summary))
        })
        .collect();
    let next = body.get("nextPageToken").and_then(|t| t.as_str()).is_some_and(|t| !t.is_empty());
    let more = next && body.get("isLast") != Some(&serde_json::Value::Bool(true));
    Some((AtRisk { count: tickets.len() as u64, tickets }, more))
}

/// `(start, end)` day offsets (as in `startOfDay(-n)`) of the `days`
/// working days before today's completed/SLA window. With `weekends_off`
/// Saturdays and Sundays are skipped when counting days back (the range is
/// contiguous, so weekend updates inside it still count, as they do in
/// Monday's figure); otherwise these are calendar days.
pub fn trend_offsets(today: NaiveDate, weekends_off: bool, days: u32) -> (i64, i64) {
    let end: i64 = if weekends_off {
        match today.weekday() {
            Weekday::Sat => 1,
            Weekday::Sun | Weekday::Mon => 2,
            _ => 0,
        }
    } else {
        0
    };
    let mut offset = end;
    let mut counted = 0;
    while counted < days {
        offset += 1;
        let date = today - chrono::Days::new(offset as u64);
        if !weekends_off || !matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
            counted += 1;
        }
    }
    (offset, end)
}

fn start_of_day(offset: i64) -> String {
    if offset == 0 { "startOfDay()".into() } else { format!("startOfDay(-{offset})") }
}

/// Completed tickets over the `days` working days before today's window,
/// on the same `updated` basis and team filter as completed-today.
pub fn trend_jql(cfg: &Config, today: NaiveDate, days: u32) -> String {
    let pk = config::get_str(cfg, "projectKey", "");
    let (start, end) = trend_offsets(today, weekends_off(cfg), days);
    let base = format!(
        "project = {pk} AND statusCategory = Done AND updated >= {} AND updated < {}",
        start_of_day(start),
        start_of_day(end)
    );
    with_team_filter(cfg, base)
}

/// The trend windows shown next to today's SLA %.
pub const TREND_DAYS: [u32; 2] = [5, 20];

/// 5- and 20-working-day SLA %: at most four counts. `None` when SLA is
/// off, latched off, or there is no project.
pub fn sla_trend(
    cfg: &Config,
    today: NaiveDate,
    sla_unavailable: &mut bool,
    count: &dyn Fn(&str) -> (Option<u64>, bool),
) -> Option<SlaTrend> {
    if config::get_str(cfg, "projectKey", "").is_empty() || !sla_wanted(cfg) || *sla_unavailable {
        return None;
    }
    let fields = sla_fields(cfg);
    let mut pcts = [None, None];
    for (slot, days) in pcts.iter_mut().zip(TREND_DAYS) {
        if *sla_unavailable {
            break;
        }
        let base = trend_jql(cfg, today, days);
        *slot = match count(&base).0 {
            Some(total) if total > 0 => sla_pct(&base, total, &fields, sla_unavailable, count),
            _ => None,
        };
    }
    Some(SlaTrend { five: pcts[0], twenty: pcts[1] })
}

/// `"5d 94% · 20d 95%"` (a window with nothing completed shows "—").
pub fn trend_text(trend: &SlaTrend) -> String {
    let show = |p: Option<i64>| p.map_or_else(|| "—".to_owned(), |p| format!("{p}%"));
    format!("{}d {} · {}d {}", TREND_DAYS[0], show(trend.five), TREND_DAYS[1], show(trend.twenty))
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

    #[test]
    fn at_risk_jql_exact_string() {
        let c = cfg(json!({"projectKey": "SD"}));
        assert_eq!(
            sla_at_risk_jql(&c).unwrap(),
            r#"project = SD AND statusCategory != Done AND (("Time to resolution" = running() AND "Time to resolution" != breached() AND "Time to resolution" < remaining("30m")))"#,
            "the JSM default SLA, 30 minutes"
        );
        let c = cfg(json!({"projectKey": "SD", "slaAtRiskMinutes": 45,
                           "slaField": "Time to resolution, Time to first response",
                           "completedTodayFilter": "assignee = a OR assignee = b"}));
        assert_eq!(
            sla_at_risk_jql(&c).unwrap(),
            concat!(
                r#"project = SD AND statusCategory != Done AND (("Time to resolution" = running() AND "Time to resolution" != breached() AND "Time to resolution" < remaining("45m"))"#,
                r#" OR ("Time to first response" = running() AND "Time to first response" != breached() AND "Time to first response" < remaining("45m")))"#,
                " AND (assignee = a OR assignee = b)"
            ),
            "every SLA field, and the team filter in its own parentheses"
        );
        let quoted = sla_at_risk_jql(&cfg(json!({"projectKey": "SD", "slaField": "Odd \"name"}))).unwrap();
        assert!(
            quoted.contains(
                r#"("Odd \"name" = running() AND "Odd \"name" != breached() AND "Odd \"name" < remaining("30m"))"#
            ),
            "{quoted}"
        );
        for off in [
            json!({"projectKey": "SD", "slaAtRiskMinutes": 0}),
            json!({"projectKey": "SD", "slaEnabled": false}),
            json!({"projectKey": "SD", "slaField": ""}),
            json!({"projectKey": ""}),
        ] {
            assert!(sla_at_risk_jql(&cfg(off.clone())).is_none(), "{off}");
        }
        assert_eq!(at_risk_minutes(&cfg(json!({"slaAtRiskMinutes": -5}))), 0);
        assert_eq!(at_risk_minutes(&cfg(json!({"slaAtRiskMinutes": "x"}))), 30, "junk falls back to the default");
    }

    #[test]
    fn at_risk_pages() {
        let (risk, more) = parse_at_risk(&json!({"issues": [
            {"key": "SD-1", "fields": {"summary": "Printer"}}, {"key": ""}, {"nokey": 1}, {"key": "SD-2"}],
            "isLast": true}))
        .unwrap();
        assert_eq!(risk.count, 2);
        assert_eq!(risk.tickets, [("SD-1".into(), Some("Printer".into())), ("SD-2".into(), None)]);
        assert!(!more);
        let (_, more) = parse_at_risk(&json!({"issues": [], "nextPageToken": "t"})).unwrap();
        assert!(more, "another page: the caller counts instead");
        assert!(parse_at_risk(&json!({"issues": null})).is_none());
        assert!(parse_at_risk(&json!([1])).is_none());
    }

    #[test]
    fn trend_windows_count_working_days() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        // Calendar days without weekendsOff.
        assert_eq!(trend_offsets(d(11, 18), false, 5), (5, 0));
        assert_eq!(trend_offsets(d(11, 21), false, 20), (20, 0));
        // Wed 18 Nov: Tue, Mon, Fri, Thu, Wed before.
        assert_eq!(trend_offsets(d(11, 18), true, 5), (7, 0));
        // Mon 16 Nov: today's window starts Saturday; Fri..previous Mon.
        assert_eq!(trend_offsets(d(11, 16), true, 5), (7, 2));
        // Sat 21: "today" is Friday; Thu..previous Fri.
        assert_eq!(trend_offsets(d(11, 21), true, 5), (8, 1));
        assert_eq!(trend_offsets(d(11, 22), true, 5), (9, 2));
        // 20 working days from a Wednesday: four whole weeks back.
        assert_eq!(trend_offsets(d(11, 18), true, 20), (28, 0));
        let c = cfg(json!({"projectKey": "SD", "weekendsOff": true, "completedTodayFilter": "assignee = x"}));
        assert_eq!(
            trend_jql(&c, d(11, 16), 5),
            "project = SD AND statusCategory = Done AND updated >= startOfDay(-7) AND updated < startOfDay(-2) AND (assignee = x)"
        );
        let c = cfg(json!({"projectKey": "SD"}));
        assert_eq!(
            trend_jql(&c, d(11, 18), 20),
            "project = SD AND statusCategory = Done AND updated >= startOfDay(-20) AND updated < startOfDay()"
        );
    }

    #[test]
    fn trend_uses_at_most_four_counts() {
        let c = cfg(json!({"projectKey": "SD"}));
        let seen = RefCell::new(Vec::new());
        let mut latch = false;
        let today = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();
        let out = sla_trend(&c, today, &mut latch, &|jql: &str| {
            seen.borrow_mut().push(jql.to_owned());
            if jql.contains("everBreached") { (Some(1), false) } else { (Some(20), false) }
        })
        .unwrap();
        assert_eq!(out, SlaTrend { five: Some(95), twenty: Some(95) });
        assert_eq!(seen.borrow().len(), 4);
        assert!(seen.borrow()[1].ends_with(r#"AND ("Time to resolution" = everBreached())"#));
        assert_eq!(trend_text(&out), "5d 95% · 20d 95%");
        assert_eq!(trend_text(&SlaTrend { five: None, twenty: Some(80) }), "5d — · 20d 80%");

        // A window with nothing completed costs one count and shows "—".
        seen.borrow_mut().clear();
        let out = sla_trend(&c, today, &mut latch, &|jql: &str| {
            seen.borrow_mut().push(jql.to_owned());
            (Some(0), false)
        });
        assert_eq!(out, Some(SlaTrend::default()));
        assert_eq!(seen.borrow().len(), 2);

        // A rejected breach query latches SLA off and stops.
        seen.borrow_mut().clear();
        let out = sla_trend(&c, today, &mut latch, &|jql: &str| {
            seen.borrow_mut().push(jql.to_owned());
            if jql.contains("everBreached") { (None, true) } else { (Some(3), false) }
        });
        assert!(latch && out == Some(SlaTrend::default()) && seen.borrow().len() == 2);
        assert!(sla_trend(&c, today, &mut latch, &|_| panic!("latched")).is_none());
        let mut fresh = false;
        assert!(
            sla_trend(&cfg(json!({"projectKey": "SD", "slaEnabled": false})), today, &mut fresh, &|_| panic!())
                .is_none()
        );
    }
}
