//! Offline command-line tools: `triage_eval.py` and `triage_review.py`.
//!
//! `args` are the arguments after the program name (Python `sys.argv[1:]`).
//! Each returns the process exit code.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::Local;
use indexmap::IndexMap;
use jo_jira::Transport;
use serde_json::{Map, Value, json};
use triage_engine::Tuning;
use triage_engine::replay::{Metrics, Record, replay, self_tune};

use crate::discovery::as_name_list;
use crate::files::{Paths, load_history, read_object, write_json};
use crate::pyfmt::{ascii_escape, list_repr, pct, prefix, py_str, truthy};
use crate::support::threshold_experiment;
use crate::watcher::{BACKFILL_CAP, BACKFILL_DAYS, TUNE_WINDOW, Watcher};

/// `triage_eval.py` equivalent.
pub fn eval(root: &Path, args: &[String]) -> i32 {
    let mut out = io::stdout().lock();
    let connect = || -> Result<Box<dyn Transport>, String> {
        jo_jira::Http::new().map(|h| Box::new(h) as Box<dyn Transport>).map_err(|e| e.to_string())
    };
    eval_with(root, args, &mut out, &connect)
}

/// `triage_review.py` equivalent.
pub fn review(root: &Path, args: &[String]) -> i32 {
    let mut out = io::stdout().lock();
    let mut err = io::stderr().lock();
    review_with(root, args, &mut out, &mut err)
}

// ---------------------------------------------------------------------------
// triage_eval
// ---------------------------------------------------------------------------

const EVAL_DOC: &str = "
Offline evaluator/tuner for the triage shadow learner (private local dataset).

  python triage_eval.py build    fetch the last 90 days of real triage
                                 decisions into triage_dataset.jsonl (read-only)
  python triage_eval.py tune     replay the dataset chronologically under many
                                 TUNING configs and report which learns best

The replay is honest: each decision is predicted using ONLY the decisions
that happened before it, then the model learns from it — exactly how the
live learner experiences the stream.
";

/// Don't score the first N decisions (model barely trained).
const WARMUP: usize = 150;

const DEFAULT_CFG: [(&str, f64); 15] = [
    ("logOdds", 1.0),
    ("summaryWeight", 6.0),
    ("mistakeWeight", 3.0),
    ("confTemp", 4.0),
    ("reqSmooth", 10.0),
    ("repSmooth", 20.0),
    ("priorWeight", 0.5),
    ("knnThresh", 0.7),
    ("decayFactor", 0.997),
    ("reviewConf", 75.0),
    ("reviewMargin", 20.0),
    ("requestTypeHintWeight", 3.0),
    ("reporterWeight", 1.0),
    ("escalationSpecialist", 1.0),
    ("escalationPriorWeight", 0.25),
];

type Connect<'a> = &'a dyn Fn() -> Result<Box<dyn Transport>, String>;

pub fn eval_with(root: &Path, args: &[String], out: &mut dyn Write, connect: Connect<'_>) -> i32 {
    let paths = Paths::new(root);
    let mode = args.first().map_or("tune", String::as_str);
    let result = match mode {
        "build" => build(&paths, out, connect),
        "tune" => tune(&paths, out),
        "review" => review_bands(&paths, out),
        "thresholds" => thresholds(&paths, out),
        "analyze" => analyze(
            &paths,
            out,
            &[
                ("summaryWeight", 4.0),
                ("mistakeWeight", 2.0),
                ("confTemp", 12.0),
                ("reqSmooth", 10.0),
                ("repSmooth", 20.0),
            ],
        ),
        _ => writeln!(out, "{EVAL_DOC}").map_err(|e| e.to_string()),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn w(out: &mut dyn Write, line: impl AsRef<str>) -> Result<(), String> {
    writeln!(out, "{}", line.as_ref()).map_err(|e| e.to_string())
}

/// Use live history by default; `TRIAGE_DATASET` selects an explicit dataset.
fn load_dataset(paths: &Paths, out: &mut dyn Write) -> Result<Option<Vec<Value>>, String> {
    let selected = std::env::var("TRIAGE_DATASET").ok().filter(|s| !s.is_empty());
    if selected.is_none() && paths.history.exists() {
        return Ok(load_history(&paths.history, &paths.error_log));
    }
    let file = selected.map_or_else(|| paths.dataset.clone(), PathBuf::from);
    let text = match fs::read(&file) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(load_history(&paths.history, &paths.error_log)),
        Err(e) => return Err(e.to_string()),
    };
    let mut records = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Value>(line) {
            Ok(v) => records.push(v),
            Err(e) => {
                w(
                    out,
                    format!(
                        "{} is corrupt ({e}). Rebuild it with:   python triage_eval.py build",
                        paths.dataset.display()
                    ),
                )?;
                return Ok(None);
            }
        }
    }
    records.sort_by(|a, b| crate::files::when_of(a).cmp(crate::files::when_of(b)));
    Ok(Some(records))
}

fn to_records(rows: &[Value]) -> Result<Vec<Record>, String> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| Record::from_value(r).ok_or_else(|| format!("record {} lacks a text 'summary' or 'dest'", i + 1)))
        .collect()
}

