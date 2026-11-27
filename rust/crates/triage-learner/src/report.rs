//! `TRIAGE_REPORT.md` and its machine-readable twin `triage_status.json`.

use std::fs;

use chrono::{Local, TimeZone};
use serde_json::{Map, Value, json};
use triage_engine::num::py_round_int;

use crate::files::write_json;
use crate::pyfmt::{dict_repr, int_or_float, pct, prefix, py_repr, py_str, truthy};
use crate::support::{accuracy_counts, performance_drift, rolling_metrics};
use crate::watcher::Watcher;

fn num(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

fn int(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f.floor() as i64))).unwrap_or(0)
}

/// `100 * a // b` for non-negative counts.
fn floor_pct(a: u64, b: u64) -> u64 {
    100 * a / b
}

fn shown(v: Option<&Value>, default: &str) -> String {
    v.map(py_str).unwrap_or_else(|| default.to_owned())
}

impl Watcher {
    /// Rewrite the report and status file. Every cycle, whatever Jira did.
    pub fn write_report(&mut self) -> Result<(), String> {
        let empty = Map::new();
        let stats = self.state.get("stats").and_then(Value::as_object).unwrap_or(&empty);
        let (pred_n, pred_c) = (int(stats.get("predictions")).max(0) as u64, int(stats.get("correct")).max(0) as u64);
        let acc = if pred_n > 0 {
            format!("{}% ({pred_c}/{pred_n})", floor_pct(pred_c, pred_n))
        } else {
            "no resolved predictions yet".into()
        };
        let seen_counts = &self.learner.model.seen;
        let trained = triage_engine::num::py_sum(seen_counts.values().copied());
        let last_poll = self.state.get("lastSuccessfulPoll");
        let all_dests = self.all_dests();
        let now = Local::now();
        let mut lines: Vec<String> = vec![
            "# Triage Shadow Learner — report".into(),
            String::new(),
            format!(
                "_Updated {} · v{} · shadow mode — no tickets are ever actioned._",
                now.format("%Y-%m-%d %H:%M:%S"),
                jo_core::VERSION
            ),
            String::new(),
        ];
        let refusals = self.jira.refusals();
        let problem = [self.state.get("runtimeProblem"), self.state.get("intakeProblem")]
            .into_iter()
            .flatten()
            .find(|v| triage_engine::text::truthy(v))
            .map(py_str);
        if let Some(problem) = &problem {
            lines.extend(["## Learner needs attention".into(), String::new(), problem.clone(), String::new()]);
        }
        if !refusals.is_empty() {
            let codes = refusals.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
            lines.extend([
                format!("## ⚠ Jira has been refusing requests ({codes}) on recent requests"),
                String::new(),
                "Some reads failed. Check access if the warning persists; a successful search clears the warning \
                 automatically."
                    .into(),
                String::new(),
            ]);
        }
        let poll_text = match last_poll {
            Some(Value::Number(n)) => n
                .as_f64()
                .and_then(|secs| Local.timestamp_opt(secs.floor() as i64, 0).single())
                .map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string())
                .unwrap_or_else(|| "not recorded yet".into()),
            _ => "not recorded yet".into(),
        };
        let since = match self.state.get("processStart") {
            Some(v) if triage_engine::text::truthy(v) => format!(" since {}", py_str(v)),
            _ => String::new(),
        };
        let last_tune = match self.state.get("lastTuneDate") {
            Some(v) if triage_engine::text::truthy(v) => py_str(v),
            _ => "never yet".into(),
        };
        lines.extend([
            "## Model provenance".into(),
            String::new(),
            format!("- **Last completed poll:** {poll_text}"),
            format!(
                "- **Polling:** {}",
                if truthy(self.state.get("pollIncomplete")) {
                    "incomplete; retrying saved checkpoint"
                } else {
                    "no incomplete scan recorded"
                }
            ),
            format!("- **Trained on:** {} labelled decisions{since}", int_or_float(trained)),
            format!(
                "- **Backfill:** {}",
                if truthy(self.state.get("backfillDone")) {
                    format!("complete ({} decisions)", shown(self.state.get("backfillCount"), "0"))
                } else {
                    "not yet complete — figures below will fill in as it runs".into()
                }
            ),
            format!(
                "- **Last self-tune:** {last_tune} · tuning schema v{}",
                shown(self.state.get("tuningSchemaVersion"), "0")
            ),
            format!("- **Outcomes tracked:** {} ({})", all_dests.len(), all_dests.join(", ")),
            String::new(),
            format!("## Learned triage map for `{}`", self.project),
            String::new(),
            "| Outcome | What the team does | Observed examples |".into(),
            "|---|---|---|".into(),
        ]);
        for dest in &all_dests {
            let seen = seen_counts.get(dest).copied().unwrap_or(0.0);
            lines.push(format!("| {dest} | {} | {} |", self.learner.plan_for(dest), int_or_float(seen)));
        }

