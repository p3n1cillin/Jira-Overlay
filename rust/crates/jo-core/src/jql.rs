//! JQL and Jira-identifier helpers shared by the overlay and the learner.

use std::sync::LazyLock;

use regex::Regex;

static SAFE_PROJECT_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_]*$").expect("valid regex"));

/// Quote a value as a JQL string literal: `\` then `"` escaped, wrapped in
/// double quotes. `None` becomes `""`.
pub fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A real Jira project key: a letter, then letters, digits or underscore.
/// Project keys are interpolated as JQL identifiers, which have no quoting
/// syntax, so an allowlist is the only defence.
pub fn safe_project_key(key: &str) -> bool {
    SAFE_PROJECT_KEY.is_match(key)
}

/// Reduce whatever the user pasted to a bare Jira host, or `""` if there is
/// none (Python `normalise_domain`).
pub fn normalise_domain(raw: &str) -> String {
    let text = raw.trim().trim_matches('/');
    if text.is_empty() {
        return String::new();
    }
    let owned;
    let text = if text.contains("//") {
        text
    } else {
        owned = format!("//{text}");
        &owned
    };
    let host = netloc(text).rsplit('@').next().unwrap_or_default();
    let host = host.strip_suffix(":443").unwrap_or(host).trim().to_lowercase();
    if !host.contains('.') || host.chars().any(|c| " /\\?#".contains(c)) {
        return String::new();
    }
    host
}

/// `urllib.parse.urlsplit(text).netloc`.
fn netloc(text: &str) -> &str {
    let mut rest = text;
    if let Some(colon) = text.find(':') {
        let scheme = &text[..colon];
        if scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        {
            rest = &text[colon + 1..];
        }
    }
    match rest.strip_prefix("//") {
        Some(after) => {
            let end = after.find(['/', '?', '#']).unwrap_or(after.len());
            &after[..end]
        }
        None => "",
    }
}

/// Percent-encode one URL path segment (issue keys, queue ids) so a value
/// from Jira can never change which page is opened.
pub fn path_segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>().replace('+', "%20")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_escapes_backslash_then_quote() {
        assert_eq!(quote(r#"O"Brien"#), r#""O\"Brien""#);
        assert_eq!(quote(r"a\b"), r#""a\\b""#);
        assert_eq!(quote(r#"x" OR project = Y OR "y"#), r#""x\" OR project = Y OR \"y""#);
        assert_eq!(quote(""), r#""""#);
    }

    #[test]
    fn project_keys() {
        assert!(safe_project_key("SD"));
        assert!(safe_project_key("ITS_2"));
        for bad in ["", "1SD", "SD OR 1=1", "SD;", "S-D", "ΣD"] {
            assert!(!safe_project_key(bad), "{bad}");
        }
    }

    #[test]
    fn domains() {
        for (raw, want) in [
            ("yourco.atlassian.net", "yourco.atlassian.net"),
            ("https://YourCo.atlassian.net/browse/ITS-1", "yourco.atlassian.net"),
            ("  //yourco.atlassian.net/ ", "yourco.atlassian.net"),
            ("https://user:pw@yourco.atlassian.net:443/x", "yourco.atlassian.net"),
            ("yourco.atlassian.net?x=1", "yourco.atlassian.net"),
            ("localhost", ""),
            ("", ""),
            ("https://", ""),
        ] {
            assert_eq!(normalise_domain(raw), want, "{raw}");
        }
    }

    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(path_segment("ITS-12"), "ITS-12");
        assert_eq!(path_segment("../x y"), "..%2Fx%20y");
    }
}