/// Outcomes that represent a handoff, taken from the discovered config;
/// otherwise only explicit record evidence counts.
pub fn handoff_dests(paths: &Paths, records: &[Value]) -> BTreeSet<String> {
    let config = read_object(&paths.config);
    if let Some(statuses) = config.get("triageStatuses") {
        return as_name_list(Some(statuses)).into_iter().collect();
    }
    records
        .iter()
        .filter(|r| truthy(r.get("cloneProjects")) || r.get("isHandoff") == Some(&Value::Bool(true)))
        .filter_map(|r| r.get("dest").map(py_str))
        .collect()
}

/// Replay plus the offline extras (`cov90`, `acc90`, `reviewCov`, confusion).
struct Evaluation {
    m: Metrics,
    cov90: f64,
    acc90: f64,
    review_cov: f64,
    confusion: IndexMap<String, u64>,
    misses: Vec<(String, String, i64, String)>,
}

fn evaluate(records: &[Record], tuning: &Tuning, dests: &[String], start: usize, end: Option<usize>) -> Evaluation {
    let m = replay(records, tuning, dests, Some(start), end, false);
    let hi: Vec<_> = m.samples.iter().filter(|p| p.conf >= 90).collect();
    let cov90 = if m.n > 0 { hi.len() as f64 / m.n as f64 } else { 0.0 };
    let acc90 = if hi.is_empty() { 0.0 } else { hi.iter().map(|p| p.correct).sum::<u64>() as f64 / hi.len() as f64 };
    let mut confusion = IndexMap::new();
    let mut misses = Vec::new();
    for p in m.samples.iter().filter(|p| p.correct == 0) {
        *confusion.entry(format!("{} predicted-as {}", p.actual, p.predicted)).or_insert(0) += 1;
        misses.push((p.actual.clone(), p.predicted.clone(), p.conf, p.summary.clone()));
    }
    let review_cov = 1.0 - m.firm_cov;
    Evaluation { m, cov90, acc90, review_cov, confusion, misses }
}

/// Records from when the team's handoff process started.
fn since_handoffs(paths: &Paths, rows: Vec<Value>) -> (Vec<Value>, Vec<String>) {
    let handoffs = handoff_dests(paths, &rows);
    let team: Option<String> = rows
        .iter()
        .filter(|r| r.get("dest").is_some_and(|d| handoffs.contains(&py_str(d))))
        .map(|r| crate::files::when_of(r).to_owned())
        .min();
    let rows = match team {
        Some(start) => rows.into_iter().filter(|r| crate::files::when_of(r) >= start.as_str()).collect(),
        None => rows,
    };
    (rows, handoffs.into_iter().collect())
}

fn review_bands(paths: &Paths, out: &mut dyn Write) -> Result<(), String> {
    let Some(rows) = load_dataset(paths, out)? else { return Ok(()) };
    let (rows, dests) = since_handoffs(paths, rows);
    let records = to_records(&rows)?;
    let warmup = WARMUP.min(records.len() / 5);
    w(
        out,
        format!(
            "replaying {} decisions (scoring after {warmup}); overall vs the confident (non-review) band\n",
            records.len()
        ),
    )?;
    w(out, "reviewConf reviewMargin |  overall  | firm-cov firm-acc | review-cov")?;
    for rc in [70.0, 75.0, 80.0] {
        for rm in [15.0, 20.0, 25.0] {
            let tuning = Tuning::default().with(&DEFAULT_CFG).with(&[("reviewConf", rc), ("reviewMargin", rm)]);
            let e = evaluate(&records, &tuning, &dests, warmup, None);
            w(
                out,
                format!(
                    "    {rc:>3}       {rm:>3}      | {:>6}   | {:>6}  {:>6}  | {:>6}",
                    pct(e.m.acc, 1),
                    pct(e.m.firm_cov, 1),
                    pct(e.m.firm_acc, 1),
                    pct(e.review_cov, 1)
                ),
            )?;
        }
    }
    Ok(())
}

fn analyze(paths: &Paths, out: &mut dyn Write, cfg: &[(&str, f64)]) -> Result<(), String> {
    let Some(rows) = load_dataset(paths, out)? else { return Ok(()) };
    let (rows, dests) = since_handoffs(paths, rows);
    let records = to_records(&rows)?;
    let warmup = WARMUP.min(records.len() / 5);
    let e = evaluate(&records, &Tuning::default().with(cfg), &dests, warmup, None);
    w(out, format!("acc={}  cov90={}  acc90={}", pct(e.m.acc, 1), pct(e.cov90, 1), pct(e.acc90, 1)))?;
    w(out, "\nconfusion (actual predicted-as guess):")?;
    let mut pairs: Vec<_> = e.confusion.iter().collect();
    pairs.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    for (pair, n) in pairs {
        w(out, format!("  {pair}: {n}"))?;
    }
    let mut per_dest: IndexMap<&String, u64> = IndexMap::new();
    for (actual, ..) in &e.misses {
        *per_dest.entry(actual).or_insert(0) += 1;
    }
    let focus = triage_engine::num::first_max(per_dest.iter().map(|(k, v)| (*k, v))).cloned().unwrap_or_default();
    w(out, format!("\nsample misses for the weakest outcome ({}):", if focus.is_empty() { "n/a" } else { &focus }))?;
    for (_, guess, conf, summary) in e.misses.iter().filter(|m| m.0 == focus).take(20) {
        w(out, format!("  [{conf:>3}% {guess}] {}", prefix(summary, 80)))?;
    }
    Ok(())
}