        let labels = self.labels.clone();
        let live = self.metrics.live(&|d| labels.canonical_dest(d)).clone();
        let naive_now = now.naive_local();
        let windows = [
            (7, rolling_metrics(&live.recent_events, naive_now, 7)),
            (30, rolling_metrics(&live.recent_events, naive_now, 30)),
        ];
        let trend = performance_drift(&live.recent_events, naive_now);
        lines.extend([
            String::new(),
            "## Recent live performance".into(),
            String::new(),
            "Windows use when a routing was observed; counts are events, not unique tickets.".into(),
            String::new(),
        ]);
        for (days, recent) in &windows {
            let (total, right) = (recent.predicted, recent.correct);
            let accuracy = if total > 0 {
                format!("{} ({right}/{total})", pct(right as f64 / total as f64, 1))
            } else {
                "no scored events".into()
            };
            let coverage = if recent.routed > 0 {
                format!("{:.1}% ({total}/{})", recent.coverage_pct.unwrap_or(0.0), recent.routed)
            } else {
                "no routing events".into()
            };
            lines.push(format!("- **Last {days} days:** {accuracy}; intake prediction coverage {coverage}"));
            for (segment, [n, ok]) in &recent.segments {
                lines.push(format!("  - {segment}: {ok}/{n} ({})", pct(*ok as f64 / *n as f64, 1)));
            }
        }
        let recent = &windows[1].1;
        lines.extend([
            String::new(),
            "### Gradual performance drift".into(),
            String::new(),
            "Compares the last 7 days with the preceding, non-overlapping 21 days. Warnings require a material drop and \
             sufficient sample evidence; they are prompts to investigate."
                .into(),
        ]);
        let (old, new) = (trend.baseline.accuracy(), trend.recent.accuracy());
        if trend.baseline.predicted > 0 && trend.recent.predicted > 0 {
            lines.push(format!(
                "- Overall: {}% ({}/{}) baseline versus {}% ({}/{}) recently.",
                py_repr(&old["pct"]),
                old["correct"],
                old["predictions"],
                py_repr(&new["pct"]),
                new["correct"],
                new["predictions"]
            ));
        }
        let warnings: Vec<_> = trend.warnings().collect();
        if warnings.is_empty() {
            lines.push(
                "- No deterioration met the warning criteria; small samples and smaller changes can still warrant review."
                    .into(),
            );
        } else {
            for w in &warnings {
                lines.push(format!(
                    "- Warning — {}: {} dropped {:.1} percentage points.",
                    w.kind, w.label, w.drop_points
                ));
            }
        }
        if !recent.by_actual.is_empty() || !recent.by_predicted.is_empty() {
            lines.extend([
                String::new(),
                "### Last 30 days by route".into(),
                String::new(),
                "| Route | Recall | Precision |".into(),
                "|---|---:|---:|".into(),
            ]);
            for dest in &all_dests {
                let [an, aok] = recent.by_actual.get(dest).copied().unwrap_or([0, 0]);
                let [sn, sok] = recent.by_predicted.get(dest).copied().unwrap_or([0, 0]);
                let recall =
                    if an > 0 { format!("{aok}/{an} ({})", pct(aok as f64 / an as f64, 1)) } else { "—".into() };
                let precision =
                    if sn > 0 { format!("{sok}/{sn} ({})", pct(sok as f64 / sn as f64, 1)) } else { "—".into() };
                lines.push(format!("| {dest} | {recall} | {precision} |"));
            }
        }
        let mut missing: Vec<(&String, &u64)> =
            recent.coverage_reasons.iter().filter(|(k, _)| *k != "predicted").collect();
        if !missing.is_empty() {
            lines.extend([
                String::new(),
                "### Unpredicted routing events (last 30 days)".into(),
                String::new(),
                "Categories describe available log evidence; no intake observation does not establish a polling failure."
                    .into(),
            ]);
            missing.sort();
            for (reason, n) in missing {
                lines.push(format!("- {}: {n}", reason.replace('_', " ")));
            }
            let mut days: Vec<(&String, u64, u64)> =
                recent.daily_coverage.iter().filter(|(_, [n, s])| n > s).map(|(d, [n, s])| (d, n - s, *n)).collect();
            days.sort_by_key(|r| std::cmp::Reverse(r.1));
            for (day, missing, total) in days.into_iter().take(3) {
                lines.push(format!("- {day}: {missing}/{total} routing events had no prediction"));
            }
        }
        lines.extend([String::new(), format!("## Prediction accuracy: {acc}")]);
        if live.routed > 0 {
            lines.push(format!(
                "- **Prediction coverage:** {}% ({}/{} routed tickets received an intake suggestion)",
                floor_pct(live.predicted, live.routed),
                live.predicted,
                live.routed
            ));
        }
        for (key, label) in [
            ("confident", "Confident suggestions"),
            ("review", "Review-flagged suggestions"),
            ("unknown", "Suggestions with unknown historical review status"),
        ] {
            let [total, right] = live.band(key).unwrap_or([0, 0]);
            if total > 0 {
                lines.push(format!("- **{label}:** {}% ({right}/{total})", floor_pct(right, total)));
            }
        }
        if !live.by_source.is_empty() {
            let mut sources: Vec<_> = live.by_source.iter().collect();
            sources.sort();
            let bits: Vec<String> = sources
                .into_iter()
                .map(|(source, [total, right])| format!("{source} {}% ({right}/{total})", floor_pct(*right, *total)))
                .collect();
            lines.push(format!("- **Novel vs recurring:** {}", bits.join("; ")));
        }
        lines.push(
            "- **Structured signals:** Request Type, affected service, and organisation are enabled for new observations"
                .into(),
        );
        if !live.by_actual.is_empty() || !live.by_predicted.is_empty() {
            lines.extend([
                String::new(),
                "### Live accuracy by route".into(),
                String::new(),
                "| Route | Recall when actually chosen | Precision when suggested | Confident precision |".into(),
                "|---|---:|---:|---:|".into(),
            ]);
            let cell =
                |[n, ok]: [u64; 2]| if n > 0 { format!("{ok}/{n} ({}%)", floor_pct(ok, n)) } else { "—".into() };
            for dest in &all_dests {
                let get = |t: &crate::support::Counts| t.get(dest).copied().unwrap_or([0, 0]);
                lines.push(format!(
                    "| {dest} | {} | {} | {} |",
                    cell(get(&live.by_actual)),
                    cell(get(&live.by_predicted)),
                    cell(get(&live.confident_by_predicted))
                ));
            }
        }
        let route_safety = self.learner.route_safety.clone();
        if let Some(segments) = live.segments.as_ref().filter(|s| !s.is_empty()) {
            lines.extend([String::new(), "### Accuracy by observed request type".into(), String::new()]);
            for (name, [total, right]) in segments {
                lines.push(format!("- {name}: {right}/{total} ({})", pct(*right as f64 / *total as f64, 1)));
            }
        }
        lines.extend([
            String::new(),
            "### Mistake review".into(),
            String::new(),
            "Run `python triage_review.py` to view recurring confusions and annotate cases.".into(),
            "Use `python triage_review.py --export TRIAGE_REVIEW_QUEUE.md` for a prioritised, representative local review \
             queue."
                .into(),
            "New training examples preserve intake features; older examples use outcome-time features.".into(),
        ]);
        let guarded: Vec<(&String, &Value)> =
            route_safety.iter().filter(|(_, p)| truthy(p.get("forceReview"))).collect();
        if !guarded.is_empty() {
            lines.extend([String::new(), "### Routes currently forced to human review".into(), String::new()]);
            for (dest, policy) in guarded {
                lines.push(format!("- **{dest}:** {}", shown(policy.get("reason"), "needs more evidence")));
            }
        }

