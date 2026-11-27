//! HTTP transport behind the Jira client, swappable for tests.

use std::io::Read;
use std::time::Duration;

use serde_json::Value;

use crate::Method;

/// One prepared request. Credentials travel only in `authorization`.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
    pub authorization: String,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct Response {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

impl Response {
    /// `requests.Response.ok`: any status below 400.
    pub fn ok(&self) -> bool {
        self.status < 400
    }

    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// Connection failure or timeout (retryable).
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

pub trait Transport: Send + Sync {
    fn send(&self, request: &Request) -> Result<Response, TransportError>;
}

/// Response bodies larger than this are refused (Jira pages are far smaller).
const MAX_BODY: u64 = 64 * 1024 * 1024;

/// The real transport: `reqwest` over SChannel, so the Windows certificate
/// store and system proxy settings apply as they do for the user's browser.
pub struct Http {
    client: reqwest::blocking::Client,
}

impl Http {
    pub fn new() -> Result<Self, TransportError> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| TransportError(e.to_string()))?;
        Ok(Self { client })
    }
}

impl Transport for Http {
    fn send(&self, request: &Request) -> Result<Response, TransportError> {
        let url = reqwest::Url::parse_with_params(&request.url, &request.query)
            .map_err(|e| TransportError(format!("invalid Jira URL: {e}")))?;
        let builder = match request.method {
            Method::Get => self.client.get(url),
            Method::Post => self.client.post(url),
        };
        let mut builder = builder
            .header("Authorization", &request.authorization)
            .header("Accept", "application/json")
            .timeout(request.timeout);
        if let Some(body) = &request.body {
            builder = builder.header("Content-Type", "application/json").body(body.to_string());
        }
        let response = builder.send().map_err(|e| TransportError(e.to_string()))?;
        let status = response.status().as_u16();
        let retry_after = response.headers().get("Retry-After").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let mut body = Vec::new();
        response.take(MAX_BODY).read_to_end(&mut body).map_err(|e| TransportError(e.to_string()))?;
        Ok(Response { status, retry_after, body })
    }
}
