use crate::config::CopilotHeaderConfig;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, USER_AGENT,
};
use http::{HeaderMap, HeaderName, HeaderValue};
use thiserror::Error;

pub const LOCAL_CLIENT_KEY_HEADER: &str = "x-codex-router-key";

pub const FORWARDED_CODEX_HEADERS: &[&str] = &[
    "x-client-request-id",
    "x-codex-parent-thread-id",
    "x-codex-sandbox",
    "x-codex-window-id",
    "x-openai-subagent",
];

pub fn forwarded_codex_header_names(inbound: &HeaderMap) -> Vec<&'static str> {
    FORWARDED_CODEX_HEADERS
        .iter()
        .copied()
        .filter(|header| inbound.contains_key(*header))
        .collect()
}

#[derive(Debug, Error)]
pub enum HeaderBuildError {
    #[error("invalid upstream authorization header")]
    InvalidAuthorization,
    #[error("invalid static upstream header value for {0}")]
    InvalidStaticValue(&'static str),
    #[error("invalid connection header")]
    InvalidConnection,
}

pub fn build_upstream_headers(
    inbound: &HeaderMap,
    authorization: &str,
    options: &CopilotHeaderConfig,
    default_accept: &'static str,
    request_id: &str,
    default_content_type: bool,
) -> Result<HeaderMap, HeaderBuildError> {
    let connection_headers = connection_header_names(inbound)?;
    let mut out = HeaderMap::new();

    let mut auth =
        HeaderValue::from_str(authorization).map_err(|_| HeaderBuildError::InvalidAuthorization)?;
    auth.set_sensitive(true);
    out.insert(AUTHORIZATION, auth);

    if let Some(value) = inbound.get(CONTENT_TYPE) {
        out.insert(CONTENT_TYPE, value.clone());
    } else if default_content_type {
        out.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }

    if let Some(value) = inbound.get(ACCEPT) {
        out.insert(ACCEPT, value.clone());
    } else {
        insert_static(&mut out, ACCEPT, default_accept, "accept")?;
    }

    copy_if_present(inbound, &mut out, ACCEPT_ENCODING);
    copy_if_present(inbound, &mut out, CONTENT_ENCODING);

    for header in FORWARDED_CODEX_HEADERS {
        let name = HeaderName::from_static(header);
        copy_if_present(inbound, &mut out, name);
    }

    insert_static(
        &mut out,
        HeaderName::from_static("copilot-integration-id"),
        "vscode-chat",
        "copilot-integration-id",
    )?;
    insert_owned(
        &mut out,
        HeaderName::from_static("editor-plugin-version"),
        format!("copilot-chat/{}", options.copilot_chat_version),
        "editor-plugin-version",
    )?;
    insert_owned(
        &mut out,
        HeaderName::from_static("editor-version"),
        options.copilot_editor_version.clone(),
        "editor-version",
    )?;
    insert_owned(
        &mut out,
        USER_AGENT,
        format!("GitHubCopilotChat/{}", options.copilot_chat_version),
        "user-agent",
    )?;
    insert_static(
        &mut out,
        HeaderName::from_static("openai-intent"),
        "conversation-agent",
        "openai-intent",
    )?;
    insert_owned(
        &mut out,
        HeaderName::from_static("x-github-api-version"),
        options.github_api_version.clone(),
        "x-github-api-version",
    )?;
    insert_static(
        &mut out,
        HeaderName::from_static("x-initiator"),
        "agent",
        "x-initiator",
    )?;
    insert_owned(
        &mut out,
        HeaderName::from_static("x-request-id"),
        request_id.to_owned(),
        "x-request-id",
    )?;
    insert_static(
        &mut out,
        HeaderName::from_static("x-vscode-user-agent-library-version"),
        "electron-fetch",
        "x-vscode-user-agent-library-version",
    )?;

    for name in connection_headers {
        out.remove(name);
    }

    Ok(out)
}

pub fn build_native_headers(
    inbound: &HeaderMap,
    default_accept: &str,
) -> Result<HeaderMap, HeaderBuildError> {
    let connection_headers = connection_header_names(inbound)?;
    let authorization_values = inbound.get_all(AUTHORIZATION);
    let mut authorization_values = authorization_values.iter();
    let authorization = authorization_values
        .next()
        .ok_or(HeaderBuildError::InvalidAuthorization)?;
    if authorization_values.next().is_some() {
        return Err(HeaderBuildError::InvalidAuthorization);
    }
    let authorization = authorization
        .to_str()
        .map_err(|_| HeaderBuildError::InvalidAuthorization)?;
    let (scheme, token) = authorization
        .split_once(' ')
        .ok_or(HeaderBuildError::InvalidAuthorization)?;
    let token = token.trim_start_matches(' ');
    if !scheme.eq_ignore_ascii_case("Bearer")
        || token.is_empty()
        || !token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
    {
        return Err(HeaderBuildError::InvalidAuthorization);
    }

    let mut out = HeaderMap::new();
    for (name, value) in inbound {
        if is_native_header(name.as_str()) && !connection_headers.contains(name) {
            let mut value = value.clone();
            if name == AUTHORIZATION
                || matches!(
                    name.as_str(),
                    "chatgpt-account-id" | "chatgpt-organization-id"
                )
            {
                value.set_sensitive(true);
            }
            out.append(name.clone(), value);
        }
    }
    if !out.contains_key(ACCEPT) && !connection_headers.contains(&ACCEPT) {
        let accept = HeaderValue::from_str(default_accept)
            .map_err(|_| HeaderBuildError::InvalidStaticValue("accept"))?;
        out.insert(ACCEPT, accept);
    }
    if !out.contains_key(CONTENT_TYPE) && !connection_headers.contains(&CONTENT_TYPE) {
        out.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    Ok(out)
}

fn is_native_header(name: &str) -> bool {
    name != LOCAL_CLIENT_KEY_HEADER
        && (name.starts_with("x-codex-")
            || matches!(
                name,
                "authorization"
                    | "chatgpt-account-id"
                    | "chatgpt-organization-id"
                    | "user-agent"
                    | "openai-beta"
                    | "originator"
                    | "version"
                    | "session_id"
                    | "conversation_id"
                    | "accept"
                    | "content-type"
                    | "content-encoding"
                    | "accept-encoding"
                    | "x-client-request-id"
                    | "x-request-id"
                    | "x-openai-subagent"
            ))
}

fn connection_header_names(inbound: &HeaderMap) -> Result<Vec<HeaderName>, HeaderBuildError> {
    let mut names = Vec::new();
    for value in inbound.get_all(http::header::CONNECTION).iter() {
        let value = value
            .to_str()
            .map_err(|_| HeaderBuildError::InvalidConnection)?;
        for name in value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| HeaderBuildError::InvalidConnection)?;
            // Authorization is required for both upstreams. Do not silently
            // remove it or forward a credential explicitly marked hop-by-hop.
            if name == AUTHORIZATION {
                return Err(HeaderBuildError::InvalidAuthorization);
            }
            names.push(name);
        }
    }
    Ok(names)
}