        if let Some(Value::Object(daily)) = self.state.get("dailyStats").filter(|d| truthy(Some(d))) {
            lines.extend([String::new(), "## Daily accuracy (live suggestions vs team)".into(), String::new()]);
            let mut days: Vec<&String> = daily.keys().collect();
            days.sort();
            for day in days.iter().skip(days.len().saturating_sub(14)) {
                let row = daily[day.as_str()].as_array();
                let (dn, dc) = (int(row.and_then(|r| r.first())), int(row.and_then(|r| r.get(1))));
                if dn != 0 {
                    let bar = "█".repeat(py_round_int(10.0 * dc as f64 / dn as f64).max(0) as usize);
                    lines.push(format!("- {day}: {:>3}% ({dc}/{dn})  {bar}", (100 * dc).div_euclid(dn)));
                }
            }
        }

        let tuned = self.state.get("tunedConfig").filter(|t| truthy(Some(t)));
        let history: Vec<Map<String, Value>> = match self.state.get("tuneHistory") {
            Some(Value::Array(h)) => h.iter().filter_map(Value::as_object).cloned().collect(),
            _ => Vec::new(),
        };
        if tuned.is_some() || !history.is_empty() {
            lines.extend([String::new(), "## Self-tuning (runs daily on accumulated history)".into(), String::new()]);
            if let Some(Value::Object(drift)) = self.state.get("tuneDrift") {
                let last_date = history.last().and_then(|t| t.get("date")).cloned();
                if !drift.is_empty() && drift.get("date").cloned() == last_date {
                    lines.push(format!(
                        "- ⚠ **Drift since last night's tune ({}):** {}",
                        shown(drift.get("date"), "None"),
                        shown(drift.get("detail"), "None")
                    ));
                }
            }
            if let Some(tuned) = tuned {
                let text = match tuned {
                    Value::Object(m) => dict_repr(m),
                    other => py_repr(other),
                };
                lines.push(format!("- **Current auto-tuned config:** `{text}`"));
            }
            for t in history.iter().skip(history.len().saturating_sub(5)) {
                let date = shown(t.get("date"), "None");
                let prefix_text = if truthy(t.get("holdoutN")) {
                    format!("- {date}: recent holdout ")
                } else {
                    format!("- {date}: ")
                };
                let mut parts = vec![format!(
                    "{prefix_text}confident-band {:.0}% over {:.0}% of tickets",
                    num(t.get("firmAcc")) * 100.0,
                    num(t.get("firmCov")) * 100.0
                )];
                if truthy(t.get("balanced")) && truthy(t.get("worst")) {
                    parts.push(format!(
                        "balanced {:.0}% (weakest {:.0}%)",
                        num(t.get("balanced")) * 100.0,
                        num(t.get("worst")) * 100.0
                    ));
                }
                if t.get("macroF1").is_some_and(|v| !v.is_null()) {
                    parts.push(format!("macro F1 {:.0}%", num(t.get("macroF1")) * 100.0));
                }
                if truthy(t.get("holdoutN")) {
                    let windows = t.get("validationWindows").cloned().unwrap_or(json!(1));
                    let plural = if windows.as_f64() != Some(1.0) { "s" } else { "" };
                    parts.push(format!(
                        "overall {:.0}% ({} unseen decisions across {} recent window{plural}; selected on {})",
                        num(t.get("acc")) * 100.0,
                        shown(t.get("holdoutN"), "None"),
                        py_str(&windows),
                        shown(t.get("trainN"), "None")
                    ));
                }
                if truthy(t.get("finalHoldoutN")) {
                    parts.push(format!(
                        "latest untouched window {:.0}% ({} decisions)",
                        num(t.get("finalAcc")) * 100.0,
                        shown(t.get("finalHoldoutN"), "None")
                    ));
                }
                if truthy(t.get("challengerRejected")) {
                    parts.push("challenger rejected; incumbent retained".into());
                } else {
                    parts.push(format!(
                        "overall {:.0}% (replayed {})",
                        num(t.get("acc")) * 100.0,
                        shown(t.get("n"), "None")
                    ));
                }
                lines.push(parts.join(" · "));
            }
        }

