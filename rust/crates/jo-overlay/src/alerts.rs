//! New-ticket detection, toast text and the alert popup's grouping.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::model::Issue;
use crate::util;

/// The seen-set with its two-poll grace (Python `_seen_newly_ids` and
/// `_missing_once`): a key is forgotten only after being absent from two
/// consecutive polls, so one empty reading can't re-notify everything.
#[derive(Clone, Debug, Default)]
pub struct SeenSet {
    pub seen: HashSet<String>,
    pub missing_once: HashSet<String>,
}

impl SeenSet {
    /// Feed the current tickets; returns the keys never notified before.
    pub fn update(&mut self, issues: &[Issue]) -> Vec<String> {
        let mut order = Vec::new();
        let mut current = HashSet::new();
        for issue in issues {
            // Blank keys are dropped: they would notify with no reference.
            if !issue.key.is_empty() && current.insert(issue.key.clone()) {
                order.push(issue.key.clone());
            }
        }
        let truly_new: Vec<String> = order.into_iter().filter(|k| !self.seen.contains(k)).collect();
        let vanished: HashSet<String> = self.seen.difference(&current).cloned().collect();
        let mut next = current;
        next.extend(vanished.difference(&self.missing_once).cloned());
        self.seen = next;
        self.missing_once = vanished;
        truly_new
    }
}

/// A learner suggestion for a ticket: `(label, review)` when it has a label.
pub fn suggestion(suggestions: &Map<String, Value>, key: &str) -> Option<(String, bool)> {
    let entry = suggestions.get(key)?.as_object()?;
    let label = entry.get("label").filter(|v| util::truthy(Some(v)))?;
    Some((util::py_str(label), util::truthy(entry.get("review"))))
}

/// Title/body for a new-ticket toast (Python `_compose_alert`). `truly_new`
/// keeps the order tickets were seen in.
pub fn compose_alert(
    truly_new: &[String],
    summaries: &HashMap<String, String>,
    queues_by_key: &HashMap<String, String>,
    suggestions: &Map<String, Value>,
) -> (String, String) {
    let empty = String::new();
    if let [key] = truly_new {
        let summary = summaries.get(key).unwrap_or(&empty);
        let src = queues_by_key.get(key).unwrap_or(&empty);
        let title = format!("Jira — {key}");
        let mut body = if !summary.is_empty() {
            util::take_chars(summary, 120)
        } else if !src.is_empty() {
            format!("New ticket in {src}")
        } else {
            "New ticket in alert queue".to_owned()
        };
        // The queue it landed in says where the desk wants it to go.
        if !summary.is_empty() && !src.is_empty() {
            body.push_str(&format!("\nIn {src}"));
        }
        if let Some((label, review)) = suggestion(suggestions, key) {
            body.push_str(&format!("\n{} {label}", if review { "⚠" } else { "💡" }));
        }
        return (title, body);
    }
    let title = format!("Jira — {} new tickets", truly_new.len());
    let mut lines: Vec<String> = truly_new
        .iter()
        .take(3)
        .map(|key| {
            let summary = summaries.get(key).unwrap_or(&empty);
            let src = queues_by_key.get(key).unwrap_or(&empty);
            let mut line = if !summary.is_empty() {
                format!("{key}: {}", util::take_chars(summary, 60))
            } else if !src.is_empty() {
                format!("{key} · {src}")
            } else {
                key.clone()
            };
            if let Some((label, review)) = suggestion(suggestions, key) {
                line.push_str(&format!(" {} {label}", if review { "⚠" } else { "💡" }));
            }
            line
        })
        .collect();
    if truly_new.len() > 3 {
        lines.push(format!("…and {} more", truly_new.len() - 3));
    }
    (title, lines.join("\n"))
}

/// Toast inputs from the fetched tickets: `(summaries, queues_by_key)`.
pub fn lookups(issues: &[Issue]) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut summaries = HashMap::new();
    let mut queues = HashMap::new();
    for issue in issues {
        summaries.insert(issue.key.clone(), issue.summary.clone().unwrap_or_default());
        queues.insert(issue.key.clone(), issue.queue.clone());
    }
    (summaries, queues)
}

