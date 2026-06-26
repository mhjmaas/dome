//! Placeholder-aware header/value redaction.
//!
//! Sensitive header values must never reach the log verbatim. This module owns the
//! scrubber the framer applies as it captures headers: against a case-insensitive
//! sensitive-header denylist it rewrites a value so the real credential can never appear,
//! while preserving *which* dome secret was used.
//!
//! Two outcomes for a sensitive header:
//!
//! - it contains a known dome placeholder → the placeholder is replaced with an attribution
//!   tag `<secret:NAME>`, recording which token was used without exposing its value;
//! - it has no known placeholder (an unrecognized credential) → it collapses to
//!   `<redacted len=N>`, keeping only a length hint.
//!
//! Non-sensitive headers pass through verbatim.
//!
//! The placeholder→secret-name replacement is a standalone function ([`attribute_placeholders`])
//! so the same logic can later scrub request/response bodies once body capture lands.

use std::collections::HashMap;

/// Maps each dome placeholder token (the value the guest sees) to the secret name it stands
/// in for (the env-var key). This is the inverse of the proxy's `name → placeholder` map.
pub type PlaceholderNames = HashMap<String, String>;

/// Case-insensitive denylist of headers whose values may carry credentials. Extensible.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
    "x-amz-security-token",
];

/// Whether a header name is on the sensitive denylist (case-insensitive).
pub fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
}

/// Replace every known dome placeholder in `value` with its `<secret:NAME>` attribution tag.
/// Returns the rewritten string and whether any replacement occurred. Reusable for bodies.
pub fn attribute_placeholders(value: &str, names: &PlaceholderNames) -> (String, bool) {
    let mut out = value.to_string();
    let mut found = false;
    for (placeholder, name) in names {
        if out.contains(placeholder.as_str()) {
            out = out.replace(placeholder.as_str(), &format!("<secret:{name}>"));
            found = true;
        }
    }
    (out, found)
}

/// Scrub one header into a log-safe value. Non-sensitive headers pass through verbatim; a
/// sensitive header is attributed to its dome secret when a placeholder is present, otherwise
/// reduced to a length hint so the real value never appears.
pub fn scrub_header(name: &str, value: &str, names: &PlaceholderNames) -> String {
    if !is_sensitive_header(name) {
        return value.to_string();
    }
    let (attributed, found) = attribute_placeholders(value, names);
    if found {
        attributed
    } else {
        format!("<redacted len={}>", value.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> PlaceholderNames {
        let mut m = PlaceholderNames::new();
        m.insert("dome_tok_abc123".into(), "OPENAI_API_KEY".into());
        m
    }

    #[test]
    fn known_placeholder_in_sensitive_header_is_attributed() {
        let v = scrub_header("Authorization", "Bearer dome_tok_abc123", &names());
        assert_eq!(v, "Bearer <secret:OPENAI_API_KEY>");
    }

    #[test]
    fn unrecognized_sensitive_value_is_length_redacted() {
        let v = scrub_header("Authorization", "Bearer sk-live-9999", &names());
        assert_eq!(v, "<redacted len=19>");
    }

    #[test]
    fn non_sensitive_header_passes_through_verbatim() {
        let v = scrub_header("Host", "api.openai.com", &names());
        assert_eq!(v, "api.openai.com");
        // A placeholder appearing in a non-sensitive header is left untouched, too.
        let v = scrub_header("X-Trace", "dome_tok_abc123", &names());
        assert_eq!(v, "dome_tok_abc123");
    }

    #[test]
    fn header_name_matching_is_case_insensitive() {
        assert!(is_sensitive_header("authorization"));
        assert!(is_sensitive_header("AUTHORIZATION"));
        assert!(is_sensitive_header("Set-Cookie"));
        assert!(!is_sensitive_header("X-Request-Id"));
        // The denylist match is case-insensitive in the scrubber, too.
        let v = scrub_header("COOKIE", "session=sk-unknown", &names());
        assert_eq!(v, "<redacted len=18>");
    }

    #[test]
    fn attribute_placeholders_reports_replacement() {
        let (out, found) = attribute_placeholders("k=dome_tok_abc123&x=1", &names());
        assert_eq!(out, "k=<secret:OPENAI_API_KEY>&x=1");
        assert!(found);
        let (out, found) = attribute_placeholders("nothing here", &names());
        assert_eq!(out, "nothing here");
        assert!(!found);
    }
}