        if let Some(Value::Object(confusion)) = self.state.get("confusion").filter(|c| truthy(Some(c))) {
            let split = |pair: &str| -> Option<(String, String)> {
                let parts: Vec<&str> = pair.split(" → ").collect();
                (parts.len() == 2).then(|| (self.canonical_dest(parts[0]), self.canonical_dest(parts[1])))
            };
            let mut per: indexmap::IndexMap<String, [i64; 2]> = indexmap::IndexMap::new();
            for (pair, count) in confusion {
                let Some((p_dest, actual)) = split(pair) else { continue };
                let count = int(Some(count));
                let row = per.entry(actual.clone()).or_insert([0, 0]);
                row[0] += count;
                if p_dest == actual {
                    row[1] += count;
                }
            }
            lines.push(String::new());
            for dest in &all_dests {
                if let Some([total, right]) = per.get(dest) {
                    let share = if *total != 0 { (100 * right).div_euclid(*total) } else { 0 };
                    lines.push(format!("- {dest}: {share}% ({right}/{total})"));
                }
            }
            let mut wrong: indexmap::IndexMap<String, i64> = indexmap::IndexMap::new();
            for (pair, count) in confusion {
                let Some((p_dest, actual)) = split(pair) else { continue };
                if p_dest != actual {
                    *wrong.entry(format!("{p_dest} → {actual}")).or_insert(0) += int(Some(count));
                }
            }
            let mut wrong: Vec<(String, i64)> = wrong.into_iter().collect();
            wrong.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            if !wrong.is_empty() {
                lines.extend([String::new(), "## Mistakes learned from (said → team chose)".into(), String::new()]);
                for (pair, n) in wrong.iter().take(10) {
                    lines.push(format!("- {pair} — {n}×"));
                }
            }
        }