fn live_tuning(paths: &Paths) -> Tuning {
    let mut tuning = Tuning::default();
    if let Some(Value::Object(cfg)) = read_object(&paths.state).get("tunedConfig") {
        tuning.update_from(cfg);
    }
    tuning
}

fn json_report(value: &Value) -> Result<String, String> {
    let bytes = jo_core::atomic::to_json_pretty(value, 2).map_err(|e| e.to_string())?;
    Ok(ascii_escape(&String::from_utf8_lossy(&bytes)))
}

fn tune(paths: &Paths, out: &mut dyn Write) -> Result<(), String> {
    let rows = load_dataset(paths, out)?.unwrap_or_default();
    if rows.len() < 300 {
        return w(out, "At least 300 labelled decisions are required.");
    }
    let tuning = live_tuning(paths);
    let dests: Vec<String> = handoff_dests(paths, &rows).into_iter().collect();
    let records = to_records(&rows[rows.len().saturating_sub(TUNE_WINDOW)..])?;
    let Some(best) = self_tune(&records, &tuning, &dests) else { return Ok(()) };
    let summary = crate::watcher::tune_summary(&best);
    let mut report = Map::new();
    for key in [
        "cfg",
        "gateReason",
        "incumbentGateScore",
        "challengerGateScore",
        "holdoutN",
        "finalHoldoutN",
        "acc",
        "macroF1",
        "segments",
        "featureOrigins",
    ] {
        report.insert(key.into(), summary.get(key).cloned().unwrap_or(Value::Null));
    }
    report.insert("finalAccuracy".into(), json!(best.final_holdout.acc));
    w(out, json_report(&Value::Object(report))?)
}

fn thresholds(paths: &Paths, out: &mut dyn Write) -> Result<(), String> {
    let rows = load_dataset(paths, out)?.unwrap_or_default();
    if rows.len() < 600 {
        return w(out, "At least 600 labelled decisions are needed for separate gate and final windows.");
    }
    let tuning = live_tuning(paths);
    let dests: Vec<String> = handoff_dests(paths, &rows).into_iter().collect();
    let records = to_records(&rows)?;
    let n = records.len();
    let start = n - 1200.min(n / 3);
    let final_start = n - 400.min((n - start) / 3);
    let gate = evaluate(&records, &tuning, &dests, start, Some(final_start));
    let fin = evaluate(&records, &tuning, &dests, final_start, None);
    w(out, "Experimental thresholds; these do not change deployed policy.")?;
    w(out, json_report(&threshold_experiment(&gate.m.samples, &fin.m.samples))?)
}

fn dataset_row(det: &crate::issue::Details, dest: &str, when: &str) -> Value {
    json!({"key": det.key, "dest": dest, "when": when, "summary": det.summary, "desc": det.desc,
           "labels": det.labels, "components": det.components, "structured": det.structured,
           "reporter": det.reporter, "reqType": det.request_type, "cloneProjects": det.clone_projects()})
}

