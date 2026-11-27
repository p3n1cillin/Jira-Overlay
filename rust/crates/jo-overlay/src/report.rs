//! Team report: who completed the most over a chosen period.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use chrono::{Days, NaiveDate};
use jo_core::config::{self, Config};
use jo_core::jql;
use jo_jira::{Method, Retry};
use regex::Regex;
use serde_json::{Value, json};

use crate::poll::{self, MakeTransport};
use crate::stats;

/// Label -> days back from today; `None` means "pick the dates yourself".
pub const REPORT_RANGES: [(&str, Option<u64>); 5] =
    [("Today", Some(0)), ("7 days", Some(6)), ("30 days", Some(29)), ("90 days", Some(89)), ("Custom", None)];

/// Which Jira date a ticket is counted against.
pub const REPORT_BASES: [(&str, &str); 2] = [("Resolved date", "resolved"), ("Last updated", "updated")];

/// The range shown for a saved `reportRange`; anything unknown falls back to
/// "30 days" (deliberate change).
pub fn initial_range(cfg: &Config) -> String {
    let saved = config::get_str(cfg, "reportRange", "30 days");
    if REPORT_RANGES.iter().any(|(label, _)| *label == saved) { saved.to_owned() } else { "30 days".to_owned() }
}

/// One person the report covers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Person {
    pub name: String,
    pub account_id: String,
}

impl Person {
    pub fn new(name: &str, account_id: &str) -> Self {
        Self { name: name.to_owned(), account_id: account_id.to_owned() }
    }

    fn key(&self) -> String {
        if self.account_id.is_empty() {
            format!("name:{}", self.name.trim().to_lowercase())
        } else {
            self.account_id.clone()
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "accountId": self.account_id})
    }
}

static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]+)""#).expect("valid regex"));

/// One entry per human: matched on accountId where there is one, and a bare
/// name that also appears with an accountId is that same person.
fn dedupe(people: Vec<Person>) -> Vec<Person> {
    let identified: HashSet<String> =
        people.iter().filter(|p| !p.account_id.is_empty()).map(|p| p.name.to_lowercase()).collect();
    let mut seen = HashSet::new();
    people
        .into_iter()
        .filter(|p| {
            let name = p.name.to_lowercase();
            if p.account_id.is_empty() && identified.contains(&name) {
                return false;
            }
            let key = if p.account_id.is_empty() { format!("name:{name}") } else { p.account_id.clone() };
            seen.insert(key)
        })
        .collect()
}

/// People to report on: `reportUsers` (objects, or plain names from a hand
/// edit), else seeded from the completed-today filter's quoted names.
pub fn report_users(cfg: &Config) -> Vec<Person> {
    if let Some(Value::Array(raw)) = cfg.get("reportUsers")
        && !raw.is_empty()
    {
        let people = raw
            .iter()
            .filter_map(|item| match item {
                Value::String(s) if !s.trim().is_empty() => Some(Person::new(s, "")),
                Value::Object(o) => {
                    let name = o.get("name").filter(|v| crate::util::truthy(Some(v)))?;
                    let account = o.get("accountId").filter(|v| crate::util::truthy(Some(v)));
                    Some(Person::new(&crate::util::py_str(name), &account.map(crate::util::py_str).unwrap_or_default()))
                }
                _ => None,
            })
            .collect();
        return dedupe(people);
    }
    let filter = config::get_str(cfg, "completedTodayFilter", "");
    dedupe(QUOTED.captures_iter(filter).map(|c| Person::new(&c[1], "")).collect())
}

/// `date.fromisoformat` for `yyyy-mm-dd`.
fn iso_date(text: &str) -> Option<NaiveDate> {
    let text = text.trim();
    (text.len() == 10).then(|| NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()).flatten()
}

/// `(start, end)` inclusive, or `None` if the typed dates make no sense.
/// Back-to-front custom dates are swapped.
pub fn period(range: &str, from: &str, to: &str, today: NaiveDate) -> Option<(NaiveDate, NaiveDate)> {
    if let Some((_, Some(days))) = REPORT_RANGES.iter().find(|(label, _)| *label == range) {
        return Some((today - Days::new(*days), today));
    }
    let (start, end) = (iso_date(from)?, iso_date(to)?);
    Some(if start <= end { (start, end) } else { (end, start) })
}

/// Neutralise a value a spreadsheet would read as a formula ("CSV injection").
pub fn csv_safe(value: &str) -> String {
    if value.starts_with(['=', '+', '-', '@', '\t', '\r']) { format!("'{value}") } else { value.to_owned() }
}

/// The base JQL. `basis` names a field, so only the two the UI offers are
/// ever interpolated; anything else counts by resolution date.
pub fn base_jql(project_key: &str, basis: &str, start: NaiveDate, end: NaiveDate) -> String {
    let basis = if REPORT_BASES.iter().any(|(_, b)| *b == basis) { basis } else { "resolved" };
    let after = end + Days::new(1); // `end` is inclusive
    format!(
        "project = {project_key} AND statusCategory = Done AND {basis} >= \"{}\" AND {basis} < \"{}\"",
        start.format("%Y-%m-%d"),
        after.format("%Y-%m-%d")
    )
}