        lines.extend([String::new(), "## Signals per destination".into(), String::new()]);
        for dest in &all_dests {
            let kws = self.learner.top_keywords(dest, 12).join(", ");
            let kws = if kws.is_empty() { "—".to_owned() } else { kws };
            let mut rts: Vec<(&String, f64)> = self
                .learner
                .model
                .req_types
                .get(dest)
                .map(|t| t.iter().map(|(k, v)| (k, *v)).collect())
                .unwrap_or_default();
            rts.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let top: Vec<String> = rts.iter().take(4).map(|(k, v)| format!("{k} ({v:.0})")).collect();
            let top = if top.is_empty() { "—".to_owned() } else { top.join(", ") };
            lines.extend([
                format!("### {dest}"),
                format!("- **Top keywords:** {kws}"),
                format!("- **Request types:** {top}"),
                String::new(),
            ]);
        }

        lines.extend(["## Recent activity (newest first)".into(), String::new()]);
        match fs::read(&self.paths.log) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let all: Vec<&str> = text.lines().collect();
                let tail = &all[all.len().saturating_sub(60)..];
                let mut count = 0;
                for raw in tail.iter().rev() {
                    if count >= 15 {
                        break;
                    }
                    let Ok(Value::Object(rec)) = serde_json::from_str::<Value>(raw) else { continue };
                    let event = rec.get("event").and_then(Value::as_str);
                    let summary = prefix(&shown(rec.get("summary"), ""), 90);
                    if event == Some("routed") {
                        let mark = match rec.get("predictionCorrect") {
                            Some(v) if triage_engine::text::truthy(v) => "✅",
                            Some(Value::Bool(false)) => "❌",
                            _ => "•",
                        };
                        let pred = match rec.get("predicted") {
                            Some(p) if triage_engine::text::truthy(p) => format!(" — I said **{}** {mark}", py_str(p)),
                            _ => String::new(),
                        };
                        let actual = self.canonical_dest(&shown(rec.get("dest"), "unknown"));
                        let clones = match rec.get("clones") {
                            Some(Value::Array(c)) if !c.is_empty() => {
                                c.iter().map(py_str).collect::<Vec<_>>().join(", ")
                            }
                            _ => String::new(),
                        };
                        let clones = if clones.is_empty() { "none".to_owned() } else { clones };
                        lines.push(format!(
                            "- `{}` **{actual}** by {} (clones: {clones}){pred}  \n  {summary}",
                            shown(rec.get("key"), ""),
                            shown(rec.get("by"), "?")
                        ));
                        count += 1;
                    } else if event == Some("would_have_done") && truthy(rec.get("prediction")) {
                        lines.push(format!(
                            "- `{}` would have done: **{}** ({}% confident)  \n  {summary}",
                            shown(rec.get("key"), ""),
                            shown(rec.get("plan"), ""),
                            shown(rec.get("confidence"), "0")
                        ));
                        count += 1;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => lines.push("_No activity logged yet._".into()),
            Err(e) => return Err(e.to_string()),
        }
        let mut body = lines.join("\n");
        body.push('\n');
        fs::write(&self.paths.report, body).map_err(|e| e.to_string())?;

        let poll_incomplete = truthy(self.state.get("pollIncomplete"));
        let message = match &problem {
            Some(p) => p.clone(),
            None if poll_incomplete => "Polling is incomplete; retrying from the last checkpoint".into(),
            None if !refusals.is_empty() => "Jira is refusing requests".into(),
            None => "Healthy".into(),
        };
        let get = |k: &str| self.state.get(k).cloned().unwrap_or(Value::Null);
        let drift = if warnings.is_empty() {
            get("tuneDrift")
        } else {
            let detail: Vec<String> = warnings
                .iter()
                .take(5)
                .map(|w| format!("{} {} dropped {:.1} points", w.label, w.kind, w.drop_points))
                .collect();
            json!({"date": now.format("%Y-%m-%d").to_string(), "detail": detail.join("; ")})
        };
        let status = json!({
            "updatedAt": Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            "version": jo_core::VERSION,
            "healthy": problem.is_none() && refusals.is_empty() && !poll_incomplete,
            "healthMessage": message,
            "lastSuccessfulPoll": get("lastSuccessfulPoll"),
            "refusals": refusals,
            "provenance": {
                "trainedOnDecisions": triage_engine::model::int_or_float(trained),
                "trainingSince": get("processStart"),
                "backfillComplete": truthy(self.state.get("backfillDone")),
                "backfillCount": self.state.get("backfillCount").cloned().unwrap_or(json!(0)),
                "lastTuneDate": get("lastTuneDate"),
                "tuningSchemaVersion": self.state.get("tuningSchemaVersion").cloned().unwrap_or(json!(0)),
                "outcomes": all_dests,
            },
            "accuracy": accuracy_counts(pred_n, pred_c),
            "routeSafety": route_safety,
            "segments": live.segments.as_ref().map_or(json!({}), |s| Value::Object(
                s.iter().map(|(k, v)| (k.clone(), json!(v))).collect())),
            "recentPerformance": {"7": windows[0].1.to_value(), "30": windows[1].1.to_value()},
            "performanceDrift": trend.to_value(),
            "drift": drift,
        });
        if let Err(e) = write_json(&self.paths.status, &status, true) {
            self.log_error(&format!("could not write {}: {e}", self.paths.status.display()));
        }
        Ok(())
    }
}