/// Fetch the 90-day decision history into `triage_dataset.jsonl` (read-only).
fn build(paths: &Paths, out: &mut dyn Write, connect: Connect<'_>) -> Result<(), String> {
    let mut wch = Watcher::open(&paths.root, connect()?)?;
    if !wch.ensure_workflow_known()? {
        return w(
            out,
            "Could not work out this project's workflow. Check projectKey and your credentials in jira_config.json, or \
             set triageStatuses / resolvedStatuses by hand.",
        );
    }
    let or_none = |v: &[String]| if v.is_empty() { "none".to_owned() } else { list_repr(v) };
    let resolved: Vec<String> = wch.resolved.iter().cloned().collect();
    w(
        out,
        format!(
            "project {}: handoffs={} resolved={} moves={}",
            wch.project,
            or_none(&wch.dests),
            list_repr(&resolved),
            or_none(&wch.labels.move_targets)
        ),
    )?;
    let cutoff = (wch.jira_now() - chrono::Duration::days(BACKFILL_DAYS)).format("%Y-%m-%d").to_string();
    let mut records: Vec<Value> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let fields: Vec<String> = wch.issue_fields.clone();
    let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
    let dest_list = wch.dests.iter().map(|d| jo_core::jql::quote(d)).collect::<Vec<_>>().join(", ");
    let jql = format!(
        "project = {} AND status CHANGED TO ({dest_list}) AFTER -{BACKFILL_DAYS}d ORDER BY updated DESC",
        wch.project
    );
    let issues = if wch.dests.is_empty() { Vec::new() } else { wch.jira.search(&jql, &fields, Some(BACKFILL_CAP)) };
    for issue in &issues {
        let det = wch.issue_details(issue);
        let changelog = wch.jira.changelog(&det.key);
        let (routed, _) = crate::issue::transitions(&det.key, &changelog, &wch.dests, &wch.resolved);
        for tr in routed {
            if seen.contains(&tr.id) || tr.when < cutoff {
                continue;
            }
            seen.insert(tr.id.clone());
            records.push(dataset_row(&det, &tr.dest, &tr.when));
        }
    }
    w(out, format!("routings: {}", records.len()))?;
    let n0 = records.len();
    let resolved_list = wch.resolved.iter().map(|s| jo_core::jql::quote(s)).collect::<Vec<_>>().join(", ");
    let jql = format!(
        "project = {} AND status CHANGED TO ({resolved_list}) AFTER -{BACKFILL_DAYS}d ORDER BY updated DESC",
        wch.project
    );
    for issue in &wch.jira.search(&jql, &fields, Some(BACKFILL_CAP)) {
        let det = wch.issue_details(issue);
        let changelog = wch.jira.changelog(&det.key);
        let (routed, resolutions) = crate::issue::transitions(&det.key, &changelog, &wch.dests, &wch.resolved);
        if !routed.is_empty() {
            continue;
        }
        for tr in resolutions {
            if seen.contains(&tr.id) || tr.when < cutoff {
                continue;
            }
            seen.insert(tr.id.clone());
            records.push(dataset_row(&det, &wch.labels.resolved_label, &tr.when));
        }
    }
    w(out, format!("resolved in place: {}", records.len() - n0))?;
    let n0 = records.len();
    for target in wch.labels.move_targets.clone() {
        let jql = format!("project = {target} AND updated >= -{BACKFILL_DAYS}d ORDER BY created DESC");
        let label = wch.labels.moved_label(&target);
        for issue in &wch.jira.search(&jql, &fields, Some(BACKFILL_CAP)) {
            let mut det = wch.issue_details(issue);
            let changelog = wch.jira.changelog(&det.key);
            let (moves, orig_summary) = crate::issue::project_moves(&det.key, &changelog, &wch.project);
            if moves.is_empty() {
                continue;
            }
            if !orig_summary.is_empty() {
                det.summary = format!("{orig_summary} — {}", det.summary);
            }
            for mv in moves {
                if seen.contains(&mv.id) || mv.when < cutoff {
                    continue;
                }
                seen.insert(mv.id.clone());
                records.push(dataset_row(&det, &label, &mv.when));
            }
        }
    }
    w(out, format!("moved to another project: {}", records.len() - n0))?;
    records.sort_by(|a, b| crate::files::when_of(a).cmp(crate::files::when_of(b)));
    let mut body = String::new();
    for r in &records {
        body.push_str(&jo_core::atomic::to_json_line(r).map_err(|e| e.to_string())?);
        body.push('\n');
    }
    jo_core::atomic::write_atomic(&paths.dataset, body.as_bytes(), jo_core::atomic::WriteOptions::default())
        .map_err(|e| e.to_string())?;
    w(out, format!("dataset: {} decisions -> {}", records.len(), paths.dataset.display()))
}

// ---------------------------------------------------------------------------
// triage_review
// ---------------------------------------------------------------------------

const PROG: &str = "triage_review.py";
const USAGE: &str = "usage: triage_review.py [-h] [--case CASE] [--note NOTE]
                        [--verdict {confirmed_routing,ambiguous,label_issue}]
                        [--limit LIMIT] [--per-pair PER_PAIR] [--export EXPORT]
                        [--since SINCE] [--analyse] [--predicted PREDICTED]";
const HELP: &str = "
Local mistake review. Annotations do not relabel training data or edit Jira.

options:
  -h, --help            show this help message and exit
  --case CASE           Case ID printed in the queue
  --note NOTE           Human explanation of the confusion
  --verdict {confirmed_routing,ambiguous,label_issue}
                        Human assessment, saved with --case and --note; never
                        relabels training data
  --limit LIMIT
  --per-pair PER_PAIR   Maximum representative examples per confusion pair
  --export EXPORT       Write the prioritised queue to a private Markdown (.md)
                        file
  --since SINCE         Include cases observed on/after YYYY-MM-DD
  --analyse, --analyze  Save evidence-based mistake analyses; cases remain
                        pending human review
  --predicted PREDICTED
                        Optionally analyse only suggestions for this exact
                        outcome label";
const VERDICTS: [&str; 3] = ["confirmed_routing", "ambiguous", "label_issue"];
const QUEUE_HEADER: &str = "# Triage mistake review queue\n";

#[derive(Debug, Default)]
struct ReviewArgs {
    case: Option<String>,
    note: Option<String>,
    verdict: Option<String>,
    limit: i64,
    per_pair: i64,
    export: Option<String>,
    since: String,
    analyse: bool,
    predicted: Option<String>,
}

enum Parsed {
    Args(ReviewArgs),
    Help,
}