/// Per-person SLA and speed over the report period.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PersonStats {
    /// Share of their completed tickets with no SLA ever breached (the
    /// overlay's any-SLA rule); `None` with SLA off or nothing completed.
    pub sla: Option<i64>,
    /// Median time to resolution in milliseconds (see [`resolution_millis`]).
    pub median_ms: Option<u64>,
}

/// A finished count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub seq: u64,
    pub results: Vec<(String, Option<u64>)>,
    /// Parallel to `results`.
    pub stats: Vec<PersonStats>,
    pub total: u64,
    pub completed: Option<u64>,
    pub unassigned: Option<u64>,
}

/// At most this many of a person's newest resolved tickets feed the median.
pub const MEDIAN_SAMPLE: usize = 500;

/// One unit of work for the report pool.
enum Job {
    /// A count (`None`: nothing to ask).
    Count(Option<String>),
    /// Resolution times of the tickets matching this JQL.
    Durations(String),
}

/// What a job produced.
#[derive(Clone)]
enum Answer {
    Count(Option<u64>),
    Durations(Option<Vec<u64>>),
}

/// Time to resolution for one resolved issue: the last completed cycle of
/// the first configured SLA (`elapsedTime.millis`, which follows the SLA's
/// business-hours calendar) when there is one, else wall-clock
/// `resolutiondate − created`.
pub fn resolution_millis(issue: &serde_json::Map<String, Value>, sla_field_id: Option<&str>) -> Option<u64> {
    let fields = issue.get("fields")?;
    let from_sla = sla_field_id
        .and_then(|id| fields.get(id)?.get("completedCycles")?.as_array()?.last().cloned())
        .and_then(|cycle| cycle.get("elapsedTime")?.get("millis")?.as_u64());
    if from_sla.is_some() {
        return from_sla;
    }
    let at = |k: &str| fields.get(k).and_then(Value::as_str).and_then(crate::preview::parse_created);
    let (created, resolved) = (at("created")?, at("resolutiondate")?);
    u64::try_from((resolved - created).num_milliseconds()).ok()
}

/// The median of a sample (mean of the middle two for an even count).
pub fn median(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        values[mid - 1] / 2 + values[mid] / 2 + (values[mid - 1] % 2 + values[mid] % 2) / 2
    })
}

/// The id (`customfield_10030`) of the field named `name`, from Jira's
/// field list (search takes ids, not names).
pub fn field_id(jira: &jo_jira::Jira, name: &str) -> Option<String> {
    let r = jira.send(Method::Get, "/rest/api/3/field", &[], None, Duration::from_secs(15), Retry::Once).ok()?;
    if !r.ok() {
        return None;
    }
    let fields = r.json()?;
    fields.as_array()?.iter().find_map(|f| {
        let matches = f.get("name")?.as_str()?.trim().eq_ignore_ascii_case(name.trim());
        matches.then(|| f.get("id")?.as_str().map(str::to_owned)).flatten()
    })
}

