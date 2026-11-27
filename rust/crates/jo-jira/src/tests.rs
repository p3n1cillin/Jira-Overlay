use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::json;

use super::*;

type Reply = Result<Response, TransportError>;

#[derive(Clone, Default)]
struct Fake {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    seen: Arc<Mutex<Vec<Request>>>,
    repeat_last: Arc<Mutex<Option<Value>>>,
}

impl Fake {
    fn with(replies: Vec<Reply>) -> Self {
        let fake = Self::default();
        *fake.replies.lock().unwrap() = replies.into();
        fake
    }

    fn json_pages(pages: Vec<Value>) -> Self {
        Self::with(pages.into_iter().map(|p| Ok(ok(p))).collect())
    }

    fn repeating(page: Value) -> Self {
        let fake = Self::default();
        *fake.repeat_last.lock().unwrap() = Some(page);
        fake
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl Transport for Fake {
    fn send(&self, request: &Request) -> Result<Response, TransportError> {
        self.seen.lock().unwrap().push(request.clone());
        if let Some(page) = self.repeat_last.lock().unwrap().clone() {
            return Ok(ok(page));
        }
        self.replies.lock().unwrap().pop_front().unwrap_or_else(|| Err(TransportError("no more replies".into())))
    }
}

fn ok(body: Value) -> Response {
    Response { status: 200, retry_after: None, body: body.to_string().into_bytes() }
}

fn status(code: u16) -> Reply {
    Ok(Response { status: code, retry_after: None, body: b"{}".to_vec() })
}

fn client(fake: &Fake) -> Jira {
    Jira::new("example.atlassian.net", "me@example.com", "token", Box::new(fake.clone())).with_stop(Stop::new())
}

/// No sleeping in tests: a pre-set stop would abort, so patch delays by
/// using a Stop that is never set and Retry-After "0".
fn fast(code: u16) -> Reply {
    Ok(Response { status: code, retry_after: Some("0".into()), body: b"{}".to_vec() })
}

#[test]
fn only_get_and_read_only_searches_are_possible() {
    let fake = Fake::with(vec![]);
    let jira = client(&fake);
    for path in ["/rest/api/3/issue", "/rest/api/3/issue/SD-1/transitions", "/rest/api/3/search/jql/../../issue"] {
        let result = jira.send(Method::Post, path, &[], Some(&json!({})), Duration::from_secs(1), Retry::Once);
        assert!(matches!(result, Err(SendError::NotReadOnly(_))), "{path}");
    }
    assert_eq!(fake.calls(), 0, "refused before any request was made");
}

#[test]
fn retry_policy() {
    for (label, replies, attempts, want) in [
        ("first-time success", vec![status(200)], 1, Some(200)),
        ("500 then success", vec![fast(500), status(200)], 2, Some(200)),
        ("429 then success", vec![fast(429), status(200)], 2, Some(200)),
        ("persistent 500", vec![fast(500), fast(500), fast(500), fast(500), fast(500), fast(500)], 5, None),
        ("401 not retried", vec![status(401)], 1, Some(401)),
        ("403 not retried", vec![status(403)], 1, Some(403)),
        ("404 not retried", vec![status(404)], 1, Some(404)),
    ] {
        let fake = Fake::with(replies);
        let jira = client(&fake);
        let got =
            jira.send(Method::Get, "/x", &[], None, Duration::from_secs(1), Retry::Backoff).ok().map(|r| r.status);
        assert_eq!(fake.calls(), attempts, "{label}");
        assert_eq!(got, want, "{label}");
    }
}

#[test]
fn quit_aborts_a_backoff_and_prevents_requests() {
    let fake = Fake::with(vec![status(500); 6]);
    let stop = Stop::new();
    let jira = Jira::new("example.atlassian.net", "e", "t", Box::new(fake.clone())).with_stop(stop.clone());
    let setter = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        setter.set();
    });
    let started = Instant::now();
    assert!(jira.send(Method::Get, "/x", &[], None, Duration::from_secs(1), Retry::Backoff).is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    let calls = fake.calls();
    assert!(jira.get("/x", &[]).is_none());
    assert_eq!(fake.calls(), calls, "no request after quit");
}

#[test]
fn retry_after_parsing() {
    let now = chrono::DateTime::parse_from_rfc2822("Wed, 25 Nov 2026 10:00:00 GMT").unwrap().to_utc();
    assert_eq!(retry_delay("Wed, 25 Nov 2026 10:02:00 GMT", 1.0, now), 120.0);
    assert_eq!(retry_delay("0.5", 1.0, now), 0.5);
    for bad in ["", "garbage", "nan", "inf"] {
        assert_eq!(retry_delay(bad, 2.0, now), 2.0, "{bad}");
    }
    assert_eq!(retry_delay("-5", 2.0, now), 0.0);
    assert_eq!(retry_delay("999999", 2.0, now), 86400.0);
}

#[test]
fn malformed_search_pages_are_never_complete() {
    for page in [
        json!({}),
        json!({"issues": null}),
        json!({"issues": "bad"}),
        json!({"issues": [null]}),
        json!({"issues": [{}]}),
        json!({"issues": [], "isLast": false}),
        json!({"issues": [], "isLast": "true"}),
        json!({"issues": [], "nextPageToken": []}),
    ] {
        let fake = Fake::json_pages(vec![page.clone()]);
        let jira = client(&fake);
        jira.search("q", &[], None);
        assert!(jira.incomplete_reads() > 0, "{page}");
    }
}

#[test]
fn search_pagination() {
    let fake = Fake::json_pages(vec![
        json!({"issues": [{"key": "ANY-1"}], "nextPageToken": "first"}),
        json!({"issues": [{"key": "ANY-1"}], "nextPageToken": "second"}),
    ]);
    let jira = client(&fake);
    assert_eq!(jira.search("q", &[], None).len(), 1);
    assert!(jira.incomplete_reads() > 0, "repeated pages terminate and keep a retry checkpoint");

    let fake = Fake::repeating(json!({"issues": [{"key": "SAMPLE-1"}], "nextPageToken": "more"}));
    let jira = client(&fake);
    assert_eq!(jira.search("q", &[], Some(1)).len(), 1);
    assert_eq!(jira.incomplete_reads(), 0, "a bounded sample is not a failure");

    let fake = Fake::json_pages(vec![json!({"issues": [], "isLast": true, "nextPageToken": ""})]);
    let jira = client(&fake);
    assert!(jira.search("q", &[], None).is_empty());
    assert_eq!(jira.incomplete_reads(), 0);

    let mut pages: Vec<Value> = (0..6)
        .map(|n| {
            json!({"issues": (0..100).map(|i| json!({"key": (n * 100 + i).to_string()})).collect::<Vec<_>>(),
                        "nextPageToken": (n + 1).to_string()})
        })
        .collect();
    pages.push(json!({"issues": [{"key": "last"}], "isLast": true}));
    let fake = Fake::json_pages(pages);
    let jira = client(&fake);
    assert_eq!(jira.search("q", &[], None).len(), 601);
    assert_eq!(jira.incomplete_reads(), 0);

    let fake = Fake::with(vec![
        Ok(ok(json!({"issues": [{"key": "SD-1"}], "nextPageToken": "next"}))),
        Err(TransportError("down".into())),
        Err(TransportError("down".into())),
        Err(TransportError("down".into())),
        Err(TransportError("down".into())),
        Err(TransportError("down".into())),
    ]);
    let stop = Stop::new();
    let jira = Jira::new("e.x", "e", "t", Box::new(fake.clone())).with_stop(stop.clone());
    let setter = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        setter.set();
    });
    assert_eq!(jira.search("q", &[], None).len(), 1);
    assert_eq!(jira.incomplete_reads(), 1, "a partial search keeps fetched work and is flagged");
}