fn parse_review(args: &[String]) -> Result<Parsed, String> {
    const OPTIONS: [&str; 11] = [
        "--help",
        "--case",
        "--note",
        "--verdict",
        "--limit",
        "--per-pair",
        "--export",
        "--since",
        "--analyse",
        "--analyze",
        "--predicted",
    ];
    let mut out = ReviewArgs { limit: 20, per_pair: 2, ..ReviewArgs::default() };
    let mut i = 0;
    let mut unknown = Vec::new();
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "-h" {
            return Ok(Parsed::Help);
        }
        if !arg.starts_with("--") || arg == "--" {
            unknown.push(arg.clone());
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v.to_owned())),
            None => (arg.as_str(), None),
        };
        let matches: Vec<&str> = if OPTIONS.contains(&name) {
            vec![name]
        } else {
            OPTIONS.iter().copied().filter(|o| o.starts_with(name)).collect()
        };
        let option = match matches.as_slice() {
            [one] => *one,
            [] => {
                unknown.push(arg.clone());
                continue;
            }
            many if many.iter().all(|m| m.starts_with("--analy")) => "--analyse",
            many => {
                return Err(format!("ambiguous option: {name} could match {}", many.join(", ")));
            }
        };
        match option {
            "--help" => return Ok(Parsed::Help),
            "--analyse" | "--analyze" => {
                if inline.is_some() {
                    return Err(format!(
                        "argument --analyse/--analyze: ignored explicit argument '{}'",
                        inline.unwrap_or_default()
                    ));
                }
                out.analyse = true;
            }
            _ => {
                let value = match inline {
                    Some(v) => v,
                    None if i < args.len() && !args[i].starts_with("--") => {
                        i += 1;
                        args[i - 1].clone()
                    }
                    None => return Err(format!("argument {option}: expected one argument")),
                };
                let int = |v: &str| {
                    v.trim().parse::<i64>().map_err(|_| format!("argument {option}: invalid int value: '{v}'"))
                };
                match option {
                    "--case" => out.case = Some(value),
                    "--note" => out.note = Some(value),
                    "--verdict" => {
                        if !VERDICTS.contains(&value.as_str()) {
                            return Err(format!(
                                "argument --verdict: invalid choice: '{value}' (choose from 'confirmed_routing', \
                                 'ambiguous', 'label_issue')"
                            ));
                        }
                        out.verdict = Some(value);
                    }
                    "--limit" => out.limit = int(&value)?,
                    "--per-pair" => out.per_pair = int(&value)?,
                    "--export" => out.export = Some(value),
                    "--since" => out.since = value,
                    _ => out.predicted = Some(value),
                }
            }
        }
    }
    if !unknown.is_empty() {
        return Err(format!("unrecognized arguments: {}", unknown.join(" ")));
    }
    Ok(Parsed::Args(out))
}

/// A JSON value as a hashable key (Python compares the values themselves).
fn vkey(v: Option<&Value>) -> String {
    v.map_or_else(|| "null".into(), Value::to_string)
}

fn iso_now() -> String {
    let now = Local::now();
    if now.timestamp_subsec_micros() == 0 {
        now.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        now.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    }
}

/// Mistakes from the routing log (and its rotated generation).
pub fn cases_from_log(log: &Path) -> IndexMap<String, Map<String, Value>> {
    let mut cases = IndexMap::new();
    let mut old = log.as_os_str().to_owned();
    old.push(".old");
    for source in [PathBuf::from(old), log.to_path_buf()] {
        let Ok(bytes) = fs::read(&source) else { continue };
        for line in String::from_utf8_lossy(&bytes).lines() {
            let Ok(Value::Object(row)) = serde_json::from_str::<Value>(line) else { continue };
            if row.get("event").and_then(Value::as_str) == Some("routed")
                && truthy(row.get("predicted"))
                && row.get("predictionCorrect") == Some(&Value::Bool(false))
            {
                let when = row.get("when").or_else(|| row.get("ts")).map(py_str).unwrap_or_default();
                let key = format!("{}@{when}", row.get("key").map(py_str).unwrap_or_default());
                let mut case = row.clone();
                case.insert("actual".into(), row.get("dest").cloned().unwrap_or(Value::Null));
                cases.insert(key, case);
            }
        }
    }
    cases
}

type Priority = (f64, String, String);
type Case<'a> = (&'a String, &'a Map<String, Value>);

fn priority(key: &str, row: &Map<String, Value>) -> Priority {
    let confidence = match row.get("confidence") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(-1.0),
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        _ => -1.0,
    };
    let stamp = [row.get("ts"), row.get("when")]
        .into_iter()
        .flatten()
        .find(|v| triage_engine::text::truthy(v))
        .map(py_str)
        .unwrap_or_default();
    (confidence, stamp, key.to_owned())
}

fn cmp_priority(a: &Priority, b: &Priority) -> std::cmp::Ordering {
    a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2))
}

