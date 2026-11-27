//! Every Jira URL the overlay opens, built only from the configured domain
//! and percent-encoded values (keys, ids, JQL), so nothing Jira or the
//! learner sends can change which site or page is opened.

use jo_core::config::{self, Config};
use jo_core::jql::path_segment;

fn domain_url(cfg: &Config, path: &str) -> String {
    format!("https://{}{path}", config::get_str(cfg, "domain", ""))
}

/// One ticket: `/browse/KEY`.
pub fn ticket(cfg: &Config, key: &str) -> String {
    domain_url(cfg, &format!("/browse/{}", path_segment(key)))
}

/// An issue search for `jql`.
pub fn jql(cfg: &Config, jql: &str) -> String {
    domain_url(cfg, &format!("/issues/?jql={}", quote_all(jql)))
}

/// One service-desk queue, or the desk's queue list without a project key.
pub fn queue(cfg: &Config, queue_id: &str) -> String {
    match config::get_str(cfg, "projectKey", "") {
        "" => domain_url(cfg, "/jira/servicedesk"),
        pk => domain_url(
            cfg,
            &format!("/jira/servicedesk/projects/{}/queues/custom/{}", path_segment(pk), path_segment(queue_id)),
        ),
    }
}

/// The service desk's queues page.
pub fn desk(cfg: &Config) -> String {
    match config::get_str(cfg, "projectKey", "") {
        "" => domain_url(cfg, "/jira/servicedesk"),
        pk => domain_url(cfg, &format!("/jira/servicedesk/projects/{}/queues", path_segment(pk))),
    }
}

/// `urllib.parse.quote(text, safe="")`.
pub fn quote_all(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn links_cannot_be_steered() {
        let cfg = json!({"domain": "e.atlassian.net", "projectKey": "SD"}).as_object().unwrap().clone();
        assert_eq!(ticket(&cfg, "SD-1"), "https://e.atlassian.net/browse/SD-1");
        assert_eq!(ticket(&cfg, "../../evil?x=1#"), "https://e.atlassian.net/browse/..%2F..%2Fevil%3Fx%3D1%23");
        assert_eq!(ticket(&cfg, "//evil.com"), "https://e.atlassian.net/browse/%2F%2Fevil.com");
        assert_eq!(
            queue(&cfg, "7/../x"),
            "https://e.atlassian.net/jira/servicedesk/projects/SD/queues/custom/7%2F..%2Fx"
        );
        assert_eq!(desk(&cfg), "https://e.atlassian.net/jira/servicedesk/projects/SD/queues");
        assert_eq!(jql(&cfg, "a = \"b\" & c"), "https://e.atlassian.net/issues/?jql=a%20%3D%20%22b%22%20%26%20c");
        assert_eq!(quote_all("SD-1_x.y~"), "SD-1_x.y~");
    }
}
