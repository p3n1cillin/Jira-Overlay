//! `triage_review.py` / `triage_eval.py` command lines
//! (`tests/test_adaptive_routes.py` review workflow, plus eval smoke tests).

use std::fs;

use indexmap::IndexMap;
use serde_json::json;

use super::*;
use crate::cli::{eval_with, prioritised_cases, review_markdown, review_with};

fn case(confidence: i64, summary: &str, actual: &str) -> Map<String, Value> {
    obj(json!({"predicted": "Queue Ω", "actual": actual, "confidence": confidence, "summary": summary,
               "ts": "2030-05-20 12:00", "requestType": "Besoin général"}))
}

#[test]
fn review_prioritises_recurring_confusions_and_diverse_examples() {
    let mut cases = IndexMap::new();
    cases.insert("A".to_owned(), case(99, "Repeated title", "Équipe α"));
    cases.insert("B".to_owned(), case(98, "repeated title", "Équipe α"));
    cases.insert("C".to_owned(), case(85, "Different wording", "Équipe α"));
    cases.insert("D".to_owned(), case(100, "Rare confusion", "Other"));
    let selected = prioritised_cases(&cases, 20, 2);
    let keys: Vec<&str> = selected.iter().map(|(k, _, _)| k.as_str()).collect();
    assert_eq!(keys, ["A", "C", "D"]);
    let md = review_markdown(&selected, 4);
    assert!(md.contains("Besoin général") && md.contains("never change training labels"));
    assert!(md.starts_with("# Triage mistake review queue\n\nGenerated "));
    assert!(md.contains("- Observed: 2030-05-20 12:00; 3 mistakes share this confusion pair."));
    let hostile = vec![("X".to_owned(), case(0, "![image](https://example.invalid/)", "Équipe α"), 1)];
    assert!(!review_markdown(&hostile, 1).contains("!["));
    let zero = vec![("X".to_owned(), case(0, "zero", "Équipe α"), 1)];
    assert!(review_markdown(&zero, 1).contains("Confidence: 0;"));
}

#[test]
fn annotations_and_exports_stay_local() {
    let dir = tempfile::tempdir().unwrap();
    let mut row = case(99, "Repeated title", "Équipe α");
    for (k, v) in [
        ("event", json!("routed")),
        ("key", json!("Q-1")),
        ("when", json!("2030-05-20 12:00")),
        ("dest", json!("Équipe α")),
        ("predictionCorrect", json!(false)),
    ] {
        row.insert(k.into(), v);
    }
    fs::write(dir.path().join("triage_log.jsonl"), format!("{}\n", Value::Object(row))).unwrap();
    fs::write(
        dir.path().join("triage_reviews.json"),
        json!({"Q-1@2030-05-20 12:00": {"analysis": {"requiresHumanReview": true}}}).to_string(),
    )
    .unwrap();
    let queue = dir.path().join("queue.md");
    let args: Vec<String> = [
        "--case",
        "Q-1@2030-05-20 12:00",
        "--note",
        "Ambiguous initial wording",
        "--verdict",
        "ambiguous",
        "--export",
        queue.to_str().unwrap(),
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(review_with(dir.path(), &args, &mut out, &mut err), 0, "{}", String::from_utf8_lossy(&err));
    let notes = read_json(&dir.path().join("triage_reviews.json"));
    let note = &notes["Q-1@2030-05-20 12:00"];
    assert_eq!(note["verdict"], json!("ambiguous"));
    assert_eq!(note["source"], json!("human"));
    assert_eq!(note["analysis"]["requiresHumanReview"], json!(true));
    assert_eq!(note["note"], json!("Ambiguous initial wording"));
    assert!(fs::read_to_string(&queue).unwrap().contains("0 pending mistakes"));
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.starts_with(
            "Annotation saved locally.\n0 unreviewed mistakes; 1 human annotations; 1 automated analyses\n"
        ),
        "{text}"
    );

    // Refuses to overwrite something that is not a generated queue.
    let other = dir.path().join("notes.md");
    fs::write(&other, "my own notes").unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = review_with(dir.path(), &["--export".into(), other.to_str().unwrap().into()], &mut out, &mut err);
    assert_eq!(code, 2);
    assert!(
        String::from_utf8_lossy(&err).contains("Refusing to overwrite a file that is not a generated review queue")
    );
    assert_eq!(fs::read_to_string(&other).unwrap(), "my own notes");

    // Unknown case / bad flags are argparse-style usage errors.
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        review_with(dir.path(), &["--case".into(), "nope".into(), "--note".into(), "x".into()], &mut out, &mut err),
        2
    );
    assert!(
        String::from_utf8_lossy(&err)
            .contains("triage_review.py: error: Supply a known --case ID and a non-empty --note")
    );
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(review_with(dir.path(), &["--per-pair".into(), "0".into()], &mut out, &mut err), 2);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(review_with(dir.path(), &["--verdict".into(), "maybe".into()], &mut out, &mut err), 2);
}