fn copy_if_present(inbound: &HeaderMap, out: &mut HeaderMap, name: HeaderName) {
    if let Some(value) = inbound.get(&name) {
        out.insert(name, value.clone());
    }
}

fn insert_static(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &'static str,
    label: &'static str,
) -> Result<(), HeaderBuildError> {
    let value =
        HeaderValue::from_str(value).map_err(|_| HeaderBuildError::InvalidStaticValue(label))?;
    headers.insert(name, value);
    Ok(())
}

fn insert_owned(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: String,
    label: &'static str,
) -> Result<(), HeaderBuildError> {
    let value =
        HeaderValue::from_str(&value).map_err(|_| HeaderBuildError::InvalidStaticValue(label))?;
    headers.insert(name, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redaction::redact_headers;
    use http::header::CONTENT_TYPE;

    fn options() -> CopilotHeaderConfig {
        CopilotHeaderConfig {
            copilot_chat_version: "test-chat".to_owned(),
            copilot_editor_version: "vscode/test".to_owned(),
            github_api_version: "2025-10-01".to_owned(),
        }
    }

    #[test]
    fn forwards_codex_headers_and_injects_copilot_headers() {
        let mut inbound = HeaderMap::new();
        inbound.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        inbound.insert(
            HeaderName::from_static("x-codex-window-id"),
            HeaderValue::from_static("thread:0"),
        );

        let headers = build_upstream_headers(
            &inbound,
            "Bearer secret-token",
            &options(),
            "text/event-stream",
            "fixed-request-id",
            true,
        )
        .unwrap();

        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer secret-token");
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(headers.get("x-codex-window-id").unwrap(), "thread:0");
        assert_eq!(
            headers.get("copilot-integration-id").unwrap(),
            "vscode-chat"
        );
        assert_eq!(
            headers.get("editor-plugin-version").unwrap(),
            "copilot-chat/test-chat"
        );
        assert_eq!(headers.get("editor-version").unwrap(), "vscode/test");
        assert_eq!(headers.get("x-github-api-version").unwrap(), "2025-10-01");
        assert_eq!(headers.get("x-request-id").unwrap(), "fixed-request-id");

        let redacted = redact_headers(&headers);
        assert!(!format!("{redacted:?}").contains("secret-token"));
    }

    #[test]
    fn responses_requests_default_to_json_bodies_and_sse_responses_when_codex_is_silent() {
        let inbound = HeaderMap::new();

        let headers = build_upstream_headers(
            &inbound,
            "Bearer secret-token",
            &options(),
            "text/event-stream",
            "fixed-request-id",
            true,
        )
        .unwrap();

        assert_eq!(
            headers.get(CONTENT_TYPE).unwrap(),
            "application/json",
            "Responses requests should default to the JSON wire format Codex sends."
        );
        assert_eq!(
            headers.get(ACCEPT).unwrap(),
            "text/event-stream",
            "Responses requests should default to accepting native Responses SSE."
        );
    }

    #[test]
    fn models_requests_do_not_invent_a_body_content_type() {
        let inbound = HeaderMap::new();

        let headers = build_upstream_headers(
            &inbound,
            "Bearer secret-token",
            &options(),
            "application/json",
            "fixed-request-id",
            false,
        )
        .unwrap();

        assert!(
            headers.get(CONTENT_TYPE).is_none(),
            "Bodyless model-catalog requests should not claim to send JSON bodies."
        );
        assert_eq!(
            headers.get(ACCEPT).unwrap(),
            "application/json",
            "The model catalog boundary should ask for JSON when Codex omits Accept."
        );
    }

    #[test]
    fn native_headers_preserve_identity_without_local_or_copilot_credentials() {
        let mut inbound = HeaderMap::new();
        for (name, value) in [
            ("authorization", "Bearer native-credential"),
            ("chatgpt-account-id", "native-account"),
            ("chatgpt-organization-id", "native-organization"),
            ("user-agent", "codex-desktop/test"),
            ("openai-beta", "responses=test"),
            ("originator", "codex_desktop"),
            ("version", "test-version"),
            ("session_id", "test-session"),
            ("conversation_id", "test-conversation"),
            ("accept", "text/event-stream"),
            ("content-type", "application/json"),
            ("content-encoding", "identity"),
            ("accept-encoding", "gzip"),
            ("x-client-request-id", "native-client-request"),
            ("x-request-id", "native-request"),
            ("x-openai-subagent", "test-subagent"),
            ("x-codex-future-header", "codex-metadata"),
            (LOCAL_CLIENT_KEY_HEADER, "local-credential"),
            ("copilot-integration-id", "vscode-chat"),
            ("editor-plugin-version", "copilot-chat/test"),
            ("editor-version", "vscode/test"),
            ("openai-intent", "conversation-agent"),
            ("x-github-api-version", "test"),
            ("x-initiator", "agent"),
            ("x-vscode-user-agent-library-version", "electron-fetch"),
            ("x-copilot-token", "copilot-credential"),
            ("proxy-authorization", "Bearer proxy-credential"),
            ("cookie", "session=browser-credential"),
        ] {
            inbound.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }

        let headers = build_native_headers(&inbound, "application/json").unwrap();

        for name in [
            "authorization",
            "chatgpt-account-id",
            "chatgpt-organization-id",
            "user-agent",
            "openai-beta",
            "originator",
            "version",
            "session_id",
            "conversation_id",
            "accept",
            "content-type",
            "content-encoding",
            "accept-encoding",
            "x-client-request-id",
            "x-request-id",
            "x-openai-subagent",
            "x-codex-future-header",
        ] {
            assert_eq!(
                headers.get(name),
                inbound.get(name),
                "lost native header {name}"
            );
        }
        for name in [
            LOCAL_CLIENT_KEY_HEADER,
            "copilot-integration-id",
            "editor-plugin-version",
            "editor-version",
            "openai-intent",
            "x-github-api-version",
            "x-initiator",
            "x-vscode-user-agent-library-version",
            "x-copilot-token",
            "proxy-authorization",
            "cookie",
        ] {
            assert!(!headers.contains_key(name), "leaked provider header {name}");
        }
        assert!(headers[AUTHORIZATION].is_sensitive());
        assert!(headers["chatgpt-account-id"].is_sensitive());
        assert!(headers["chatgpt-organization-id"].is_sensitive());
    }

    #[test]
    fn native_headers_require_one_nonempty_bearer_authorization() {
        for authorization in [
            None,
            Some(""),
            Some("Bearer"),
            Some("Bearer "),
            Some("Basic native"),
            Some("Bearer first, Bearer second"),
        ] {
            let mut inbound = HeaderMap::new();
            if let Some(authorization) = authorization {
                inbound.insert(AUTHORIZATION, HeaderValue::from_static(authorization));
            }
            assert!(matches!(
                build_native_headers(&inbound, "application/json"),
                Err(HeaderBuildError::InvalidAuthorization)
            ));
        }
        let mut inbound = HeaderMap::new();
        inbound.append(AUTHORIZATION, HeaderValue::from_static("Bearer first"));
        inbound.append(AUTHORIZATION, HeaderValue::from_static("Bearer second"));
        assert!(matches!(
            build_native_headers(&inbound, "application/json"),
            Err(HeaderBuildError::InvalidAuthorization)
        ));
    }

    #[test]
    fn native_connection_nominated_headers_are_not_forwarded_or_recreated() {
        let mut inbound = HeaderMap::new();
        inbound.insert(AUTHORIZATION, HeaderValue::from_static("Bearer native"));
        inbound.append(
            "connection",
            HeaderValue::from_static("keep-alive, X-Codex-Hop, Accept"),
        );
        inbound.append(
            "connection",
            HeaderValue::from_static("ChatGPT-Account-Id, Content-Type"),
        );
        inbound.insert("x-codex-hop", HeaderValue::from_static("hop-only"));
        inbound.insert(
            "chatgpt-account-id",
            HeaderValue::from_static("hop-account"),
        );
        inbound.insert("accept", HeaderValue::from_static("text/event-stream"));
        inbound.insert("content-type", HeaderValue::from_static("application/json"));
        inbound.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        inbound.insert("transfer-encoding", HeaderValue::from_static("chunked"));

        let headers = build_native_headers(&inbound, "application/json").unwrap();

        assert_eq!(headers[AUTHORIZATION], "Bearer native");
        for name in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "x-codex-hop",
            "chatgpt-account-id",
            "accept",
            "content-type",
        ] {
            assert!(!headers.contains_key(name), "forwarded hop header {name}");
        }
    }

    #[test]
    fn native_connection_cannot_hide_required_authorization() {
        let mut inbound = HeaderMap::new();
        inbound.insert(AUTHORIZATION, HeaderValue::from_static("Bearer native"));
        inbound.append("connection", HeaderValue::from_static("keep-alive"));
        inbound.append("connection", HeaderValue::from_static("AUTHORIZATION"));

        assert!(matches!(
            build_native_headers(&inbound, "application/json"),
            Err(HeaderBuildError::InvalidAuthorization)
        ));
    }

    #[test]
    fn copilot_headers_never_forward_native_or_local_credentials() {
        let mut inbound = HeaderMap::new();
        inbound.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer native-credential"),
        );
        inbound.insert(
            LOCAL_CLIENT_KEY_HEADER,
            HeaderValue::from_static("local-credential"),
        );
        inbound.insert(
            "chatgpt-account-id",
            HeaderValue::from_static("native-account"),
        );
        inbound.insert(
            "chatgpt-organization-id",
            HeaderValue::from_static("native-organization"),
        );
        inbound.insert(
            "connection",
            HeaderValue::from_static("x-codex-window-id, Accept"),
        );
        inbound.insert("x-codex-window-id", HeaderValue::from_static("hop-window"));

        let headers = build_upstream_headers(
            &inbound,
            "Bearer copilot-credential",
            &options(),
            "text/event-stream",
            "request",
            true,
        )
        .unwrap();

        assert_eq!(headers[AUTHORIZATION], "Bearer copilot-credential");
        for name in [
            LOCAL_CLIENT_KEY_HEADER,
            "chatgpt-account-id",
            "chatgpt-organization-id",
            "x-codex-window-id",
            "accept",
        ] {
            assert!(
                !headers.contains_key(name),
                "leaked native/local/hop header {name}"
            );
        }
    }
}
