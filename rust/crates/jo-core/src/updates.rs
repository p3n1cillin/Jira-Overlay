//! Notify-only update check against public GitHub releases.
//!
//! Never downloads or runs anything: a newer release is announced and the
//! user may open its release page. Requests carry no Jira credentials or
//! cookies, follow only HTTPS redirects to an allowlist of GitHub hosts and
//! are size-capped.

use std::fs;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::NaiveDate;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::atomic::{WriteOptions, canonical, inside_file, to_json_line, write_atomic};
use crate::{REPOSITORY, VERSION};

pub const UPDATE_STATE: &str = "app_update_status.json";
const HOSTS: [&str; 5] = [
    "api.github.com",
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
    "github-releases.githubusercontent.com",
];
const RELEASE_LIMIT: usize = 512 * 1024;

static VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v?\d{4}\.\d{2}\.\d{2}(?:\.\d+)?$").expect("valid regex"));
static REPO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").expect("valid regex"));
static DIGEST_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^sha256:[0-9a-f]{64}$").expect("valid regex"));

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct UpdateError(pub String);

fn err(msg: &str) -> UpdateError {
    UpdateError(msg.to_owned())
}

/// `YYYY.MM.DD[.N]` (optional leading `v`) as a comparable 4-tuple.
pub fn version_key(value: &str) -> Result<[u32; 4], UpdateError> {
    if !VERSION_RE.is_match(value) {
        return Err(err("Unsupported release version"));
    }
    let parts: Vec<u32> = value
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().map_err(|_| err("Unsupported release version")))
        .collect::<Result<_, _>>()?;
    NaiveDate::from_ymd_opt(parts[0] as i32, parts[1], parts[2]).ok_or_else(|| err("Invalid release date"))?;
    let mut key = [0; 4];
    key[..parts.len()].copy_from_slice(&parts);
    Ok(key)
}

/// Is `url` an HTTPS URL on an allowlisted GitHub host, default port, no
/// credentials?
pub fn safe_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else { return false };
    parsed.scheme() == "https"
        && parsed.host_str().is_some_and(|h| HOSTS.contains(&h))
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.port().is_none_or(|p| p == 443)
}

/// A minimal HTTP GET for the update check, injectable for tests.
pub trait Fetch {
    /// `Ok(None)` for HTTP 404; other non-success statuses are errors.
    fn get(&self, url: &str, limit: usize) -> Result<Option<Vec<u8>>, UpdateError>;
}

/// The real GitHub client: no auth, no cookies, allowlisted redirects.
pub struct GitHub {
    client: reqwest::blocking::Client,
}

impl GitHub {
    pub fn new() -> Result<Self, UpdateError> {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 5 || !safe_url(attempt.url().as_str()) {
                attempt.error("Untrusted update URL")
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::blocking::Client::builder()
            .redirect(policy)
            .timeout(Duration::from_secs(20))
            .user_agent(format!("JiraOverlay/{VERSION}"))
            .build()
            .map_err(|e| UpdateError(e.to_string()))?;
        Ok(Self { client })
    }
}

impl Fetch for GitHub {
    fn get(&self, url: &str, limit: usize) -> Result<Option<Vec<u8>>, UpdateError> {
        if !safe_url(url) {
            return Err(err("Untrusted update URL"));
        }
        let response = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .map_err(|e| UpdateError(format!("GitHub update check failed ({e}); try again later")))?;
        if !safe_url(response.url().as_str()) {
            return Err(err("Untrusted update URL"));
        }
        let status = response.status().as_u16();
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(UpdateError(format!("GitHub update check failed (HTTP {status}); try again later")));
        }
        if response.content_length().is_some_and(|n| n as usize > limit) {
            return Err(err("Update download exceeds the size limit"));
        }
        use std::io::Read;
        let mut data = Vec::new();
        response.take(limit as u64 + 1).read_to_end(&mut data).map_err(|e| UpdateError(e.to_string()))?;
        if data.len() > limit {
            return Err(err("Update download exceeds the size limit"));
        }
        Ok(Some(data))
    }
}

/// A newer published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub page: String,
    pub installer: Option<(String, String)>,
}

