use http::{HeaderMap, HeaderName};
use serde_json::Value;
use std::collections::BTreeMap;

pub const REDACTED: &str = "<redacted>";
const NON_UTF8: &str = "<non-utf8>";
const DEFAULT_TRUNCATE_CHARS: usize = 96;

pub fn is_sensitive_header(name: &HeaderName) -> bool {
    is_sensitive_header_name(name.as_str())
}

pub fn is_sensitive_header_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "authorization"
        || lower == "proxy-authorization"
        || lower == "cookie"
        || lower == "set-cookie"
        || lower == crate::headers::LOCAL_CLIENT_KEY_HEADER
        || lower == "chatgpt-account-id"
        || lower == "chatgpt-organization-id"
        || lower.contains("token")
        || lower.contains("secret")
}

pub fn redact_header_value(name: &HeaderName, value: &str) -> String {
    if is_sensitive_header(name) {
        REDACTED.to_owned()
    } else if is_request_id_header(name.as_str()) {
        hash_and_truncate(value)
    } else {
        truncate_for_log(value, DEFAULT_TRUNCATE_CHARS)
    }
}

pub fn redact_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = value.to_str().unwrap_or(NON_UTF8);
            (name.as_str().to_owned(), redact_header_value(name, value))
        })
        .collect()
}

pub fn redact_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    if is_sensitive_json_field(key) {
                        (key.clone(), Value::String(REDACTED.to_owned()))
                    } else {
                        (key.clone(), redact_json(value))
                    }
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_json).collect()),
        _ => value.clone(),
    }
}

pub fn truncate_json_strings(value: &Value, max_chars: usize) -> Value {
    match value {
        Value::String(text) => Value::String(truncate_for_log(text, max_chars)),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), truncate_json_strings(value, max_chars)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| truncate_json_strings(value, max_chars))
                .collect(),
        ),
        _ => value.clone(),
    }
}

pub fn redact_json_string_values(value: &Value) -> Value {
    match value {
        Value::String(_) => Value::String("<redacted-content>".to_owned()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), redact_json_string_values(value)))
                .collect(),
        ),
        Value::Array(values) => {
            Value::Array(values.iter().map(redact_json_string_values).collect())
        }
        _ => value.clone(),
    }
}

pub fn redact_url(raw: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(raw) {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        return url.to_string();
    }

    raw.split(['?', '#'])
        .next()
        .map(|value| truncate_for_log(value, DEFAULT_TRUNCATE_CHARS))
        .unwrap_or_default()
}

