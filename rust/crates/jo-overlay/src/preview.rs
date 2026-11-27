//! Hover preview of a queue's newest tickets: fetched on demand from the
//! queue-issue endpoint on its own client and thread (never the 5 s poll),
//! cached per queue for a minute. Alert queues reuse the pulled tickets.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use jo_core::jql;
use jo_jira::{Jira, Method, Retry};
use serde_json::{Map, Value};

use crate::alerts;
use crate::model::Issue;
use crate::util;

/// Tickets shown per queue.
pub const LIMIT: usize = 8;
/// How long one queue's fetched tickets are reused.
pub const CACHE_FOR: Duration = Duration::from_secs(60);
/// One page is enough to find the newest few.
const PAGE: usize = 50;

/// One preview row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub key: String,
    pub summary: String,
    pub age: Option<String>,
    pub assignee: Option<String>,
    /// `(hint, review)`, e.g. `("→ With DEV · 92%", false)`.
    pub suggestion: Option<(String, bool)>,
}

/// Jira's timestamp (`2026-11-27T09:15:00.000+0000`) or RFC 3339.
pub fn parse_created(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    DateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f%z")
        .or_else(|_| DateTime::parse_from_rfc3339(text))
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// `"<1m"`, `"5m"`, `"3h"`, `"2d"` since `created`.
pub fn age_text(created: &str, now: DateTime<Utc>) -> Option<String> {
    let secs = (now - parse_created(created)?).num_seconds().max(0);
    Some(match secs {
        0..60 => "<1m".to_owned(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    })
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() > max { format!("{}…", util::take_chars(text, max)) } else { text.to_owned() }
}

/// The newest `LIMIT` tickets (by `created`; undated ones keep queue order
/// after them), ready to draw.
pub fn tickets(issues: &[Issue], suggestions: &Map<String, Value>, now: DateTime<Utc>) -> Vec<Ticket> {
    let mut seen = HashSet::new();
    let mut ordered: Vec<&Issue> = issues.iter().filter(|i| !i.key.is_empty() && seen.insert(&i.key)).collect();
    ordered.sort_by_key(|i| std::cmp::Reverse(i.created.as_deref().and_then(parse_created)));
    ordered
        .into_iter()
        .take(LIMIT)
        .map(|i| Ticket {
            key: i.key.clone(),
            summary: short(i.summary.as_deref().unwrap_or("(no summary)"), 48),
            age: i.created.as_deref().and_then(|c| age_text(c, now)),
            assignee: i.assignee.as_deref().map(|a| short(a, 24)),
            suggestion: alerts::suggestion(suggestions, &i.key).map(|(l, r)| (alerts::suggestion_hint(&l, r), r)),
        })
        .collect()
}

/// One queue-issue page as tickets; `None` when it isn't a page at all.
pub fn parse_page(body: &Value, queue: &str) -> Option<Vec<Issue>> {
    match body.get("values") {
        Some(Value::Array(items)) => Some(items.iter().map(|v| Issue::from_json(v, queue)).collect()),
        Some(Value::Null) | None => body.is_object().then(Vec::new),
        Some(_) => None,
    }
}

/// GET one page of the queue (read-only, no retry storm).
pub fn fetch(jira: &Jira, desk_id: &str, queue_id: &str, queue: &str) -> Option<Vec<Issue>> {
    let path = format!(
        "/rest/servicedeskapi/servicedesk/{}/queue/{}/issue",
        jql::path_segment(desk_id),
        jql::path_segment(queue_id)
    );
    let query = [("start", "0".to_owned()), ("limit", PAGE.to_string())];
    let r = jira.send(Method::Get, &path, &query, None, Duration::from_secs(10), Retry::Once).ok()?;
    if !r.ok() {
        return None;
    }
    parse_page(&r.json()?, queue)
}

/// Per-queue cache of fetched tickets (`None`: the fetch failed).
#[derive(Default)]
pub struct Cache {
    entries: HashMap<String, (Instant, Option<Vec<Issue>>)>,
    in_flight: HashSet<String>,
}

impl Cache {
    /// True (and marks it in flight) when `queue_id` needs a fetch now.
    pub fn begin(&mut self, queue_id: &str, now: Instant) -> bool {
        let fresh = self.entries.get(queue_id).is_some_and(|(at, _)| now.saturating_duration_since(*at) < CACHE_FOR);
        if fresh || self.in_flight.contains(queue_id) {
            return false;
        }
        self.in_flight.insert(queue_id.to_owned());
        true
    }

    pub fn finish(&mut self, queue_id: &str, issues: Option<Vec<Issue>>, now: Instant) {
        self.in_flight.remove(queue_id);
        self.entries.insert(queue_id.to_owned(), (now, issues));
    }

    /// `None` while loading; `Some(None)` after a failed fetch.
    pub fn get(&self, queue_id: &str) -> Option<Option<&[Issue]>> {
        self.entries.get(queue_id).map(|(_, issues)| issues.as_deref())
    }

    pub fn loading(&self, queue_id: &str) -> bool {
        self.in_flight.contains(queue_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        parse_created("2026-11-27T12:00:00.000+0000").unwrap()
    }

    #[test]
    fn ages() {
        assert_eq!(age_text("2026-11-27T11:59:30.000+0000", now()).as_deref(), Some("<1m"));
        assert_eq!(age_text("2026-11-27T11:15:00.000+0000", now()).as_deref(), Some("45m"));
        assert_eq!(age_text("2026-11-27T13:00:00.000+0200", now()).as_deref(), Some("1h"), "offsets respected");
        assert_eq!(age_text("2026-11-24T12:00:00Z", now()).as_deref(), Some("3d"));
        assert_eq!(age_text("2026-11-28T12:00:00Z", now()).as_deref(), Some("<1m"), "clock skew never goes negative");
        assert_eq!(age_text("yesterday", now()), None);
    }

    #[test]
    fn newest_eight_with_details() {
        let page = json!({"values": (0..12).map(|i| json!({
            "key": format!("SD-{i}"),
            "fields": {"summary": format!("Ticket {i} {}", "x".repeat(i * 6)),
                       "created": format!("2026-11-27T{:02}:00:00.000+0000", i),
                       "assignee": if i % 2 == 0 { json!({"displayName": "Sam Carter"}) } else { Value::Null }}
        })).chain([json!({"key": "SD-99", "fields": {"summary": "undated"}}), json!({"key": "SD-3"})]).collect::<Vec<_>>()});
        let issues = parse_page(&page, "Open").unwrap();
        assert_eq!(issues.len(), 14);
        let suggestions =
            json!({"SD-11": {"label": "With DEV · 92% — because: vpn", "review": false}}).as_object().unwrap().clone();
        let rows = tickets(&issues, &suggestions, now());
        assert_eq!(rows.len(), LIMIT);
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["SD-11", "SD-10", "SD-9", "SD-8", "SD-7", "SD-6", "SD-5", "SD-4"], "newest first");
        assert_eq!(rows[0].age.as_deref(), Some("1h"));
        assert_eq!(rows[0].assignee, None);
        assert_eq!(rows[1].assignee.as_deref(), Some("Sam Carter"));
        assert!(rows[0].summary.ends_with('…') && rows[0].summary.chars().count() == 49);
        assert_eq!(rows[0].suggestion, Some(("→ With DEV · 92%".into(), false)));
        assert!(parse_page(&json!({"values": "x"}), "Q").is_none());
        assert_eq!(parse_page(&json!({"values": null}), "Q"), Some(vec![]));
        assert!(parse_page(&json!([1]), "Q").is_none());
    }

    #[test]
    fn cache_reuses_for_a_minute() {
        let mut cache = Cache::default();
        let t0 = Instant::now();
        assert!(cache.begin("2", t0));
        assert!(!cache.begin("2", t0), "already in flight");
        assert!(cache.loading("2") && cache.get("2").is_none());
        cache.finish("2", Some(vec![Issue::new("SD-1", None, "Q")]), t0);
        assert_eq!(cache.get("2").unwrap().unwrap().len(), 1);
        assert!(!cache.begin("2", t0 + Duration::from_secs(59)), "fresh for 60 s");
        assert!(cache.begin("2", t0 + CACHE_FOR), "then fetched again");
        cache.finish("2", None, t0 + CACHE_FOR);
        assert_eq!(cache.get("2"), Some(None), "a failed fetch is remembered too");
    }
}