/// Frequent confusions first; diverse, high-confidence examples per pair.
pub fn prioritised_cases(
    cases: &IndexMap<String, Map<String, Value>>,
    limit: i64,
    per_pair: i64,
) -> Vec<(String, Map<String, Value>, usize)> {
    let mut groups: IndexMap<(String, String), Vec<Case<'_>>> = IndexMap::new();
    for (key, case) in cases {
        groups.entry((vkey(case.get("predicted")), vkey(case.get("actual")))).or_default().push((key, case));
    }
    let mut ordered: Vec<(usize, Priority, Vec<Case<'_>>)> = groups
        .into_values()
        .map(|items| {
            let best = items
                .iter()
                .map(|(k, r)| priority(k, r))
                .reduce(|a, b| if cmp_priority(&b, &a).is_gt() { b } else { a })
                .expect("groups are never empty");
            (items.len(), best, items)
        })
        .collect();
    // Python's sorted(reverse=True) is a stable descending sort.
    ordered.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| cmp_priority(&b.1, &a.1)));
    let mut selected = Vec::new();
    for (count, _, mut items) in ordered {
        items.sort_by(|a, b| cmp_priority(&priority(b.0, b.1), &priority(a.0, a.1)));
        let mut titles = BTreeSet::new();
        let mut chosen = 0;
        for (key, row) in items {
            let title = match row.get("summary") {
                Some(v) if triage_engine::text::truthy(v) => py_str(v),
                _ => key.clone(),
            }
            .trim()
            .to_lowercase();
            if !titles.insert(title) {
                continue;
            }
            selected.push((key.clone(), row.clone(), count));
            chosen += 1;
            if chosen >= per_pair {
                break;
            }
        }
    }
    selected.truncate(limit.max(0) as usize);
    selected
}

fn clean(value: Option<&Value>) -> String {
    let text = match value {
        None | Some(Value::Null) => "unknown".into(),
        Some(Value::String(s)) if s.is_empty() => "unknown".into(),
        Some(v) => py_str(v),
    };
    let mut result = text.replace(['\n', '\r'], " ").replace('<', "&lt;").replace('>', "&gt;");
    for c in "\\`*_{}[]()#!|".chars() {
        result = result.replace(c, &format!("\\{c}"));
    }
    result
}