pub fn stable_hash_hex(value: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn hash_and_truncate(value: &str) -> String {
    let prefix = value.chars().take(8).collect::<String>();
    if prefix.is_empty() {
        return format!("hash:{}", stable_hash_hex(value));
    }
    format!("hash:{} prefix:{}", stable_hash_hex(value), prefix)
}

pub fn truncate_for_log(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn is_request_id_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "x-request-id" || lower == "x-client-request-id"
}

fn is_sensitive_json_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "authorization"
        || lower == "device_code"
        || lower == "encrypted_content"
        || lower == "access_token"
        || lower == "githubtoken"
        || lower == "copilottoken"
        || lower == "github_token"
        || lower == "copilot_token"
        || lower == crate::headers::LOCAL_CLIENT_KEY_HEADER
        || lower == "chatgpt-account-id"
        || lower == "chatgpt-organization-id"
        || lower == "chatgpt_account_id"
        || lower == "chatgpt_organization_id"
        || lower.contains("token")
        || lower.contains("secret")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::{AUTHORIZATION, CONTENT_TYPE};
    use http::{HeaderName, HeaderValue};
    use serde_json::json;

    #[test]
    fn redacts_sensitive_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer secret-token"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            HeaderName::from_static("x-request-id"),
            HeaderValue::from_static("raw-request-id-that-should-be-hashed"),
        );

        let redacted = redact_headers(&headers);

        assert_eq!(
            redacted.get("authorization"),
            Some(&"<redacted>".to_owned())
        );
        assert_eq!(
            redacted.get("content-type"),
            Some(&"application/json".to_owned())
        );
        assert!(redacted.get("x-request-id").unwrap().starts_with("hash:"));
        assert!(!format!("{redacted:?}").contains("secret-token"));
        assert!(!format!("{redacted:?}").contains("raw-request-id-that-should-be-hashed"));
    }

    #[test]
    fn redacts_mixed_case_sensitive_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("proxy-authorization"),
            HeaderValue::from_static("Bearer proxy-secret"),
        );
        headers.insert(
            HeaderName::from_static("x-copilot-token"),
            HeaderValue::from_static("copilot-secret"),
        );

        let redacted = redact_headers(&headers);

        assert_eq!(
            redacted.get("proxy-authorization"),
            Some(&REDACTED.to_owned())
        );
        assert_eq!(redacted.get("x-copilot-token"), Some(&REDACTED.to_owned()));
        let text = format!("{redacted:?}");
        assert!(!text.contains("proxy-secret"));
        assert!(!text.contains("copilot-secret"));
    }

    #[test]
    fn redacts_recursive_json_secret_fields() {
        let value = json!({
            "githubToken": "github-secret",
            "nested": {
                "copilotToken": "copilot-secret",
                "access_token": "access-secret",
                "device_code": "device-secret",
                "encrypted_content": "encrypted-secret",
                "authorization": "Bearer auth-secret",
                "token": "generic-token-secret",
                "secret": "generic-secret",
                "safe": "visible"
            },
            "array": [{"token": "array-secret"}]
        });

        let redacted = redact_json(&value);
        let text = redacted.to_string();

        assert!(text.contains("visible"));
        for leaked in [
            "github-secret",
            "copilot-secret",
            "access-secret",
            "device-secret",
            "encrypted-secret",
            "auth-secret",
            "generic-token-secret",
            "generic-secret",
            "array-secret",
        ] {
            assert!(!text.contains(leaked), "leaked {leaked}: {text}");
        }
    }

    #[test]
    fn redacts_all_json_string_values_for_content_redacted_mode() {
        let value = json!({
            "model": "gpt-5.5",
            "reasoning": {"effort": "high"},
            "stream": true,
            "count": 2,
            "array": ["hello", {"tool": "calculator"}],
        });

        let redacted = redact_json_string_values(&value);
        let text = redacted.to_string();

        assert!(!text.contains("gpt-5.5"));
        assert!(!text.contains("high"));
        assert!(!text.contains("hello"));
        assert!(!text.contains("calculator"));
        assert!(text.contains("<redacted-content>"));
        assert!(text.contains("true"));
        assert!(text.contains("2"));
    }

    #[test]
    fn redacts_url_queries_credentials_and_fragments() {
        let redacted = redact_url(
            "https://user:password@example.test/responses?access_token=secret&safe=value#token-secret",
        );

        assert_eq!(redacted, "https://example.test/responses");
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("password"));
    }

    #[test]
    fn stable_hash_is_deterministic_and_does_not_echo_full_value() {
        let first = hash_and_truncate("request-id-with-sensitive-suffix");
        let second = hash_and_truncate("request-id-with-sensitive-suffix");

        assert_eq!(first, second);
        assert!(first.contains("request-"));
        assert!(!first.contains("sensitive-suffix"));
    }

    #[test]
    fn local_key_and_native_account_headers_are_always_redacted() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("x-codex-router-key", "local-client-credential"),
            ("chatgpt-account-id", "private-account-identifier"),
            ("chatgpt-organization-id", "private-organization-identifier"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
            assert!(is_sensitive_header_name(&name.to_ascii_uppercase()));
        }

        let redacted = redact_headers(&headers);

        for name in [
            "x-codex-router-key",
            "chatgpt-account-id",
            "chatgpt-organization-id",
        ] {
            assert_eq!(redacted[name], REDACTED);
        }
        let diagnostic = format!("{redacted:?}");
        for value in [
            "local-client-credential",
            "private-account-identifier",
            "private-organization-identifier",
        ] {
            assert!(!diagnostic.contains(value));
        }
    }

    #[test]
    fn native_identity_fields_are_redacted_in_nested_diagnostics() {
        let value = json!({
            "headers": {
                "X-Codex-Router-Key": "local-client-credential",
                "ChatGPT-Account-Id": "private-account-identifier",
                "ChatGPT-Organization-Id": "private-organization-identifier"
            },
            "account": {"chatgpt_account_id": "private-account-identifier"},
            "organization": {"chatgpt_organization_id": "private-organization-identifier"}
        });

        let redacted = redact_json(&value).to_string();

        for leaked in [
            "local-client-credential",
            "private-account-identifier",
            "private-organization-identifier",
        ] {
            assert!(!redacted.contains(leaked), "leaked {leaked}");
        }
    }
}