/// Count every person (completed, and breached when SLA is on), collect
/// their resolution times, plus the two coverage totals, on a pool of four
/// workers, each with its own client. `still_wanted` lets a superseded or
/// closed report stop early; it then returns `None` and posts nothing.
pub fn count_all(
    seq: u64,
    cfg: &Config,
    people: &[Person],
    (basis, start, end): (&str, NaiveDate, NaiveDate),
    make: &MakeTransport,
    still_wanted: &(dyn Fn() -> bool + Sync),
) -> Option<Outcome> {
    let base = base_jql(config::get_str(cfg, "projectKey", ""), basis, start, end);
    let sla_fields = if stats::sla_wanted(cfg) { stats::sla_fields(cfg) } else { Vec::new() };
    let breach = (!sla_fields.is_empty()).then(|| stats::breach_clause(&sla_fields));
    // Jobs: three per person (completed, breached, durations), then the
    // two totals. A person with no name or id costs nothing.
    let mut jobs: Vec<Job> = Vec::new();
    for p in people {
        let who = if p.account_id.is_empty() { &p.name } else { &p.account_id };
        let mine = (!who.is_empty()).then(|| format!("{base} AND assignee = {}", jql::quote(who)));
        jobs.push(Job::Count(mine.clone()));
        jobs.push(Job::Count(mine.as_ref().zip(breach.as_ref()).map(|(q, b)| format!("{q} AND {b}"))));
        jobs.push(match mine {
            Some(q) => Job::Durations(format!("{q} ORDER BY resolved DESC")),
            None => Job::Count(None),
        });
    }
    jobs.push(Job::Count(Some(base.clone())));
    jobs.push(Job::Count(Some(format!("{base} AND assignee IS EMPTY"))));

    let sla_id: OnceLock<Option<String>> = OnceLock::new();
    let workers = 4.min((people.len() + 2).max(1));
    let next = AtomicUsize::new(0);
    let answers: Mutex<Vec<Answer>> = Mutex::new(vec![Answer::Count(None); jobs.len()]);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let (next, answers, jobs, sla_id, sla_fields) = (&next, &answers, &jobs, &sla_id, &sla_fields);
            scope.spawn(move || {
                let jira = poll::client(cfg, make);
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(job) = jobs.get(i) else { break };
                    if !still_wanted() {
                        break;
                    }
                    let answer = match job {
                        Job::Count(q) => Answer::Count(q.as_deref().and_then(|q| jira.count(q).0)),
                        Job::Durations(q) => {
                            let id = sla_id.get_or_init(|| sla_fields.first().and_then(|n| field_id(&jira, n)));
                            let mut wanted = vec!["created", "resolutiondate"];
                            if let Some(id) = id {
                                wanted.push(id);
                            }
                            let issues = jira.search(q, &wanted, Some(MEDIAN_SAMPLE));
                            Answer::Durations(Some(
                                issues.iter().filter_map(|i| resolution_millis(i, id.as_deref())).collect(),
                            ))
                        }
                    };
                    answers.lock().unwrap_or_else(|p| p.into_inner())[i] = answer;
                }
            });
        }
    });
    if !still_wanted() {
        return None;
    }
    let answers = answers.into_inner().unwrap_or_else(|p| p.into_inner());
    let count_at = |i: usize| match &answers[i] {
        Answer::Count(c) => *c,
        Answer::Durations(_) => None,
    };
    let mut results = Vec::with_capacity(people.len());
    let mut person_stats = Vec::with_capacity(people.len());
    for (n, p) in people.iter().enumerate() {
        let completed = count_at(3 * n);
        let breached = count_at(3 * n + 1);
        let sla = match (completed, breached) {
            (Some(total), Some(b)) if total > 0 => {
                Some(crate::util::py_round((total - b.min(total)) as f64 / total as f64 * 100.0))
            }
            _ => None,
        };
        let median_ms = match &answers[3 * n + 2] {
            Answer::Durations(Some(d)) => median(&mut d.clone()),
            _ => None,
        };
        results.push((p.name.clone(), completed));
        person_stats.push(PersonStats { sla, median_ms });
    }
    let total = results.iter().filter_map(|(_, c)| *c).sum();
    let last = 3 * people.len();
    Some(Outcome {
        seq,
        results,
        stats: person_stats,
        total,
        completed: count_at(last),
        unassigned: count_at(last + 1),
    })
}

/// `"35m"`, `"4.2h"`, `"3.1d"`.
pub fn duration_text(ms: u64) -> String {
    let hours = ms as f64 / 3_600_000.0;
    if hours < 1.0 {
        format!("{}m", (ms / 60_000).max(1))
    } else if hours < 48.0 {
        format!("{hours:.1}h")
    } else {
        format!("{:.1}d", hours / 24.0)
    }
}

/// The status line after a count: `(text, warn)`.
pub fn coverage_status(outcome: &Outcome) -> (String, bool) {
    let failed: Vec<&str> = outcome.results.iter().filter(|(_, c)| c.is_none()).map(|(n, _)| n.as_str()).collect();
    if !failed.is_empty() {
        return (format!("Could not count: {}", failed.join(", ")), true);
    }
    match outcome.completed {
        Some(completed) if completed > 0 => {
            // Separate queries moments apart can make the parts exceed the
            // whole: never claim more than 100%.
            let share = (100 * outcome.total / completed).min(100);
            let mut text = format!("{} of {completed} completed ({share}%)", outcome.total);
            if let Some(unassigned) = outcome.unassigned.filter(|u| *u > 0) {
                text.push_str(&format!(" · {unassigned} had no assignee"));
            }
            (text, share < 50)
        }
        _ => (format!("{} completed in total", outcome.total), false),
    }
}

/// One rendered ranking row.
#[derive(Clone, Debug, PartialEq)]
pub struct RankRow {
    pub place: usize,
    pub name: String,
    /// `"—"` when the count failed.
    pub count: String,
    /// `"42%"` of the total, or empty when the total is 0.
    pub share: String,
    /// Bar length as a fraction of the best count.
    pub bar: f32,
    /// `"SLA 94%"`, or empty.
    pub sla: String,
    /// `"4.2h"` median time to resolution, or empty.
    pub median: String,
}

