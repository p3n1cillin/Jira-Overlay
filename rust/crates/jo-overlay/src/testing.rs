//! Test doubles: a fake Jira transport and a recording platform.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use jo_jira::{Request, Response, Transport, TransportError};
use serde_json::Value;

use crate::platform::{Platform, WorkArea};
use crate::poll::MakeTransport;

type Handler = dyn Fn(&Request) -> Result<Response, TransportError> + Send + Sync;

/// Canned Jira: every client built from [`FakeJira::make`] answers with the
/// same handler and records its requests. Never touches the network.
#[derive(Clone)]
pub struct FakeJira {
    handler: Arc<Handler>,
    log: Arc<Mutex<Vec<Request>>>,
    built: Arc<AtomicUsize>,
}

struct FakeTransport {
    handler: Arc<Handler>,
    log: Arc<Mutex<Vec<Request>>>,
}

impl Transport for FakeTransport {
    fn send(&self, request: &Request) -> Result<Response, TransportError> {
        self.log.lock().unwrap().push(request.clone());
        (self.handler)(request)
    }
}

impl FakeJira {
    pub fn new(handler: impl Fn(&Request) -> Result<Response, TransportError> + Send + Sync + 'static) -> Self {
        Self { handler: Arc::new(handler), log: Arc::default(), built: Arc::default() }
    }

    pub fn make(&self) -> MakeTransport {
        let (handler, log, built) = (self.handler.clone(), self.log.clone(), self.built.clone());
        Arc::new(move || {
            built.fetch_add(1, Ordering::SeqCst);
            Box::new(FakeTransport { handler: handler.clone(), log: log.clone() }) as Box<dyn Transport>
        })
    }

    pub fn requests(&self) -> Vec<Request> {
        self.log.lock().unwrap().clone()
    }

    /// How many clients (transports) have been built.
    pub fn clients(&self) -> usize {
        self.built.load(Ordering::SeqCst)
    }
}

pub fn ok_json(value: Value) -> Result<Response, TransportError> {
    Ok(Response { status: 200, retry_after: None, body: serde_json::to_vec(&value).unwrap() })
}

pub fn status(code: u16) -> Result<Response, TransportError> {
    Ok(Response { status: code, retry_after: None, body: b"{}".to_vec() })
}

/// Everything the UI asked the operating system to do.
#[derive(Default)]
pub struct Recorded {
    pub urls: Vec<String>,
    pub opened: Vec<PathBuf>,
    pub toasts: Vec<(String, String)>,
    /// What clicking each toast would open, in toast order.
    pub toast_urls: Vec<Option<String>>,
    pub beeps: usize,
    pub messages: Vec<(String, String)>,
    pub questions: Vec<(String, String)>,
    pub startup: Vec<bool>,
}

/// A platform that records instead of acting.
pub struct FakePlatform {
    pub log: Mutex<Recorded>,
    pub answer: Mutex<bool>,
    pub startup_ok: Mutex<bool>,
    pub startup_on: Mutex<bool>,
    pub open_fails: Mutex<bool>,
    pub work_area: Mutex<Option<WorkArea>>,
}

impl Default for FakePlatform {
    fn default() -> Self {
        Self {
            log: Mutex::default(),
            answer: Mutex::new(false),
            startup_ok: Mutex::new(true),
            startup_on: Mutex::new(false),
            open_fails: Mutex::new(false),
            work_area: Mutex::new(Some(WorkArea { left: 0, top: 0, right: 1920, bottom: 1032 })),
        }
    }
}

impl FakePlatform {
    pub fn with<R>(&self, f: impl FnOnce(&Recorded) -> R) -> R {
        f(&self.log.lock().unwrap())
    }
}

impl Platform for FakePlatform {
    fn open_url(&self, url: &str) -> bool {
        self.log.lock().unwrap().urls.push(url.to_owned());
        true
    }
    fn open_folder(&self, path: &Path) -> std::io::Result<()> {
        if *self.open_fails.lock().unwrap() {
            return Err(std::io::Error::other("isolated failure"));
        }
        self.log.lock().unwrap().opened.push(path.to_path_buf());
        Ok(())
    }
    fn open_document(&self, path: &Path) {
        self.log.lock().unwrap().opened.push(path.to_path_buf());
    }
    fn toast(&self, title: &str, body: &str, open: Option<&str>) {
        let mut log = self.log.lock().unwrap();
        log.toasts.push((title.to_owned(), body.to_owned()));
        log.toast_urls.push(open.map(str::to_owned));
    }
    fn beep(&self) {
        self.log.lock().unwrap().beeps += 1;
    }
    fn info(&self, title: &str, text: &str) {
        self.log.lock().unwrap().messages.push((title.to_owned(), text.to_owned()));
    }
    fn error(&self, title: &str, text: &str) {
        self.log.lock().unwrap().messages.push((title.to_owned(), text.to_owned()));
    }
    fn ask_yes_no(&self, title: &str, text: &str) -> bool {
        self.log.lock().unwrap().questions.push((title.to_owned(), text.to_owned()));
        *self.answer.lock().unwrap()
    }
    fn startup_enabled(&self) -> bool {
        *self.startup_on.lock().unwrap()
    }
    fn set_startup(&self, enable: bool) -> bool {
        self.log.lock().unwrap().startup.push(enable);
        *self.startup_ok.lock().unwrap()
    }
    fn work_area(&self, _x: i32, _y: i32) -> Option<WorkArea> {
        *self.work_area.lock().unwrap()
    }
    fn screen_size(&self) -> (i32, i32) {
        (1920, 1080)
    }
}