fn verified_asset(assets: &[Value], repository: &str, tag: &str, name: &str) -> Result<Option<(String, String)>, ()> {
    let found: Vec<&Map<String, Value>> = assets
        .iter()
        .filter_map(Value::as_object)
        .filter(|a| a.get("name").and_then(Value::as_str) == Some(name))
        .collect();
    match found.as_slice() {
        [] => Ok(None),
        [asset] => {
            let expected = format!("https://github.com/{repository}/releases/download/{tag}/{name}");
            let digest = asset.get("digest").and_then(Value::as_str).unwrap_or_default();
            if asset.get("browser_download_url").and_then(Value::as_str) == Some(&expected)
                && DIGEST_RE.is_match(digest)
            {
                Ok(Some((expected, digest[7..].to_owned())))
            } else {
                Err(())
            }
        }
        _ => Err(()),
    }
}

/// The newest stable release if it is newer than `current`.
pub fn latest_release(fetch: &dyn Fetch, repository: &str, current: &str) -> Result<Option<Release>, UpdateError> {
    if !REPO_RE.is_match(repository) {
        return Err(err("Invalid update repository"));
    }
    let Some(raw) = fetch.get(&format!("https://api.github.com/repos/{repository}/releases/latest"), RELEASE_LIMIT)?
    else {
        return Ok(None);
    };
    let release: Value = serde_json::from_slice(&raw).map_err(|_| err("GitHub returned an unreadable release"))?;
    let Some(release) = release.as_object() else { return Ok(None) };
    let flag = |k: &str| release.get(k).is_some_and(|v| v.as_bool().unwrap_or(!v.is_null()));
    if flag("draft") || flag("prerelease") {
        return Ok(None);
    }
    let tag = release.get("tag_name").and_then(Value::as_str).unwrap_or_default();
    if version_key(tag)? <= version_key(current)? {
        return Ok(None);
    }
    let version = tag.trim_start_matches('v').to_owned();
    let assets = release.get("assets").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let (url, sha256) = match verified_asset(assets, repository, tag, &format!("JiraOverlay-{version}.zip")) {
        Ok(Some(found)) => found,
        Ok(None) => return Err(err("This release has no complete application package")),
        Err(()) => return Err(err("GitHub did not provide a verifiable package; use the release page")),
    };
    let installer = verified_asset(assets, repository, tag, &format!("JiraOverlay-{version}-Setup.exe"))
        .map_err(|()| err("GitHub did not provide a verifiable installer"))?;
    Ok(Some(Release {
        page: format!("https://github.com/{repository}/releases/tag/{tag}"),
        version,
        url,
        sha256,
        installer,
    }))
}

/// The page a confirmed manual update opens: built from the configured
/// repository and a validated version, never from a response URL.
pub fn release_page(version: &str) -> Result<String, UpdateError> {
    version_key(version)?;
    Ok(format!("https://github.com/{REPOSITORY}/releases/tag/v{version}"))
}

/// A Git checkout or organised source tree (never updated by installers).
pub fn source_checkout(root: &Path) -> bool {
    root.join(".git").exists() || root.join("app").join("app_release.py").is_file()
}

/// An installer-managed installation of the native build.
pub fn native_install(root: &Path) -> bool {
    root.join("unins000.exe").is_file() && root.join("JiraOverlay.exe").is_file()
}

/// Bounded, non-secret record of the last update attempt.
pub fn update_state(root: &Path) -> Map<String, Value> {
    let read = || -> Option<Map<String, Value>> {
        let root = canonical(root).ok()?;
        let path = inside_file(&root, UPDATE_STATE).ok()?;
        if fs::metadata(&path).ok()?.len() > 16384 {
            return None;
        }
        let text = fs::read_to_string(&path).ok()?;
        let value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
        let map = value.as_object()?.clone();
        version_key(map.get("version")?.as_str()?).ok()?;
        let attempted = map.get("attemptedAt")?;
        if attempted.is_boolean() || !attempted.as_f64().is_some_and(f64::is_finite) {
            return None;
        }
        Some(map)
    };
    read().unwrap_or_default()
}