/// Group issues by source queue, one entry per ticket (Python
/// `_group_by_queue`). Returns `(groups, multiple_queues)`.
pub fn group_by_queue(issues: &[Issue]) -> (Vec<(String, Vec<Issue>)>, bool) {
    let mut seen_keys = HashSet::new();
    let mut groups: Vec<(String, Vec<Issue>)> = Vec::new();
    for issue in issues {
        if !issue.key.is_empty() && !seen_keys.insert(issue.key.clone()) {
            continue; // a ticket matching two alert queues is listed once
        }
        match groups.last_mut() {
            Some((name, group)) if *name == issue.queue => group.push(issue.clone()),
            _ => groups.push((issue.queue.clone(), vec![issue.clone()])),
        }
    }
    let multi = groups.iter().filter(|(name, _)| !name.is_empty()).count() > 1;
    (groups, multi)
}

/// Popup row text: summary cut at 55 characters.
pub fn short_summary(issue: &Issue) -> String {
    let summary = issue.summary.as_deref().unwrap_or("(no summary)");
    if summary.chars().count() > 55 { format!("{}…", util::take_chars(summary, 55)) } else { summary.to_owned() }
}

/// What the alert popup lists: queue headers, at most 15 ticket rows and an
/// overflow line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PopupLine {
    Header(String),
    Ticket { key: String, summary: String, suggestion: Option<(String, bool)> },
    More(usize),
}