/// The private Markdown worksheet (`TRIAGE_REVIEW_QUEUE.md`).
pub fn review_markdown(selected: &[(String, Map<String, Value>, usize)], pending_count: usize) -> String {
    let mut lines: Vec<String> = vec![
        "# Triage mistake review queue".into(),
        String::new(),
        format!("Generated {}; {pending_count} pending mistakes.", Local::now().format("%Y-%m-%d %H:%M:%S")),
        String::new(),
        "Frequent confusion pairs first, then higher-confidence examples. Repeated titles are sampled once per pair. \
         Counts are historical events, not unique tickets. This file contains private ticket data; do not share or \
         commit it."
            .into(),
        String::new(),
        "Human checklist: confirm the recorded destination reflects the intended routing; check whether intake \
         information was sufficient; flag ambiguous or incorrect labels. Automated notes are not human decisions and \
         never change training labels."
            .into(),
        String::new(),
        "Record your conclusion locally with `python triage_review.py --case CASE_ID --note \"Your explanation\" \
         --verdict confirmed_routing` (alternatives: `ambiguous`, `label_issue`)."
            .into(),
        String::new(),
    ];
    let empty = Map::new();
    for (key, row, count) in selected {
        let analysis = row.get("analysis").and_then(Value::as_object).unwrap_or(&empty);
        let facts = analysis.get("observedFacts").and_then(Value::as_object).unwrap_or(&empty);
        let key_value = Value::String(key.clone());
        let observed = [row.get("ts"), row.get("when")].into_iter().flatten().find(|v| triage_engine::text::truthy(v));
        lines.extend([
            format!("## {}", clean(Some(&key_value))),
            String::new(),
            format!(
                "- Suggestion: {}; observed destination: {}.",
                clean(row.get("predicted")),
                clean(row.get("actual"))
            ),
            format!("- Confidence: {}; request type: {}.", clean(row.get("confidence")), clean(row.get("requestType"))),
            format!("- Observed: {}; {count} mistakes share this confusion pair.", clean(observed)),
            format!("- Summary: {}", clean(row.get("summary"))),
            format!("- Feature provenance: {}.", clean(facts.get("featureOrigin"))),
        ]);
        if truthy(analysis.get("note")) {
            lines.push(format!("- Automated note (awaiting human confirmation): {}", clean(analysis.get("note"))));
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

/// This installation's observations, without interpreting names.
pub struct Evidence {
    pub request_routes: IndexMap<String, IndexMap<String, u64>>,
    pub pairs: HashMap<(String, String), u64>,
}

pub fn analysis_evidence(history: &[Value], cases: &IndexMap<String, Map<String, Value>>) -> Evidence {
    let mut request_routes: IndexMap<String, IndexMap<String, u64>> = IndexMap::new();
    for row in history {
        let request_type = row.get("requestType").or_else(|| row.get("reqType"));
        if let (Some(rt), Some(dest)) =
            (request_type.filter(|v| truthy(Some(v))), row.get("dest").filter(|v| truthy(Some(v))))
        {
            *request_routes.entry(py_str(rt)).or_default().entry(py_str(dest)).or_insert(0) += 1;
        }
    }
    let mut pairs = HashMap::new();
    for case in cases.values() {
        *pairs.entry((vkey(case.get("predicted")), vkey(case.get("actual")))).or_insert(0) += 1;
    }
    Evidence { request_routes, pairs }
}

/// Reproducible routing facts and a follow-up, never a new label.
pub fn analyse_case(
    case: &Map<String, Value>,
    history: &Map<String, Value>,
    evidence: Option<&Evidence>,
) -> Map<String, Value> {
    let request_type = history
        .get("requestType")
        .or_else(|| history.get("reqType"))
        .or_else(|| case.get("requestType"))
        .cloned()
        .unwrap_or(json!(""));
    let actual = case.get("actual").cloned().unwrap_or(Value::Null);
    let guess = case.get("predicted").cloned().unwrap_or(Value::Null);
    let origin = history.get("featureOrigin").cloned().unwrap_or(json!("unrecorded"));
    let mut facts = Map::new();
    facts.insert("predicted".into(), guess.clone());
    facts.insert("actual".into(), actual.clone());
    facts.insert("requestType".into(), request_type.clone());
    facts.insert("confidence".into(), case.get("confidence").cloned().unwrap_or(Value::Null));
    facts.insert("featureOrigin".into(), origin.clone());
    let mut focus =
        format!("The recorded suggestion was {}; the observed destination was {}.", py_str(&guess), py_str(&actual));
    if let Some(evidence) = evidence {
        let counts = request_type
            .as_str()
            .filter(|s| !s.is_empty())
            .and_then(|rt| evidence.request_routes.get(rt))
            .filter(|c| !c.is_empty());
        if let Some(counts) = counts {
            facts.insert("requestTypeOutcomes".into(), json!(counts));
            focus.push_str(&format!(
                " This request type has {} historical routing observations across {} outcomes.",
                counts.values().sum::<u64>(),
                counts.len()
            ));
        }
        let pair_count = evidence.pairs.get(&(vkey(Some(&guess)), vkey(Some(&actual)))).copied().unwrap_or(0);
        if pair_count > 0 {
            facts.insert("matchingMistakes".into(), json!(pair_count));
            focus.push_str(&format!(
                " This suggestion/destination pair occurs in {pair_count} mistakes in the selected review window."
            ));
        }
    }
    focus.push_str(
        " Compare the original intake features with both outcomes; these retrospective counts do not establish the cause \
         or a routing rule.",
    );
    if origin != json!("intake") {
        focus.push_str(" Original intake features are not confirmed, so later ticket edits may affect comparisons.");
    }
    let mut out = Map::new();
    out.insert("source".into(), json!("automated_log_analysis"));
    out.insert("requiresHumanReview".into(), json!(true));
    out.insert("observedFacts".into(), Value::Object(facts));
    out.insert("note".into(), json!(focus));
    out
}

fn usage_error(err: &mut dyn Write, message: &str) -> i32 {
    let _ = writeln!(err, "{USAGE}\n{PROG}: error: {message}");
    2
}

pub fn review_with(root: &Path, args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match review_inner(root, args, out, err) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            1
        }
    }
}

fn review_inner(root: &Path, args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let args = match parse_review(args) {
        Ok(Parsed::Args(a)) => a,
        Ok(Parsed::Help) => {
            w(out, format!("{USAGE}\n{HELP}"))?;
            return Ok(0);
        }
        Err(message) => return Ok(usage_error(err, &message)),
    };
    if args.per_pair < 1 {
        return Ok(usage_error(err, "--per-pair must be at least 1"));
    }
    let paths = Paths::new(root);
    let mut notes: Map<String, Value> = match fs::read(&paths.reviews) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(m)) => m,
            Ok(_) => return Err(format!("{} does not hold a JSON object", paths.reviews.display())),
            Err(e) => return Err(format!("{} is not valid JSON: {e}", paths.reviews.display())),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(e.to_string()),
    };
    let mut cases = cases_from_log(&paths.log);
    if paths.state.exists() {
        let state = read_object(&paths.state);
        for row in state.get("reviewCases").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object)
        {
            let when = row.get("when").cloned().unwrap_or(Value::Null);
            let key = format!("{}@{}", row.get("key").map(py_str).unwrap_or_default(), py_str(&when));
            cases.entry(key).or_insert_with(|| {
                let mut r = row.clone();
                r.insert("ts".into(), when);
                r
            });
        }
    }
    let note_of =
        |notes: &Map<String, Value>, key: &str, field: &str| notes.get(key).and_then(|n| n.get(field)).cloned();
    if args.case.is_some() || args.note.is_some() || args.verdict.is_some() {
        let known = args.case.as_ref().is_some_and(|c| cases.contains_key(c));
        let note = args.note.clone().unwrap_or_default();
        if !known || note.is_empty() {
            return Ok(usage_error(err, "Supply a known --case ID and a non-empty --note"));
        }
        let case = args.case.clone().unwrap_or_default();
        let mut existing = notes.get(&case).and_then(Value::as_object).cloned().unwrap_or_default();
        existing.insert("note".into(), json!(note));
        existing.insert("at".into(), json!(iso_now()));
        existing.insert("source".into(), json!("human"));
        if let Some(verdict) = &args.verdict {
            existing.insert("verdict".into(), json!(verdict));
        }
        notes.insert(case, Value::Object(existing));
        write_json(&paths.reviews, &Value::Object(notes.clone()), false).map_err(|e| e.to_string())?;
        w(out, "Annotation saved locally.")?;
    }
    if !args.since.is_empty() {
        if chrono::NaiveDate::parse_from_str(&args.since, "%Y-%m-%d").is_err() {
            return Ok(usage_error(err, "--since must be YYYY-MM-DD"));
        }
        cases.retain(|_, v| {
            let stamp = v.get("ts").or_else(|| v.get("when")).map(py_str).unwrap_or_default();
            prefix(&stamp, 10) >= args.since
        });
    }
    if args.analyse {
        let records = load_history(&paths.history, &paths.error_log).unwrap_or_default();
        let mut history: HashMap<(String, String), &Map<String, Value>> = HashMap::new();
        for r in records.iter().filter_map(Value::as_object) {
            history.insert((vkey(r.get("key")), vkey(r.get("when"))), r);
        }
        let evidence = analysis_evidence(&records, &cases);
        let empty = Map::new();
        let mut count = 0;
        for (key, case) in &cases {
            let filtered = args.predicted.as_ref().is_some_and(|p| case.get("predicted") != Some(&json!(p)));
            if filtered || truthy(note_of(&notes, key, "note").as_ref()) {
                continue;
            }
            let mut saved = notes.get(key).and_then(Value::as_object).cloned().unwrap_or_default();
            let row = history.get(&(vkey(case.get("key")), vkey(case.get("when")))).copied().unwrap_or(&empty);
            let mut analysis = analyse_case(case, row, Some(&evidence));
            analysis.insert("at".into(), json!(iso_now()));
            saved.insert("analysis".into(), Value::Object(analysis));
            notes.insert(key.clone(), Value::Object(saved));
            count += 1;
        }
        write_json(&paths.reviews, &Value::Object(notes.clone()), false).map_err(|e| e.to_string())?;
        w(out, format!("Saved {count} automated analyses; human review is still pending."))?;
    }
    let mut pending: IndexMap<String, Map<String, Value>> = IndexMap::new();
    for (k, v) in &cases {
        if truthy(note_of(&notes, k, "note").as_ref()) {
            continue;
        }
        let mut row = v.clone();
        row.insert("analysis".into(), note_of(&notes, k, "analysis").unwrap_or(Value::Null));
        pending.insert(k.clone(), row);
    }
    let mut pairs: IndexMap<(String, String), (String, String, u64)> = IndexMap::new();
    for v in pending.values() {
        let entry = pairs.entry((vkey(v.get("predicted")), vkey(v.get("actual")))).or_insert_with(|| {
            let show = |k: &str| v.get(k).map(py_str).unwrap_or_else(|| "None".into());
            (show("predicted"), show("actual"), 0)
        });
        entry.2 += 1;
    }
    let rows = notes.values().filter_map(Value::as_object);
    let human = rows.clone().filter(|r| truthy(r.get("note"))).count();
    let automated = rows.filter(|r| truthy(r.get("analysis"))).count();
    w(
        out,
        format!("{} unreviewed mistakes; {human} human annotations; {automated} automated analyses", pending.len()),
    )?;
    let mut common: Vec<_> = pairs.values().collect();
    common.sort_by_key(|(_, _, n)| std::cmp::Reverse(*n));
    for (guess, actual, n) in common.into_iter().take(8) {
        w(out, format!("  {guess} -> {actual}: {n}"))?;
    }
    let selected = prioritised_cases(&pending, args.limit, args.per_pair);
    if let Some(export) = &args.export {
        let target = std::path::absolute(export).map_err(|e| e.to_string())?;
        if target.extension().is_none_or(|e| !e.eq_ignore_ascii_case("md")) {
            return Ok(usage_error(err, "--export must name a Markdown (.md) file"));
        }
        if target.exists() {
            let current = fs::read(&target).map_err(|e| e.to_string())?;
            if !String::from_utf8_lossy(&current).replace("\r\n", "\n").starts_with(QUEUE_HEADER) {
                return Ok(usage_error(err, "Refusing to overwrite a file that is not a generated review queue"));
            }
        }
        fs::write(&target, review_markdown(&selected, pending.len())).map_err(|e| e.to_string())?;
        w(out, format!("Private review queue saved to {}; do not share or commit it.", target.display()))?;
    }
    for (key, row, count) in &selected {
        let show = |k: &str, default: &str| row.get(k).map(py_str).unwrap_or_else(|| default.to_owned());
        w(
            out,
            format!(
                "\n{key}\n  {} -> {}\n  {}",
                show("predicted", "None"),
                show("actual", "None"),
                prefix(&show("summary", ""), 160)
            ),
        )?;
        w(out, format!("  Confidence: {}; {count} mistakes share this pair", show("confidence", "unknown")))?;
        if let Some(Value::Object(analysis)) = note_of(&notes, key, "analysis").filter(|a| truthy(Some(a))) {
            w(
                out,
                format!("  Analysis (awaiting human review): {}", analysis.get("note").map(py_str).unwrap_or_default()),
            )?;
        }
    }
    Ok(0)
}