#[test]
fn analyse_saves_evidence_without_relabelling() {
    let dir = tempfile::tempdir().unwrap();
    let mut body = String::new();
    for (key, when) in [("Q-1", "2030-05-20 12:00"), ("Q-2", "2030-05-21 12:00")] {
        let mut row = case(90, &format!("title {key}"), "Équipe α");
        for (k, v) in [
            ("event", json!("routed")),
            ("key", json!(key)),
            ("when", json!(when)),
            ("ts", json!(when)),
            ("dest", json!("Équipe α")),
            ("predictionCorrect", json!(false)),
        ] {
            row.insert(k.into(), v);
        }
        body.push_str(&format!("{}\n", Value::Object(row)));
    }
    fs::write(dir.path().join("triage_log.jsonl"), body).unwrap();
    fs::write(
        dir.path().join("triage_history.jsonl"),
        format!(
            "{}\n",
            json!({"key": "Q-1", "when": "2030-05-20 12:00", "dest": "Équipe α", "requestType": "Besoin général",
                               "featureOrigin": "intake", "summary": "x"})
        ),
    )
    .unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let args = vec!["--analyze".to_owned(), "--since".to_owned(), "2030-05-21".to_owned()];
    assert_eq!(review_with(dir.path(), &args, &mut out, &mut err), 0);
    let notes = read_json(&dir.path().join("triage_reviews.json"));
    assert_eq!(notes.as_object().unwrap().len(), 1, "--since filtered the older case");
    let analysis = &notes["Q-2@2030-05-21 12:00"]["analysis"];
    assert_eq!(analysis["observedFacts"]["featureOrigin"], json!("unrecorded"));
    assert!(analysis["at"].is_string());
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("Saved 1 automated analyses; human review is still pending."));
    assert!(text.contains("  Queue Ω -> Équipe α: 1"));
    assert!(text.contains("  Analysis (awaiting human review): The recorded suggestion was Queue Ω"));
}

#[test]
fn eval_commands() {
    let dir = tempfile::tempdir().unwrap();
    let offline = || -> Result<Box<dyn Transport>, String> { Err("offline".into()) };
    let run = |args: &[&str]| {
        let mut out = Vec::new();
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let code = eval_with(dir.path(), &args, &mut out, &offline);
        (code, String::from_utf8(out).unwrap())
    };
    assert_eq!(run(&["tune"]), (0, "At least 300 labelled decisions are required.\n".into()));
    assert_eq!(run(&[]).1, "At least 300 labelled decisions are required.\n");
    assert_eq!(
        run(&["thresholds"]).1,
        "At least 600 labelled decisions are needed for separate gate and final windows.\n"
    );
    assert!(run(&["help"]).1.contains("Offline evaluator/tuner"));
    let mut body = String::new();
    for i in 0..60 {
        body.push_str(&format!(
            "{}\n",
            json!({"key": format!("K-{i}"), "when": format!("2026-09-{:02} 10:00", 1 + i % 28), "summary": if i % 2 == 0 { "server down" } else { "data report" },
                   "dest": if i % 2 == 0 { "IT" } else { "With DATA" }, "cloneProjects": if i % 2 == 0 { json!([]) } else { json!(["DATA"]) }})
        ));
    }
    fs::write(dir.path().join("triage_dataset.jsonl"), body).unwrap();
    let (code, text) = run(&["review"]);
    assert_eq!(code, 0);
    assert!(text.contains("reviewConf reviewMargin |  overall  | firm-cov firm-acc | review-cov"), "{text}");
    assert_eq!(text.lines().filter(|l| l.starts_with("     70 ")).count(), 3, "{text}");
    fs::write(dir.path().join("triage_dataset.jsonl"), "{not json\n").unwrap();
    assert!(run(&["analyze"]).1.contains("is corrupt"));
}

#[test]
fn eval_build_writes_the_dataset_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJira::new();
    let issues: Vec<Value> =
        (0..3).map(|i| json!({"key": format!("HD-{i}"), "fields": {"summary": format!("ticket {i}")}})).collect();
    fake.with(|j| {
        for i in 0..3 {
            j.changelogs.insert(
                format!("HD-{i}"),
                vec![status_change(&format!("c{i}"), if i == 0 { "Passed to Networks" } else { "Closed" })],
            );
        }
    });
    fake.on_search(move |_| issues.clone());
    // Opens through the same constructor as the service.
    drop(watcher(
        dir.path(),
        json!({"triageStatuses": ["Passed to Networks"], "resolvedStatuses": ["Closed"]}),
        None,
        &fake,
    ));
    let connect = {
        let fake = fake.clone();
        move || -> Result<Box<dyn Transport>, String> { Ok(Box::new(fake.clone())) }
    };
    let mut out = Vec::new();
    assert_eq!(eval_with(dir.path(), &["build".into()], &mut out, &connect), 0);
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("project HD: handoffs=['Passed to Networks'] resolved=['Closed'] moves=none\nroutings: 1\nresolved in place: 2\n"), "{text}");
    let rows = read_lines(&dir.path().join("triage_dataset.jsonl"));
    assert_eq!(rows.len(), 3);
    let keys: Vec<&String> = rows[0].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "key",
            "dest",
            "when",
            "summary",
            "desc",
            "labels",
            "components",
            "structured",
            "reporter",
            "reqType",
            "cloneProjects"
        ]
    );
    assert!(fake.queries().iter().all(|q| q.starts_with("project = HD")));
}