/// Ranked rows: highest first, failures last. `stats` is parallel to
/// `results` (missing entries show blank).
pub fn ranking(results: &[(String, Option<u64>)], stats: &[PersonStats], total: u64) -> Vec<RankRow> {
    let blank = PersonStats::default();
    let mut ranked: Vec<(&(String, Option<u64>), &PersonStats)> =
        results.iter().enumerate().map(|(i, r)| (r, stats.get(i).unwrap_or(&blank))).collect();
    ranked.sort_by_key(|((_, c), _)| (c.is_none(), std::cmp::Reverse(c.unwrap_or(0))));
    let best = ranked.iter().map(|((_, c), _)| c.unwrap_or(0)).max().filter(|b| *b > 0).unwrap_or(1);
    ranked
        .into_iter()
        .enumerate()
        .map(|(i, ((name, count), st))| RankRow {
            place: i + 1,
            name: name.clone(),
            count: count.map_or_else(|| "—".to_owned(), |c| c.to_string()),
            share: (100 * count.unwrap_or(0)).checked_div(total).map_or_else(String::new, |p| format!("{p}%")),
            bar: count.unwrap_or(0) as f32 / best as f32,
            sla: st.sla.map_or_else(String::new, |p| format!("SLA {p}%")),
            median: st.median_ms.map_or_else(String::new, duration_text),
        })
        .collect()
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// CSV text as Python's `csv.writer` writes it. `stats` is parallel to
/// `results`; the median is in hours.
pub fn csv_text(
    results: &[(String, Option<u64>)],
    stats: &[PersonStats],
    start: NaiveDate,
    end: NaiveDate,
    basis: &str,
) -> String {
    let blank = PersonStats::default();
    let mut rows: Vec<(&(String, Option<u64>), &PersonStats)> =
        results.iter().enumerate().map(|(i, r)| (r, stats.get(i).unwrap_or(&blank))).collect();
    rows.sort_by_key(|((_, c), _)| std::cmp::Reverse(c.unwrap_or(0)));
    let mut out = String::from("Person,Completed,SLA %,Median hours to resolution,From,To,Counted by\r\n");
    for ((name, count), st) in rows {
        let fields = [
            csv_safe(name),
            count.map(|c| c.to_string()).unwrap_or_default(),
            st.sla.map(|p| p.to_string()).unwrap_or_default(),
            st.median_ms.map(|ms| format!("{:.1}", ms as f64 / 3_600_000.0)).unwrap_or_default(),
            start.format("%Y-%m-%d").to_string(),
            end.format("%Y-%m-%d").to_string(),
            basis.to_owned(),
        ];
        out.push_str(&fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(","));
        out.push_str("\r\n");
    }
    out
}

/// `team-report-{start}-to-{end}.csv` next to the app.
pub fn export_path(root: &Path, start: NaiveDate, end: NaiveDate) -> PathBuf {
    root.join(format!("team-report-{}-to-{}.csv", start.format("%Y-%m-%d"), end.format("%Y-%m-%d")))
}

/// Combine the people already chosen with the ones Jira knows about: match
/// on accountId when there is one, otherwise on name. Returns the list to
/// show (sorted by name) and who is already ticked.
pub fn merge(current: &[Person], discovered: Vec<Person>) -> (Vec<Person>, Vec<bool>) {
    let chosen = chosen_predicate(current);
    let mut people = discovered;
    let known_ids: HashSet<String> =
        people.iter().filter(|p| !p.account_id.is_empty()).map(|p| p.account_id.clone()).collect();
    let known_names: HashSet<String> = people.iter().map(|p| p.name.trim().to_lowercase()).collect();
    // Keep anyone chosen whom the lookup didn't return (left, or typed by hand).
    for person in current {
        if !person.account_id.is_empty() && known_ids.contains(&person.account_id) {
            continue;
        }
        if known_names.contains(&person.name.trim().to_lowercase()) {
            continue;
        }
        people.push(person.clone());
    }
    people.sort_by_key(|p| p.name.to_lowercase());
    let ticked = people.iter().map(&chosen).collect();
    (people, ticked)
}

/// Whether a person is already chosen, before any Jira lookup came back.
pub fn chosen_predicate(current: &[Person]) -> impl Fn(&Person) -> bool + use<> {
    let ids: HashSet<String> =
        current.iter().filter(|p| !p.account_id.is_empty()).map(|p| p.account_id.clone()).collect();
    let names: HashSet<String> =
        current.iter().filter(|p| !p.name.is_empty()).map(|p| p.name.trim().to_lowercase()).collect();
    move |p: &Person| {
        (!p.account_id.is_empty() && ids.contains(&p.account_id)) || names.contains(&p.name.trim().to_lowercase())
    }
}

/// Users Jira says can be assigned work in the project, paginated (200 a
/// page, at most 1000), on the picker's own client. Partial results beat none.
pub fn discover_people(cfg: &Config, make: &MakeTransport) -> Vec<Person> {
    let jira = poll::client(cfg, make);
    let pk = config::get_str(cfg, "projectKey", "").to_owned();
    let mut out = Vec::new();
    let mut start = 0_usize;
    loop {
        let query = [("project", pk.clone()), ("maxResults", "200".into()), ("startAt", start.to_string())];
        let Ok(r) = jira.send(
            Method::Get,
            "/rest/api/3/user/assignable/search",
            &query,
            None,
            Duration::from_secs(15),
            Retry::Once,
        ) else {
            break;
        };
        if !r.ok() {
            break;
        }
        let Some(Value::Array(page)) = r.json() else { break };
        for user in &page {
            let name = user.get("displayName").and_then(Value::as_str).unwrap_or_default();
            if !name.is_empty() {
                out.push(Person::new(name, user.get("accountId").and_then(Value::as_str).unwrap_or_default()));
            }
        }
        if page.len() < 200 || out.len() >= 1000 {
            break;
        }
        start += page.len();
    }
    out
}

/// Re-list people after discovery, keeping ticks made in the meantime.
pub fn repopulate(
    previous: &[(Person, bool)],
    people: Vec<Person>,
    already_chosen: impl Fn(&Person) -> bool,
) -> Vec<(Person, bool)> {
    let prev: std::collections::HashMap<String, bool> = previous.iter().map(|(p, t)| (p.key(), *t)).collect();
    people
        .into_iter()
        .map(|p| {
            let ticked = prev.get(&p.key()).copied().unwrap_or_else(|| already_chosen(&p));
            (p, ticked)
        })
        .collect()
}

/// Shared cancellation for one report window.
#[derive(Default)]
pub struct ReportGate {
    pub seq: std::sync::atomic::AtomicU64,
    pub closed: std::sync::atomic::AtomicBool,
}

impl ReportGate {
    pub fn wanted(self: &Arc<Self>, seq: u64) -> bool {
        !self.closed.load(Ordering::SeqCst) && self.seq.load(Ordering::SeqCst) == seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeJira, ok_json};
    use std::sync::Barrier;

    fn cfg(v: Value) -> Config {
        v.as_object().unwrap().clone()
    }

    fn names(people: &[Person]) -> Vec<&str> {
        people.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn report_users_shapes() {
        assert_eq!(
            report_users(&cfg(json!({"reportUsers": [{"name": "Sam", "accountId": "a1"}]}))),
            [Person::new("Sam", "a1")]
        );
        assert_eq!(
            report_users(&cfg(json!({"reportUsers": ["Sam", "Ada"]}))),
            [Person::new("Sam", ""), Person::new("Ada", "")]
        );
        let seeded = report_users(&cfg(json!({"completedTodayFilter": "assignee in (\"Sam Carter\", \"Ada Nkemi\")"})));
        assert_eq!(names(&seeded), ["Sam Carter", "Ada Nkemi"]);
        assert!(report_users(&cfg(json!({}))).is_empty());
        for (label, users, expected) in [
            ("the same name twice", json!([{"name": "Sam", "accountId": ""}, {"name": "Sam", "accountId": ""}]), 1),
            (
                "one accountId under two names",
                json!([{"name": "Sam", "accountId": "a1"}, {"name": "Sam Carter", "accountId": "a1"}]),
                1,
            ),
            (
                "with and without an accountId",
                json!([{"name": "Sam", "accountId": ""}, {"name": "Sam", "accountId": "a1"}]),
                1,
            ),
            (
                "different case",
                json!([{"name": "sam carter", "accountId": ""}, {"name": "Sam Carter", "accountId": ""}]),
                1,
            ),
            (
                "two different people",
                json!([{"name": "Sam", "accountId": "a1"}, {"name": "Ada", "accountId": "a2"}]),
                2,
            ),
        ] {
            assert_eq!(report_users(&cfg(json!({"reportUsers": users}))).len(), expected, "{label}");
        }
        let repeated = report_users(&cfg(json!({"completedTodayFilter": "assignee in (\"Sam\",\"Ada\",\"Sam\")"})));
        assert_eq!(names(&repeated), ["Sam", "Ada"]);
    }

    #[test]
    fn periods() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 27).unwrap();
        for (label, days) in REPORT_RANGES {
            let Some(days) = days else { continue };
            let (start, end) = period(label, "", "", today).unwrap();
            assert!(end == today && (end - start).num_days() == days as i64, "{label}");
        }
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        assert_eq!(period("Custom", "2026-01-10", "2026-01-20", today), Some((d(1, 10), d(1, 20))));
        assert_eq!(period("Custom", "2026-02-01", "2026-01-01", today), Some((d(1, 1), d(2, 1))), "swapped");
        assert_eq!(period("Custom", "not a date", "2026-01-01", today), None);
        assert_eq!(period("Custom", "2026-1-5", "2026-01-01", today), None);
        assert_eq!(initial_range(&cfg(json!({"reportRange": "Fortnight"}))), "30 days", "deliberate change 6");
        assert_eq!(initial_range(&cfg(json!({"reportRange": "7 days"}))), "7 days");
        assert_eq!(initial_range(&cfg(json!({}))), "30 days");
    }

    fn counting(handler: impl Fn(&str) -> Option<u64> + Send + Sync + 'static) -> (FakeJira, MakeTransport) {
        let fake = FakeJira::new(move |req| {
            let jql = req.body.as_ref().and_then(|b| b["jql"].as_str()).unwrap_or_default().to_owned();
            match handler(&jql) {
                Some(n) => ok_json(json!({"count": n})),
                None => crate::testing::status(500),
            }
        });
        let make = fake.make();
        (fake, make)
    }

    fn jqls(fake: &FakeJira) -> Vec<String> {
        fake.requests().iter().filter_map(|r| r.body.as_ref()?["jql"].as_str().map(str::to_owned)).collect()
    }

    #[test]
    fn the_jql_actually_issued() {
        let (fake, make) = counting(|_| Some(7));
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let people = [Person::new("Sam Carter", "acc-1"), Person::new("Ada", "")];
        let c = cfg(json!({"projectKey": "SD"}));
        let out = count_all(1, &c, &people, ("resolved", d(1, 1), d(1, 31)), &make, &|| true).unwrap();
        let seen = jqls(&fake);
        let q = seen.iter().find(|s| s.contains("assignee = \"acc-1\"")).unwrap();
        assert!(q.contains("statusCategory = Done") && q.contains("project = SD"));
        assert!(q.contains("resolved >= \"2026-01-01\"") && q.contains("resolved < \"2026-02-01\""));
        assert!(seen.iter().any(|s| s.contains("assignee = \"Ada\"")), "falls back to display name");
        assert_eq!(out.total, 14);

        let (fake, make) = counting(|_| Some(1));
        count_all(2, &c, &people, ("updated", d(1, 1), d(1, 2)), &make, &|| true);
        assert!(jqls(&fake).iter().all(|s| s.contains("updated >= \"2026-01-01\"")));

        let (fake, make) = counting(|_| Some(1));
        count_all(3, &c, &people, ("resolved\" OR project = \"SECRET", d(1, 1), d(1, 2)), &make, &|| true);
        assert!(jqls(&fake).iter().all(|s| !s.contains("SECRET")), "unknown basis falls back to resolved");

        let (fake, make) = counting(|_| Some(1));
        count_all(4, &c, &[Person::new("O\"Brien", "")], ("resolved", d(1, 1), d(1, 2)), &make, &|| true);
        assert!(jqls(&fake).iter().any(|s| s.contains("\\\"Brien")), "the name is escaped");

        let (fake, make) = counting(|_| Some(1));
        let out = count_all(5, &c, &[Person::new("", "")], ("resolved", d(1, 1), d(1, 2)), &make, &|| true).unwrap();
        assert!(!jqls(&fake).iter().any(|s| s.contains("assignee =")), "no name costs no query");
        assert_eq!(out.results, [(String::new(), None)]);
    }

    #[test]
    fn counts_run_concurrently_on_their_own_clients() {
        let barrier = Arc::new(Barrier::new(2));
        let b = barrier.clone();
        let (fake, make) = counting(move |jql| {
            // Only the counts rendezvous (an even number of them).
            if !jql.is_empty() && !jql.contains("ORDER BY") {
                b.wait();
            }
            Some(if jql.contains("assignee =") { 7 } else { 3 })
        });
        let people = [Person::new("A", "a"), Person::new("B", "b")];
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let c = cfg(json!({"projectKey": "SD", "email": "e@x", "token": "t", "domain": "x.atlassian.net"}));
        let out = count_all(1, &c, &people, ("resolved", d(8, 1), d(8, 26)), &make, &|| true).unwrap();
        assert_eq!(out.results, [("A".into(), Some(7)), ("B".into(), Some(7))]);
        assert!(out.total == 14 && out.completed == Some(3) && out.unassigned == Some(3));
        assert_eq!(fake.clients(), 4, "every worker brought its own client");
    }

    #[test]
    fn superseded_reports_post_nothing() {
        let gate = Arc::new(ReportGate::default());
        gate.seq.store(2, Ordering::SeqCst);
        let (_fake, make) = counting(|_| Some(1));
        let d = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let g = gate.clone();
        let out =
            count_all(1, &cfg(json!({})), &[Person::new("Sam", "")], ("resolved", d, d), &make, &move || g.wanted(1));
        assert!(out.is_none());
        gate.closed.store(true, Ordering::SeqCst);
        assert!(!gate.wanted(2), "a closed window wants nothing");
    }

    fn outcome(completed: Option<u64>, unassigned: Option<u64>) -> Outcome {
        Outcome {
            seq: 1,
            results: vec![("Alfie".into(), Some(291)), ("Luke".into(), Some(235))],
            stats: vec![],
            total: 526,
            completed,
            unassigned,
        }
    }

    #[test]
    fn coverage_text() {
        let (text, warn) = coverage_status(&outcome(Some(1028), Some(436)));
        assert!(text.contains("526 of 1028") && text.contains("51%") && text.contains("436") && !warn);
        let (text, _) = coverage_status(&outcome(Some(600), Some(0)));
        assert!(!text.contains("assignee"));
        assert!(coverage_status(&outcome(Some(2000), Some(1900))).1);
        assert!(coverage_status(&outcome(None, None)).0.contains("completed in total"));
        assert!(coverage_status(&outcome(Some(300), Some(100))).0.contains("(100%)"));
        let mut failed = outcome(Some(10), None);
        failed.results[1].1 = None;
        assert_eq!(coverage_status(&failed), ("Could not count: Luke".into(), true));
    }

    #[test]
    fn ranking_rows() {
        let stats = [
            PersonStats { sla: Some(90), median_ms: Some(2 * 3_600_000) },
            PersonStats::default(),
            PersonStats { sla: Some(100), median_ms: Some(20 * 60_000) },
        ];
        let rows = ranking(&[("A".into(), Some(2)), ("B".into(), None), ("C".into(), Some(6))], &stats, 8);
        assert_eq!(names_of(&rows), ["C", "A", "B"]);
        assert_eq!(rows[0].share, "75%");
        assert_eq!(rows[2].count, "—");
        assert!((rows[1].bar - 1.0 / 3.0).abs() < 1e-6);
        assert_eq!((rows[0].sla.as_str(), rows[0].median.as_str()), ("SLA 100%", "20m"), "stats follow the person");
        assert_eq!((rows[1].sla.as_str(), rows[1].median.as_str()), ("SLA 90%", "2.0h"));
        assert_eq!((rows[2].sla.as_str(), rows[2].median.as_str()), ("", ""));
        assert_eq!(ranking(&[("Z".into(), Some(0))], &[], 0)[0].share, "");
        assert_eq!(duration_text(30_000), "1m");
        assert_eq!(duration_text(3 * 86_400_000), "3.0d");
    }

    fn names_of(rows: &[RankRow]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn csv_export_is_safe() {
        for name in ["=1+1", "+SUM(A1:A9)", "-2+3", "@SUM(1,1)", "\tcmd", "-"] {
            let safe = csv_safe(name);
            assert!(safe.starts_with('\'') && &safe[1..] == name, "{name:?}");
        }
        for name in ["Sam Carter", "O'Brien", ""] {
            assert_eq!(csv_safe(name), name);
        }
        let d = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let stats = [
            PersonStats { sla: Some(50), median_ms: Some(5_400_000) },
            PersonStats { sla: Some(100), median_ms: None },
            PersonStats::default(),
        ];
        let text = csv_text(
            &[("=cmd".into(), Some(1)), ("Lee, Ann".into(), Some(3)), ("Gone".into(), None)],
            &stats,
            d,
            d,
            "resolved",
        );
        assert_eq!(
            text,
            "Person,Completed,SLA %,Median hours to resolution,From,To,Counted by\r\n\"Lee, Ann\",3,100,,2026-01-01,2026-01-01,resolved\r\n'=cmd,1,50,1.5,2026-01-01,2026-01-01,resolved\r\nGone,,,,2026-01-01,2026-01-01,resolved\r\n"
        );
    }

    #[test]
    fn resolution_times_and_median() {
        let issue = |v: Value| v.as_object().unwrap().clone();
        let sla = issue(json!({"key": "SD-1", "fields": {
            "created": "2026-11-02T09:00:00.000+0000", "resolutiondate": "2026-11-04T09:00:00.000+0000",
            "customfield_10030": {"completedCycles": [
                {"elapsedTime": {"millis": 1000}}, {"elapsedTime": {"millis": 7_200_000}}]}}}));
        assert_eq!(resolution_millis(&sla, Some("customfield_10030")), Some(7_200_000), "the SLA's last cycle");
        assert_eq!(resolution_millis(&sla, None), Some(2 * 86_400_000), "without the SLA: wall clock");
        let open_cycle = issue(json!({"fields": {"created": "2026-11-02T09:00:00.000+0000",
            "resolutiondate": "2026-11-02T10:30:00.000+0000", "customfield_10030": {"completedCycles": []}}}));
        assert_eq!(resolution_millis(&open_cycle, Some("customfield_10030")), Some(5_400_000));
        let unresolved = issue(json!({"fields": {"created": "2026-11-02T09:00:00.000+0000", "resolutiondate": null}}));
        assert_eq!(resolution_millis(&unresolved, None), None);
        let backwards =
            issue(json!({"fields": {"created": "2026-11-03T09:00:00Z", "resolutiondate": "2026-11-02T09:00:00Z"}}));
        assert_eq!(resolution_millis(&backwards, None), None);
        assert_eq!(median(&mut [5, 1, 3]), Some(3));
        assert_eq!(median(&mut [4, 1, 3, 2]), Some(2), "mean of the middle two, rounded down");
        assert_eq!(median(&mut [u64::MAX, u64::MAX]), Some(u64::MAX), "no overflow");
        assert_eq!(median(&mut []), None);
    }

    #[test]
    fn per_person_sla_and_median_queries() {
        let fake = FakeJira::new(|req| {
            if req.url.ends_with("/rest/api/3/field") {
                return ok_json(json!([{"id": "summary", "name": "Summary"},
                                      {"id": "customfield_10030", "name": "Time to resolution"}]));
            }
            let body = req.body.clone().unwrap_or_default();
            let jql = body["jql"].as_str().unwrap_or_default().to_owned();
            if req.url.ends_with("/search/jql") {
                assert!(jql.ends_with("ORDER BY resolved DESC"), "{jql}");
                let fields: Vec<&str> =
                    body["fields"].as_array().unwrap().iter().map(|f| f.as_str().unwrap()).collect();
                assert_eq!(fields, ["created", "resolutiondate", "customfield_10030"], "only the needed fields");
                let mins = if jql.contains("acc-1") { [10, 20, 90] } else { [60, 60, 60] };
                let issues: Vec<Value> = mins
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        json!({"key": format!("SD-{i}"),
                    "fields": {"customfield_10030": {"completedCycles": [{"elapsedTime": {"millis": m * 60_000}}]}}})
                    })
                    .collect();
                return ok_json(json!({"issues": issues, "isLast": true}));
            }
            let n = if jql.contains("everBreached") {
                1
            } else if jql.contains("assignee =") {
                4
            } else {
                10
            };
            ok_json(json!({"count": n}))
        });
        let d = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let people = [Person::new("Sam", "acc-1"), Person::new("Ada", "acc-2"), Person::new("", "")];
        let c = cfg(json!({"projectKey": "SD", "domain": "x.atlassian.net"}));
        let out = count_all(1, &c, &people, ("resolved", d, d), &fake.make(), &|| true).unwrap();
        assert_eq!(out.stats[0], PersonStats { sla: Some(75), median_ms: Some(20 * 60_000) });
        assert_eq!(out.stats[1], PersonStats { sla: Some(75), median_ms: Some(60 * 60_000) });
        assert_eq!(out.stats[2], PersonStats::default(), "no name, no queries");
        assert_eq!((out.total, out.completed, out.unassigned), (8, Some(10), Some(10)));
        let breach = fake
            .requests()
            .iter()
            .filter_map(|r| r.body.as_ref()?["jql"].as_str().map(str::to_owned))
            .find(|j| j.contains("everBreached") && j.contains("acc-1"))
            .unwrap();
        assert!(breach.ends_with(r#"AND assignee = "acc-1" AND ("Time to resolution" = everBreached())"#), "{breach}");
        let lookups = fake.requests().iter().filter(|r| r.url.ends_with("/field")).count();
        assert_eq!(lookups, 1, "the SLA field id is looked up once per report");
        assert!(fake.clients() <= 4, "still the four-worker pool");

        // SLA switched off: no breach counts, wall-clock medians, no field lookup.
        let fake = FakeJira::new(|req| {
            if req.url.ends_with("/search/jql") {
                return ok_json(json!({"issues": [{"key": "SD-1", "fields": {
                    "created": "2026-11-02T09:00:00.000+0000", "resolutiondate": "2026-11-02T13:00:00.000+0000"}}]}));
            }
            ok_json(json!({"count": 2}))
        });
        let c = cfg(json!({"projectKey": "SD", "slaEnabled": false}));
        let out = count_all(2, &c, &people[..1], ("resolved", d, d), &fake.make(), &|| true).unwrap();
        assert_eq!(out.stats[0], PersonStats { sla: None, median_ms: Some(4 * 3_600_000) });
        assert!(
            !fake.requests().iter().any(|r| r.url.ends_with("/field")
                || r.body.as_ref().is_some_and(|b| b.to_string().contains("everBreached")))
        );
    }

    #[test]
    fn people_picker_merge() {
        let jira_users = vec![
            Person::new("Ada Nkemi", "acc-ada"),
            Person::new("Bart Bielski", "acc-bart"),
            Person::new("Jo Bright", "acc-jo"),
            Person::new("Sam Carter", "acc-sam"),
        ];
        let mut current: Vec<Person> =
            ["Ada Nkemi", "Jo Bright", "Sam Carter"].iter().map(|n| Person::new(n, "")).collect();
        current.push(Person::new("Ex Employee", ""));
        let (listed, ticked) = merge(&current, jira_users);
        let names: Vec<&str> = listed.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names.len(), names.iter().collect::<HashSet<_>>().len(), "nobody is listed twice");
        let mut chosen: Vec<&str> =
            listed.iter().zip(&ticked).filter(|(_, t)| **t).map(|(p, _)| p.name.as_str()).collect();
        chosen.sort();
        assert_eq!(chosen, ["Ada Nkemi", "Ex Employee", "Jo Bright", "Sam Carter"]);
        assert!(!chosen_predicate(&current)(&Person::new("Bart Bielski", "acc-bart")));
        assert!(names.contains(&"Ex Employee"));
        assert_eq!(listed.iter().find(|p| p.name == "Ada Nkemi").unwrap().account_id, "acc-ada");

        let prev = vec![(Person::new("Bart Bielski", "acc-bart"), true)];
        let again = repopulate(&prev, listed, chosen_predicate(&current));
        assert!(again.iter().find(|(p, _)| p.name == "Bart Bielski").unwrap().1, "ticks survive a late repopulate");
    }

    #[test]
    fn discovery_pages_until_a_short_page() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let log = calls.clone();
        let fake = FakeJira::new(move |req| {
            let start: usize = req.query.iter().find(|(k, _)| k == "startAt").unwrap().1.parse().unwrap();
            log.lock().unwrap().push(start);
            if start == 0 {
                ok_json(Value::Array(
                    (0..200)
                        .map(|i| json!({"displayName": format!("User {i}"), "accountId": format!("a{i}")}))
                        .collect(),
                ))
            } else {
                ok_json(
                    json!([{"displayName": "Latecomer", "accountId": "late"}, {"displayName": "", "accountId": "x"}]),
                )
            }
        });
        let people = discover_people(&cfg(json!({"projectKey": "SD", "domain": "x.atlassian.net"})), &fake.make());
        assert_eq!(people.len(), 201);
        assert_eq!(*calls.lock().unwrap(), [0, 200], "successive startAt offsets");
        assert_eq!(fake.clients(), 1, "its own client, never the poller's");
    }
}