#[test]
fn malformed_changelogs_keep_retry_evidence() {
    for page in [
        json!({"values": null}),
        json!({"values": [], "isLast": false}),
        json!({"values": [{}], "isLast": true}),
        json!({"values": [], "isLast": "true"}),
        json!({"values": [], "startAt": 10}),
    ] {
        let fake = Fake::json_pages(vec![page.clone()]);
        let jira = client(&fake);
        jira.changelog("ARBITRARY-1");
        assert!(jira.incomplete_reads() > 0, "{page}");
    }
    let fake = Fake::repeating(json!({"values": [{"id": "stable-id", "items": []}], "isLast": false}));
    let jira = client(&fake);
    assert_eq!(jira.changelog("OTHER-1").len(), 1);
    assert_eq!(fake.calls(), 2);
    assert!(jira.incomplete_reads() > 0);

    let fake = Fake::json_pages(vec![
        json!({"values": [{"id": "one"}], "total": 2, "startAt": 0}),
        json!({"values": [{"id": "two"}], "total": 2, "startAt": 1}),
    ]);
    let jira = client(&fake);
    assert_eq!(jira.changelog("NEW-9").len(), 2);
    assert_eq!(jira.incomplete_reads(), 0);
}

#[test]
fn refusals_are_logged_once_and_cleared_by_a_search() {
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = lines.clone();
    let fake = Fake::with(vec![status(404), status(401), status(401), Ok(ok(json!({"issues": [], "isLast": true})))]);
    let jira =
        Jira::new("e.x", "e", "t", Box::new(fake.clone())).with_log(move |l| sink.lock().unwrap().push(l.into()));
    jira.get("/rest/api/3/issue/SD-1", &[]);
    assert!(jira.refusals().is_empty(), "a missing ticket is not an outage");
    jira.get("/rest/api/3/field", &[]);
    jira.get("/rest/api/3/field", &[]);
    assert_eq!(jira.refusals(), [401]);
    assert_eq!(lines.lock().unwrap().len(), 1);
    jira.search("q", &[], None);
    assert!(jira.refusals().is_empty());
}

#[test]
fn counts() {
    let fake = Fake::json_pages(vec![json!({"count": 37})]);
    assert_eq!(client(&fake).count("q"), (Some(37), false));

    let fake = Fake::with(vec![status(400)]);
    assert_eq!(client(&fake).count("bad field"), (None, true));
    let fake = Fake::with(vec![status(503)]);
    assert_eq!(client(&fake).count("q"), (None, false), "transient failure never latches");

    let fake = Fake::with(vec![
        status(404),
        Ok(ok(json!({"issues": vec![json!({}); 200], "nextPageToken": "n"}))),
        Ok(ok(json!({"issues": vec![json!({}); 120]}))),
        Ok(ok(json!({"issues": null, "isLast": true}))),
    ]);
    let jira = client(&fake);
    assert_eq!(jira.count("q"), (Some(320), false));
    assert_eq!(jira.count("q"), (Some(0), false), "stays on the fallback; null issues count as 0");
    let bodies: Vec<String> = fake.seen.lock().unwrap().iter().map(|r| r.url.clone()).collect();
    assert!(bodies[0].ends_with("approximate-count") && bodies[3].ends_with("/search/jql"));
}

#[test]
fn credentials_travel_only_in_the_authorization_header() {
    let fake = Fake::json_pages(vec![json!({})]);
    let jira = client(&fake);
    jira.get("/rest/api/3/myself", &[]);
    let request = fake.seen.lock().unwrap()[0].clone();
    assert_eq!(request.url, "https://example.atlassian.net/rest/api/3/myself");
    assert!(request.authorization.starts_with("Basic "));
    assert!(!request.url.contains("token"));
}