pub fn popup_lines(issues: &[Issue], suggestions: &Map<String, Value>) -> Vec<PopupLine> {
    let (groups, multi) = group_by_queue(issues);
    let mut lines = Vec::new();
    let mut shown = 0;
    for (name, group) in &groups {
        if multi && !name.is_empty() {
            lines.push(PopupLine::Header(name.clone()));
        }
        for issue in group {
            if shown >= 15 {
                break;
            }
            lines.push(PopupLine::Ticket {
                key: issue.key.clone(),
                summary: short_summary(issue),
                suggestion: suggestion(suggestions, &issue.key),
            });
            shown += 1;
        }
    }
    let total: usize = groups.iter().map(|(_, g)| g.len()).sum();
    if shown < total {
        lines.push(PopupLine::More(total - shown));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn issue(key: &str, queue: &str) -> Issue {
        Issue { key: key.into(), summary: None, queue: queue.into() }
    }

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn one_empty_reading_does_not_renotify() {
        let tickets: Vec<Issue> = (1..=5).map(|i| issue(&format!("SD-{i}"), "Newly Opened")).collect();
        let mut seen = SeenSet::default();
        assert_eq!(seen.update(&tickets).len(), 5, "all five tickets are new on the first poll");
        assert!(seen.update(&[]).is_empty());
        assert!(seen.update(&tickets).is_empty(), "a single stale-zero poll does not re-notify the queue");
        seen.update(&[]);
        seen.update(&[]);
        assert_eq!(seen.update(&tickets).len(), 5, "a ticket absent for two polls notifies again");
        let blank = [issue("", "Q")];
        assert!(seen.update(&blank).is_empty(), "keyless tickets never notify");
    }

    #[test]
    fn grouping_folds_and_dedupes() {
        let (groups, multi) =
            group_by_queue(&[issue("A-1", "Escalations"), issue("A-2", "Escalations"), issue("B-1", "Billing")]);
        let shape: Vec<(String, Vec<String>)> =
            groups.iter().map(|(n, g)| (n.clone(), g.iter().map(|i| i.key.clone()).collect())).collect();
        assert_eq!(
            shape,
            [("Escalations".into(), vec!["A-1".to_owned(), "A-2".into()]), ("Billing".into(), vec!["B-1".into()])]
        );
        assert!(multi);
        let (dupe, dupe_multi) =
            group_by_queue(&[issue("A-1", "Escalations"), issue("A-1", "Billing"), issue("B-1", "Billing")]);
        assert_eq!(dupe.iter().map(|(_, g)| g.len()).sum::<usize>(), 2, "a ticket in two queues is listed once");
        assert!(dupe_multi);
        let (one, one_multi) = group_by_queue(&[issue("A-1", "Only"), issue("A-2", "Only")]);
        assert!(!one_multi && one.len() == 1, "a single-queue popup skips the sub-headers");
        let (keyless, _) = group_by_queue(&[issue("", "Q"), issue("", "Q")]);
        assert_eq!(keyless[0].1.len(), 2, "keyless issues pass through");
    }

    #[test]
    fn popup_caps_at_fifteen_rows() {
        let mut issues: Vec<Issue> = (0..12).map(|i| issue(&format!("A-{i}"), "Alpha")).collect();
        issues.extend((0..8).map(|i| issue(&format!("B-{i}"), "Beta")));
        let lines = popup_lines(&issues, &Map::new());
        let tickets = lines.iter().filter(|l| matches!(l, PopupLine::Ticket { .. })).count();
        assert_eq!(tickets, 15);
        assert_eq!(lines[0], PopupLine::Header("Alpha".into()));
        assert!(lines.contains(&PopupLine::Header("Beta".into())));
        assert_eq!(lines.last(), Some(&PopupLine::More(5)));
        let sugg = map(json!({"A-0": {"label": "With DEV", "review": true}, "A-1": {"label": ""}}));
        let lines = popup_lines(&issues[..2], &sugg);
        assert_eq!(
            lines[0],
            PopupLine::Ticket {
                key: "A-0".into(),
                summary: "(no summary)".into(),
                suggestion: Some(("With DEV".into(), true))
            }
        );
        assert!(matches!(&lines[1], PopupLine::Ticket { suggestion: None, .. }));
    }

    #[test]
    fn summaries_are_cut_at_55() {
        let mut i = issue("A-1", "Q");
        i.summary = Some("x".repeat(60));
        assert_eq!(short_summary(&i), format!("{}…", "x".repeat(55)));
        i.summary = Some("short".into());
        assert_eq!(short_summary(&i), "short");
    }

    #[test]
    fn toast_text() {
        let s = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let (title, body) = compose_alert(
            &["SD-9".into()],
            &s(&[("SD-9", "Printer on fire")]),
            &s(&[("SD-9", "Escalations")]),
            &Map::new(),
        );
        assert_eq!(title, "Jira — SD-9");
        assert_eq!(body, "Printer on fire\nIn Escalations");

        let (_, body) =
            compose_alert(&["SD-9".into()], &s(&[("SD-9", "")]), &s(&[("SD-9", "Escalations")]), &Map::new());
        assert_eq!(body, "New ticket in Escalations");
        let (_, body) = compose_alert(&["SD-9".into()], &s(&[]), &s(&[]), &Map::new());
        assert_eq!(body, "New ticket in alert queue");

        let keys: Vec<String> = (0..5).map(|i| format!("SD-{i}")).collect();
        let (title, body) = compose_alert(&keys, &s(&[]), &s(&[]), &map(json!({"SD-1": {"label": "Password reset"}})));
        assert_eq!(title, "Jira — 5 new tickets");
        assert_eq!(body.lines().count(), 4);
        assert!(body.ends_with("…and 2 more"));
        assert!(body.contains("SD-1 💡 Password reset"));

        let (_, body) = compose_alert(
            &["SD-9".into()],
            &s(&[("SD-9", "Printer on fire")]),
            &s(&[("SD-9", "Escalations")]),
            &map(json!({"SD-9": {"label": "Power cycle", "review": true}})),
        );
        assert_eq!(body, "Printer on fire\nIn Escalations\n⚠ Power cycle");

        let long = "y".repeat(200);
        let (_, body) = compose_alert(&["A".into(), "B".into()], &s(&[("A", &long)]), &s(&[("B", "Q")]), &Map::new());
        assert_eq!(body, format!("A: {}\nB · Q", "y".repeat(60)));
    }
}