pub fn record_update(root: &Path, version: &str, phase: &str, detail: &str, notified: bool) -> Result<(), UpdateError> {
    version_key(version)?;
    let previous = update_state(root);
    let now = crate::lifecycle::now_epoch();
    let attempted = if previous.get("version").and_then(Value::as_str) == Some(version) && phase != "downloading" {
        previous.get("attemptedAt").and_then(Value::as_f64).unwrap_or(now)
    } else {
        now
    };
    let truncate = |s: &str| s.chars().take(1500).collect::<String>();
    let value = json!({"schema": 1, "version": version, "attemptedAt": attempted, "phase": phase,
                       "detail": truncate(detail), "notified": notified, "log": ""});
    let root = canonical(root).map_err(|e| UpdateError(e.to_string()))?;
    let path = inside_file(&root, UPDATE_STATE).map_err(|e| UpdateError(e.to_string()))?;
    let data = to_json_line(&value).map_err(|e| UpdateError(e.to_string()))?;
    write_atomic(&path, data.as_bytes(), WriteOptions { prefix: Some(".update-"), sync: true, ..Default::default() })
        .map_err(|e| UpdateError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Canned {
        body: Option<Value>,
        requested: RefCell<Vec<String>>,
    }

    impl Fetch for Canned {
        fn get(&self, url: &str, _limit: usize) -> Result<Option<Vec<u8>>, UpdateError> {
            self.requested.borrow_mut().push(url.into());
            Ok(self.body.as_ref().map(|v| serde_json::to_vec(v).unwrap()))
        }
    }

    fn release(tag: &str, assets: Value) -> Canned {
        Canned { body: Some(json!({"tag_name": tag, "assets": assets})), requested: RefCell::default() }
    }

    fn asset(repo: &str, tag: &str, name: &str) -> Value {
        json!({"name": name, "browser_download_url": format!("https://github.com/{repo}/releases/download/{tag}/{name}"),
               "digest": format!("sha256:{}", "a".repeat(64))})
    }

    #[test]
    fn versions() {
        assert_eq!(version_key("v2026.11.24").unwrap(), [2026, 11, 24, 0]);
        assert_eq!(version_key("2026.10.22.1").unwrap(), [2026, 10, 22, 1]);
        assert!(version_key("2026.02.30").is_err());
        assert!(version_key("1.2.3").is_err());
        assert!(version_key("v2026.11.24-beta").is_err());
    }

    #[test]
    fn newer_release_with_bound_assets() {
        let repo = "owner/repo";
        let fetch = release(
            "v2026.12.01",
            json!([
                asset(repo, "v2026.12.01", "JiraOverlay-2026.12.01.zip"),
                asset(repo, "v2026.12.01", "JiraOverlay-2026.12.01-Setup.exe")
            ]),
        );
        let found = latest_release(&fetch, repo, "2026.11.24").unwrap().unwrap();
        assert_eq!(found.version, "2026.12.01");
        assert_eq!(found.page, "https://github.com/owner/repo/releases/tag/v2026.12.01");
        assert!(found.installer.is_some());
        assert_eq!(fetch.requested.borrow()[0], "https://api.github.com/repos/owner/repo/releases/latest");
    }

    #[test]
    fn rejections() {
        let repo = "owner/repo";
        assert_eq!(latest_release(&release("v2026.11.24", json!([])), repo, "2026.11.24").unwrap(), None);
        let missing = latest_release(&release("v2026.12.01", json!([])), repo, "2026.11.24");
        assert!(missing.unwrap_err().0.contains("no complete application package"));
        let mut wrong = asset(repo, "v2026.12.01", "JiraOverlay-2026.12.01.zip");
        wrong["browser_download_url"] = json!("https://evil.example/x.zip");
        assert!(latest_release(&release("v2026.12.01", json!([wrong])), repo, "2026.11.24").is_err());
        let draft =
            Canned { body: Some(json!({"tag_name": "v2027.01.01", "draft": true})), requested: RefCell::default() };
        assert_eq!(latest_release(&draft, repo, "2026.11.24").unwrap(), None);
        let none = Canned { body: None, requested: RefCell::default() };
        assert_eq!(latest_release(&none, repo, "2026.11.24").unwrap(), None);
        assert!(latest_release(&none, "not a repo", "2026.11.24").is_err());
    }

    #[test]
    fn url_allowlist() {
        assert!(safe_url("https://api.github.com/repos/a/b/releases/latest"));
        assert!(safe_url("https://objects.githubusercontent.com/x"));
        for bad in [
            "http://github.com/x",
            "https://github.com.evil.example/x",
            "https://user@github.com/x",
            "https://github.com:8443/x",
            "file:///c:/x",
        ] {
            assert!(!safe_url(bad), "{bad}");
        }
    }

    #[test]
    fn release_page_is_built_from_configuration() {
        assert_eq!(
            release_page("2026.12.01").unwrap(),
            format!("https://github.com/{REPOSITORY}/releases/tag/v2026.12.01")
        );
        assert!(release_page("../../evil").is_err());
    }

    #[test]
    fn update_state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(update_state(dir.path()).is_empty());
        record_update(dir.path(), "2026.12.01", "failed", "boom", false).unwrap();
        let state = update_state(dir.path());
        assert_eq!(state["phase"], "failed");
        assert_eq!(state["notified"], false);
    }
}
