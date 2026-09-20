use crate::catalog::load_copilot_catalog;
use crate::config::{
    AppConfig, DEFAULT_MODELS_URL, DEFAULT_REQUEST_BODY_LIMIT_BYTES, DEFAULT_RESPONSES_URL,
    DEFAULT_RETRY_BUDGET,
};
use crate::diagnostics::{
    request_content_snapshot, response_content_snapshot, write_raw_content_event, write_raw_event,
};
use crate::headers::{
    build_native_headers, build_upstream_headers, forwarded_codex_header_names,
    LOCAL_CLIENT_KEY_HEADER,
};
use crate::redaction::{hash_and_truncate, redact_headers, redact_url, truncate_for_log};
use crate::retry::{retry_budget_exceeded, select_retry_wait, MAX_RATE_LIMIT_RETRIES};
use crate::routing::{route_combined_request, Operation, Provider, RoutingError};
use crate::token::{
    refresh_token_file_authorization, resolve_upstream_authorization, AuthError, AuthSource,
    ResolvedAuthorization,
};
use anyhow::{bail, Context as _};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, HOST, ORIGIN};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use futures_util::Stream;
use serde_json::json;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const HOP_BY_HOP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone)]
pub struct AppState {
    config: Arc<AppConfig>,
    client: reqwest::Client,
    client_token: Arc<[u8]>,
    copilot_models: Option<Arc<HashSet<String>>>,
}

impl AppState {
    pub fn new(mut config: AppConfig) -> anyhow::Result<Self> {
        let client_token = load_client_token(&config.client_token_file)?;
        let copilot_models = match load_copilot_catalog(&config.copilot_catalog_file) {
            Ok(models) => Some(Arc::new(models)),
            Err(_) => {
                warn!("Copilot model catalog unavailable or invalid; combined Copilot aliases disabled");
                None
            }
        };
        if config.request_body_limit_bytes == 0 {
            config.request_body_limit_bytes = DEFAULT_REQUEST_BODY_LIMIT_BYTES;
        }
        if config.rate_limit.max_total_wait.is_zero() {
            config.rate_limit.max_total_wait = DEFAULT_RETRY_BUDGET;
        }
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self {
            config: Arc::new(config),
            client,
            client_token,
            copilot_models,
        })
    }
}

pub fn app(state: AppState) -> Router {
    let request_body_limit_bytes = state.config.request_body_limit_bytes;

    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .route("/combined/v1/responses", post(combined_responses))
        .route("/combined/v1/responses/compact", post(combined_compact))
        .route("/combined/v1/responses/lite", post(combined_lite))
        .fallback(not_found)
        .layer(request_body_limit_layer(request_body_limit_bytes))
        .layer(middleware::from_fn_with_state(state.clone(), guard_ingress))
        .with_state(state)
}

fn request_body_limit_layer(limit_bytes: usize) -> DefaultBodyLimit {
    DefaultBodyLimit::max(limit_bytes)
}

fn load_client_token(path: &Path) -> anyhow::Result<Arc<[u8]>> {
    let metadata = fs::symlink_metadata(path).context("cannot inspect local client token file")?;
    if !metadata.file_type().is_file() {
        bail!("local client token must be a regular file, not a symlink");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .context("cannot open local client token file")?;
    let opened = file
        .metadata()
        .context("cannot inspect opened local client token file")?;
    if !opened.is_file() {
        bail!("local client token must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.mode() & 0o077 != 0 {
            bail!("local client token file must have owner-only permissions (0600)");
        }
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            bail!("local client token file changed while opening");
        }
    }
    let mut text = String::new();
    file.take(4099)
        .read_to_string(&mut text)
        .context("cannot read local client token file")?;
    let token = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    let unpadded = token.trim_end_matches('=');
    if !(32..=4096).contains(&token.len())
        || unpadded.is_empty()
        || !unpadded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/".contains(&byte))
    {
        bail!("local client token must contain 32 to 4096 valid bearer-token characters");
    }
    Ok(Arc::from(token.as_bytes()))
}

async fn guard_ingress(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let mut hosts = headers.get_all(HOST).iter();
    let host_allowed = hosts
        .next()
        .and_then(|host| host.to_str().ok())
        .is_some_and(loopback_authority)
        && hosts.next().is_none();
    let uri_allowed = request
        .uri()
        .authority()
        .is_none_or(|authority| loopback_authority(authority.as_str()));
    if !host_allowed
        || !uri_allowed
        || headers.contains_key(ORIGIN)
        || headers
            .keys()
            .any(|name| name.as_str().starts_with("sec-fetch-"))
    {
        return json_response(
            StatusCode::FORBIDDEN,
            json!({"error": "local_clients_only"}),
        );
    }

    let combined =
        request.uri().path() == "/combined/v1" || request.uri().path().starts_with("/combined/v1/");
    if combined {
        let mut values = headers.get_all(LOCAL_CLIENT_KEY_HEADER).iter();
        let authorized = values
            .next()
            .is_some_and(|value| bool::from(state.client_token.as_ref().ct_eq(value.as_bytes())))
            && values.next().is_none();
        if !authorized {
            return json_response(
                StatusCode::UNAUTHORIZED,
                json!({"error": "invalid_client_authorization"}),
            );
        }
        if headers.get_all(CONTENT_ENCODING).iter().any(|value| {
            value.to_str().map_or(true, |value| {
                value
                    .split(',')
                    .any(|encoding| !encoding.trim().eq_ignore_ascii_case("identity"))
            })
        }) {
            return json_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                json!({"error": "encoded_combined_request_unsupported"}),
            );
        }
    } else if matches!(request.uri().path(), "/v1/models" | "/v1/responses") {
        let mut values = headers.get_all(AUTHORIZATION).iter();
        let authorized = values
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .is_some_and(|(scheme, token)| {
                scheme.eq_ignore_ascii_case("Bearer")
                    && bool::from(state.client_token.as_ref().ct_eq(token.as_bytes()))
            })
            && values.next().is_none();
        if !authorized {
            return json_response(
                StatusCode::UNAUTHORIZED,
                json!({"error": "invalid_client_authorization"}),
            );
        }
    }
    request.headers_mut().remove(LOCAL_CLIENT_KEY_HEADER);
    if !combined {
        request.headers_mut().remove(AUTHORIZATION);
    }
    next.run(request).await
}

fn loopback_authority(authority: &str) -> bool {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, suffix)) = rest.split_once(']') else {
            return false;
        };
        let port = if suffix.is_empty() {
            None
        } else if let Some(port) = suffix.strip_prefix(':') {
            Some(port)
        } else {
            return false;
        };
        if !host
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|ip| ip.is_loopback())
        {
            return false;
        }
        (host, port)
    } else {
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)));
        if !host.eq_ignore_ascii_case("localhost")
            && !host
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|ip| ip.is_loopback())
        {
            return false;
        }
        (host, port)
    };
    !host.is_empty()
        && port.is_none_or(|port| {
            !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok()
        })
}

pub async fn serve(config: AppConfig) -> anyhow::Result<()> {
    let host = if config.host.eq_ignore_ascii_case("localhost") {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        config
            .host
            .parse::<IpAddr>()
            .context("HOST must be localhost or a numeric loopback address")?
    };
    if !host.is_loopback() {
        bail!("HOST must be a loopback address");
    }
    let bind = SocketAddr::new(host, config.port);
    let config_summary = config.safe_summary();
    let upstream = redact_url(
        config
            .upstream_responses_url
            .as_deref()
            .unwrap_or(DEFAULT_RESPONSES_URL),
    );
    let state = AppState::new(config)?;
    let listener = TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;

    info!(
        %addr,
        upstream_responses_url = %upstream,
        config = ?config_summary,
        "codex-code-router listening"
    );
    if state.config.raw_log.level.allows_metadata() {
        warn!(
            raw_log_file = %state.config.raw_log.file.display(),
            max_bytes = state.config.raw_log.max_bytes,
            level = state.config.raw_log.level.as_str(),
            "raw diagnostics are enabled; logs may reveal request/tool context"
        );
        if state.config.raw_log.level.allows_content() {
            warn!(
                level = state.config.raw_log.level.as_str(),
                "content-level raw diagnostics are enabled; keep this mode temporary and treat logs as sensitive"
            );
        }
    }

    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

async fn health() -> Response {
    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "service": "codex-code-router",
        }),
    )
}

async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    forward(
        state,
        Method::GET,
        Target::Models,
        headers,
        None,
        "application/json",
        false,
    )
    .await
}

async fn responses(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    forward(
        state,
        Method::POST,
        Target::Responses,
        headers,
        Some(body),
        "text/event-stream",
        true,
    )
    .await
}

async fn combined_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    combined(state, headers, body, Operation::Responses).await
}

async fn combined_compact(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    combined(state, headers, body, Operation::Compact).await
}

async fn combined_lite(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    combined(state, headers, body, Operation::Lite).await
}

async fn combined(
    state: AppState,
    mut headers: HeaderMap,
    body: Bytes,
    operation: Operation,
) -> Response {
    let routed = match route_combined_request(body, operation, state.copilot_models.as_deref()) {
        Ok(routed) => routed,
        Err(error) => {
            let (status, code) = match error {
                RoutingError::InvalidRequest => {
                    (StatusCode::BAD_REQUEST, "invalid_routing_request")
                }
                RoutingError::UnknownCopilotModel => {
                    (StatusCode::BAD_REQUEST, "unknown_copilot_model")
                }
                RoutingError::CatalogUnavailable => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "copilot_catalog_unavailable",
                ),
                RoutingError::UnsupportedCopilotOperation => {
                    (StatusCode::NOT_IMPLEMENTED, "unsupported_copilot_operation")
                }
            };
            return json_response(status, json!({"error": code}));
        }
    };
    let target = match routed.provider {
        Provider::OpenAI => Target::Native(operation),
        Provider::Copilot => {
            for name in [
                "authorization",
                "chatgpt-account-id",
                "chatgpt-organization-id",
                "openai-organization",
                "openai-project",
            ] {
                headers.remove(name);
            }
            Target::CombinedCopilotResponses
        }
    };
    let accept = match operation {
        Operation::Responses => "text/event-stream",
        Operation::Compact | Operation::Lite => "application/json",
    };
    forward(
        state,
        Method::POST,
        target,
        headers,
        Some(routed.body),
        accept,
        true,
    )
    .await
}

async fn not_found() -> Response {
    json_response(
        StatusCode::NOT_FOUND,
        json!({
            "error": "not_found",
            "message": "Supported endpoints: GET /health, GET /v1/models, POST /v1/responses."
        }),
    )
}

async fn forward(
    state: AppState,
    method: Method,
    target: Target,
    inbound_headers: HeaderMap,
    body: Option<Bytes>,
    default_accept: &'static str,
    default_content_type: bool,
) -> Response {
    let local_id = Uuid::new_v4().to_string();
    let body_len = body.as_ref().map(|body| body.len()).unwrap_or_default();
    let content_type = header_value(&inbound_headers, CONTENT_TYPE);
    let accept = header_value(&inbound_headers, ACCEPT);
    let forwarded_codex_headers = forwarded_codex_header_names(&inbound_headers);

    info!(
        local_id,
        method = %method,
        target = target.as_str(),
        body_len,
        content_type = ?content_type,
        accept = ?accept,
        forwarded_codex_headers = ?forwarded_codex_headers,
        "inbound request"
    );
    write_raw_event(
        &state.config.raw_log,
        "inbound_request",
        json!({
            "local_id": &local_id,
            "method": method.as_str(),
            "target": target.as_str(),
            "body_len": body_len,
            "content_type": content_type,
            "accept": accept,
            "forwarded_codex_headers": forwarded_codex_headers,
        }),
    );
    if let Some(body) = &body {
        write_raw_content_event(
            &state.config.raw_log,
            "inbound_request_content",
            json!({
                "local_id": &local_id,
                "target": target.as_str(),
                "snapshot": request_content_snapshot(&state.config.raw_log, body),
            }),
        );
    }

    let mut authorization = if target.is_native() {
        None
    } else {
        match resolve_upstream_authorization(
            &state.config.auth,
            &state.config.headers,
            &state.client,
        )
        .await
        {
            Ok(authorization) => {
                info!(
                    local_id,
                    auth_source = ?authorization.source(),
                    "resolved upstream authorization"
                );
                write_raw_event(
                    &state.config.raw_log,
                    "auth_resolved",
                    json!({
                        "local_id": &local_id,
                        "auth_source": format!("{:?}", authorization.source()),
                    }),
                );
                Some(authorization)
            }
            Err(error) => {
                log_auth_error(&local_id, target, &state, &error);
                return auth_error_response(error, target);
            }
        }
    };

    let mut attempt = 0_u32;
    let mut total_wait = Duration::ZERO;
    let mut auth_refresh_attempted = false;
    let retry_started = Instant::now();
    let retry_budget = state.config.rate_limit.max_total_wait;
    let mut rate_limit_retries = 0_u32;

    loop {
        let url = match target.url(&state.config, authorization.as_ref()) {
            Ok(url) => url,
            Err(error) => {
                log_auth_error(&local_id, target, &state, &error);
                return auth_error_response(error, target);
            }
        };
        let redacted_url = redact_url(&url);
        if retry_started.elapsed() >= retry_budget {
            return retry_timeout_response();
        }
        let request_id = Uuid::new_v4().to_string();
        let request_id_summary = hash_and_truncate(&request_id);
        let attempt_number = attempt.saturating_add(1);
        let header_result = match &authorization {
            Some(authorization) => build_upstream_headers(
                &inbound_headers,
                authorization.header_value(),
                &state.config.headers,
                default_accept,
                &request_id,
                default_content_type,
            ),
            None => build_native_headers(&inbound_headers, default_accept),
        };
        let upstream_headers = match header_result {
            Ok(headers) => headers,
            Err(error) => {
                warn!(
                    local_id,
                    target = target.as_str(),
                    attempt = attempt_number,
                    error = %error,
                    "failed to build upstream headers"
                );
                write_raw_event(
                    &state.config.raw_log,
                    "upstream_header_build_failed",
                    json!({
                        "local_id": &local_id,
                        "target": target.as_str(),
                        "attempt": attempt_number,
                        "error": error.to_string(),
                    }),
                );
                return json_response(
                    if target.is_native() {
                        StatusCode::UNAUTHORIZED
                    } else {
                        StatusCode::BAD_GATEWAY
                    },
                    json!({
                        "error": if target.is_native() { "native_auth_unavailable" } else { "upstream_header_build_failed" },
                        "message": error.to_string(),
                    }),
                );
            }
        };

        debug!(
            local_id,
            target = target.as_str(),
            attempt = attempt_number,
            upstream_url = %redacted_url,
            body_len,
            upstream_request_id = %request_id_summary,
            upstream_headers = ?redact_headers(&upstream_headers),
            "upstream attempt starting"
        );

        let mut request = state
            .client
            .request(method.clone(), &url)
            .headers(upstream_headers);

        if let Some(body) = body.clone() {
            request = request.body(body);
        }

        let attempt_started = Instant::now();
        let mut pending_attempt_log = PendingUpstreamAttemptLog::new(
            state.config.raw_log.clone(),
            local_id.clone(),
            target,
            attempt_number,
            body_len,
            redacted_url.clone(),
            request_id_summary.clone(),
        );
        // Bound only response-header acquisition; successful SSE retains request_timeout.
        let send_result = tokio::time::timeout(
            retry_budget.saturating_sub(retry_started.elapsed()),
            request.send(),
        )
        .await;
        let send_result = match send_result {
            Ok(result) => result,
            Err(_) => {
                pending_attempt_log.complete();
                warn!(
                    local_id,
                    attempt = attempt_number,
                    "retry budget exhausted waiting for upstream headers"
                );
                return retry_timeout_response();
            }
        };
        let upstream = match send_result {
            Ok(response) => {
                pending_attempt_log.complete();
                let elapsed = attempt_started.elapsed();
                debug!(
                    local_id,
                    target = target.as_str(),
                    attempt = attempt_number,
                    status = %response.status(),
                    elapsed_ms = elapsed.as_millis(),
                    upstream_request_id = %request_id_summary,
                    "upstream attempt completed"
                );
                response
            }
            Err(error) => {
                pending_attempt_log.complete();
                let elapsed = attempt_started.elapsed();
                warn!(
                    local_id,
                    target = target.as_str(),
                    attempt = attempt_number,
                    upstream_url = %redacted_url,
                    elapsed_ms = elapsed.as_millis(),
                    error_kind = classify_reqwest_error(&error),
                    error = %safe_reqwest_error(&error),
                    "upstream request failed"
                );
                write_raw_event(
                    &state.config.raw_log,
                    "upstream_request_failed",
                    json!({
                        "local_id": &local_id,
                        "target": target.as_str(),
                        "attempt": attempt_number,
                        "upstream_url": redacted_url,
                        "elapsed_ms": elapsed.as_millis(),
                        "error_kind": classify_reqwest_error(&error),
                        "error": safe_reqwest_error(&error),
                    }),
                );
                return json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({
                        "error": "upstream_request_failed",
                        "message": safe_reqwest_error(&error),
                    }),
                );
            }
        };

        if upstream.status().is_redirection() {
            return json_response(
                StatusCode::BAD_GATEWAY,
                json!({"error": "upstream_redirect_rejected"}),
            );
        }

        let refresh_source = authorization
            .as_ref()
            .map(|authorization| authorization.source())
            .filter(|source| auth_source_is_refreshable_token_file(source));

        if upstream.status() == StatusCode::UNAUTHORIZED
            && !auth_refresh_attempted
            && refresh_source.is_some()
        {
            let elapsed = attempt_started.elapsed();
            warn!(
                local_id,
                target = target.as_str(),
                attempt = attempt_number,
                status = %StatusCode::UNAUTHORIZED,
                auth_source = ?refresh_source,
                elapsed_ms = elapsed.as_millis(),
                upstream_request_id = %request_id_summary,
                "upstream rejected token-file authorization; force-refreshing token and retrying once"
            );
            write_raw_event(
                &state.config.raw_log,
                "upstream_auth_refresh_retry",
                json!({
                    "local_id": &local_id,
                    "target": target.as_str(),
                    "attempt": attempt_number,
                    "status": StatusCode::UNAUTHORIZED.as_u16(),
                    "auth_source": format!("{:?}", refresh_source),
                    "elapsed_ms": elapsed.as_millis(),
                    "upstream_request_id": request_id_summary,
                }),
            );

            drop(upstream);
            auth_refresh_attempted = true;
            let refreshed = match tokio::time::timeout(
                retry_budget.saturating_sub(retry_started.elapsed()),
                refresh_token_file_authorization(
                    &state.config.auth,
                    &state.config.headers,
                    &state.client,
                ),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => return retry_timeout_response(),
            };
            match refreshed {
                Ok(refreshed_authorization) => {
                    info!(
                        local_id,
                        target = target.as_str(),
                        auth_source = ?refreshed_authorization.source(),
                        "resolved refreshed token-file authorization for upstream retry"
                    );
                    write_raw_event(
                        &state.config.raw_log,
                        "auth_refreshed",
                        json!({
                            "local_id": &local_id,
                            "target": target.as_str(),
                            "auth_source": format!("{:?}", refreshed_authorization.source()),
                        }),
                    );
                    authorization = Some(refreshed_authorization);
                    attempt = attempt.saturating_add(1);
                    continue;
                }
                Err(error) => {
                    log_auth_error(&local_id, target, &state, &error);
                    return auth_error_response(error, target);
                }
            }
        }

        if upstream.status() == StatusCode::UNAUTHORIZED && target.is_combined_copilot() {
            return json_response(
                StatusCode::BAD_GATEWAY,
                json!({"error": "copilot_auth_rejected"}),
            );
        }

        if upstream.status() != StatusCode::TOO_MANY_REQUESTS {
            let status = upstream.status();
            let content_type = header_value(upstream.headers(), CONTENT_TYPE);
            let elapsed = attempt_started.elapsed();
            info!(
                local_id,
                target = target.as_str(),
                attempt_count = attempt_number,
                status = %status,
                content_type = ?content_type,
                elapsed_ms = elapsed.as_millis(),
                "upstream response ready; streaming to client"
            );
            write_raw_event(
                &state.config.raw_log,
                "upstream_response_ready",
                json!({
                    "local_id": &local_id,
                    "target": target.as_str(),
                    "attempt_count": attempt_number,
                    "status": status.as_u16(),
                    "content_type": content_type,
                    "elapsed_ms": elapsed.as_millis(),
                }),
            );
            if let Some(content_length) = upstream.content_length() {
                write_raw_content_event(
                    &state.config.raw_log,
                    "upstream_response_content_hint",
                    json!({
                        "local_id": &local_id,
                        "target": target.as_str(),
                        "status": status.as_u16(),
                        "content_length": content_length,
                    }),
                );
            }
            return response_from_upstream(
                upstream,
                state.config.raw_log.clone(),
                local_id,
                target,
            );
        }

        let wait = select_retry_wait(
            upstream.headers(),
            rate_limit_retries,
            &state.config.rate_limit,
            SystemTime::now(),
        );
        let budget_exceeded =
            retry_budget_exceeded(retry_started.elapsed(), wait.delay, retry_budget);
        let retry_limit_exceeded = rate_limit_retries >= MAX_RATE_LIMIT_RETRIES;
        let total_wait_after = total_wait.saturating_add(wait.delay);
        let budget_ms = retry_budget.as_millis();
        warn!(
            local_id,
            target = target.as_str(),
            attempt = attempt_number,
            status = %StatusCode::TOO_MANY_REQUESTS,
            wait_ms = wait.delay.as_millis(),
            raw_wait_ms = wait.raw_delay.as_millis(),
            wait_clamped = wait.clamped,
            wait_source = ?wait.source,
            total_wait_before_ms = total_wait.as_millis(),
            total_wait_after_ms = total_wait_after.as_millis(),
            budget_ms = ?budget_ms,
            retry_budget_exceeded = budget_exceeded,
            retry_limit_exceeded,
            elapsed_ms = retry_started.elapsed().as_millis(),
            upstream_request_id = %request_id_summary,
            "upstream rate-limited request"
        );
        write_raw_event(
            &state.config.raw_log,
            "upstream_rate_limited",
            json!({
                "local_id": &local_id,
                "target": target.as_str(),
                "attempt": attempt_number,
                "status": StatusCode::TOO_MANY_REQUESTS.as_u16(),
                "wait_ms": wait.delay.as_millis(),
                "raw_wait_ms": wait.raw_delay.as_millis(),
                "wait_clamped": wait.clamped,
                "wait_source": format!("{:?}", wait.source),
                "total_wait_before_ms": total_wait.as_millis(),
                "total_wait_after_ms": total_wait_after.as_millis(),
                "budget_ms": budget_ms,
                "retry_budget_exceeded": budget_exceeded,
                "retry_limit_exceeded": retry_limit_exceeded,
                "elapsed_ms": retry_started.elapsed().as_millis(),
                "upstream_request_id": request_id_summary,
            }),
        );
        if budget_exceeded || retry_limit_exceeded {
            warn!(
                local_id,
                target = target.as_str(),
                attempt = attempt_number,
                total_wait_ms = total_wait.as_millis(),
                next_wait_ms = wait.delay.as_millis(),
                budget_ms = ?budget_ms,
                "rate-limit retry time or attempt budget exceeded; returning upstream 429"
            );
            return response_from_upstream(
                upstream,
                state.config.raw_log.clone(),
                local_id,
                target,
            );
        }

        attempt = attempt.saturating_add(1);
        rate_limit_retries += 1;
        drop(upstream);
        tokio::time::sleep(
            wait.delay
                .min(retry_budget.saturating_sub(retry_started.elapsed())),
        )
        .await;
        total_wait = total_wait_after;
    }
}

fn retry_timeout_response() -> Response {
    json_response(
        StatusCode::GATEWAY_TIMEOUT,
        json!({"error": "upstream_retry_budget_exhausted"}),
    )
}

struct PendingUpstreamAttemptLog {
    raw_log: crate::config::RawLogConfig,
    local_id: String,
    target: Target,
    attempt: u32,
    body_len: usize,
    upstream_url: String,
    upstream_request_id: String,
    started: Instant,
    completed: bool,
}

impl PendingUpstreamAttemptLog {
    fn new(
        raw_log: crate::config::RawLogConfig,
        local_id: String,
        target: Target,
        attempt: u32,
        body_len: usize,
        upstream_url: String,
        upstream_request_id: String,
    ) -> Self {
        Self {
            raw_log,
            local_id,
            target,
            attempt,
            body_len,
            upstream_url,
            upstream_request_id,
            started: Instant::now(),
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for PendingUpstreamAttemptLog {
    fn drop(&mut self) {
        if self.completed {
            return;
        }

        let elapsed = self.started.elapsed();
        warn!(
            local_id = %self.local_id,
            target = self.target.as_str(),
            attempt = self.attempt,
            upstream_url = %self.upstream_url,
            body_len = self.body_len,
            elapsed_ms = elapsed.as_millis(),
            upstream_request_id = %self.upstream_request_id,
            "upstream attempt cancelled before response"
        );
        write_raw_event(
            &self.raw_log,
            "upstream_attempt_cancelled",
            json!({
                "local_id": &self.local_id,
                "target": self.target.as_str(),
                "attempt": self.attempt,
                "upstream_url": &self.upstream_url,
                "body_len": self.body_len,
                "elapsed_ms": elapsed.as_millis(),
                "upstream_request_id": &self.upstream_request_id,
            }),
        );
    }
}

fn auth_source_is_refreshable_token_file(source: &AuthSource) -> bool {
    matches!(
        source,
        AuthSource::TokenFile {
            refreshable: true,
            ..
        }
    )
}

fn log_auth_error(local_id: &str, target: Target, state: &AppState, error: &AuthError) {
    let status = auth_error_status(error, target);
    let level = if status == StatusCode::UNAUTHORIZED {
        "warn"
    } else {
        "error"
    };

    if status == StatusCode::UNAUTHORIZED {
        warn!(
            local_id,
            target = target.as_str(),
            status = %status,
            error_class = auth_error_class(error),
            "upstream authorization unavailable"
        );
    } else {
        error!(
            local_id,
            target = target.as_str(),
            status = %status,
            error_class = auth_error_class(error),
            "upstream authorization unavailable"
        );
    }

    write_raw_event(
        &state.config.raw_log,
        "auth_error",
        json!({
            "local_id": local_id,
            "target": target.as_str(),
            "status": status.as_u16(),
            "level": level,
            "error_class": auth_error_class(error),
        }),
    );
}

fn auth_error_response(error: AuthError, target: Target) -> Response {
    let status = auth_error_status(&error, target);

    json_response(
        status,
        json!({
            "error": "copilot_auth_unavailable",
            "error_class": auth_error_class(&error),
        }),
    )
}

fn auth_error_status(error: &AuthError, target: Target) -> StatusCode {
    if target.is_combined_copilot() {
        return StatusCode::BAD_GATEWAY;
    }
    match error {
        AuthError::Missing
        | AuthError::Expired { .. }
        | AuthError::ExpiredMissingGithubToken { .. }
        | AuthError::TokenFileNotRefreshable { .. } => StatusCode::UNAUTHORIZED,
        AuthError::ReadTokenFile { .. }
        | AuthError::ParseTokenFile { .. }
        | AuthError::MissingCopilotToken { .. }
        | AuthError::RefreshStatus { .. }
        | AuthError::RefreshRequest { .. }
        | AuthError::WriteTokenFile { .. }
        | AuthError::MissingRefreshFields => StatusCode::INTERNAL_SERVER_ERROR,
        AuthError::InvalidEndpointMetadata => StatusCode::BAD_GATEWAY,
    }
}

fn response_from_upstream(
    upstream: reqwest::Response,
    raw_log: crate::config::RawLogConfig,
    local_id: String,
    target: Target,
) -> Response {
    let status = upstream.status();
    let mut builder = Response::builder().status(status);

    for (name, value) in upstream.headers() {
        if !is_hop_by_hop(name) {
            builder = builder.header(name.clone(), value.clone());
        }
    }

    let stream = LoggedByteStream::new(
        upstream.bytes_stream(),
        raw_log,
        local_id,
        target,
        status.as_u16(),
    );
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|error| {
            json_response(
                StatusCode::BAD_GATEWAY,
                json!({
                    "error": "upstream_response_build_failed",
                    "message": error.to_string(),
                }),
            )
        })
}

struct LoggedByteStream<S> {
    inner: Pin<Box<S>>,
    raw_log: crate::config::RawLogConfig,
    local_id: String,
    target: Target,
    status: u16,
    started: Instant,
    chunk_count: u64,
    byte_count: u64,
    captured_preview: Vec<u8>,
    completed: bool,
}

impl<S> LoggedByteStream<S> {
    fn new(
        inner: S,
        raw_log: crate::config::RawLogConfig,
        local_id: String,
        target: Target,
        status: u16,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            raw_log,
            local_id,
            target,
            status,
            started: Instant::now(),
            chunk_count: 0,
            byte_count: 0,
            captured_preview: Vec::new(),
            completed: false,
        }
    }
}

impl<S> Stream for LoggedByteStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>>,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.chunk_count = self.chunk_count.saturating_add(1);
                self.byte_count = self.byte_count.saturating_add(chunk.len() as u64);
                if self.raw_log.level.allows_content()
                    && self.captured_preview.len() < self.raw_log.content_max_bytes
                {
                    let remaining = self
                        .raw_log
                        .content_max_bytes
                        .saturating_sub(self.captured_preview.len());
                    let to_copy = remaining.min(chunk.len());
                    self.captured_preview.extend_from_slice(&chunk[..to_copy]);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.completed = true;
                let elapsed = self.started.elapsed();
                warn!(
                    local_id = %self.local_id,
                    target = self.target.as_str(),
                    status = self.status,
                    chunk_count = self.chunk_count,
                    byte_count = self.byte_count,
                    elapsed_ms = elapsed.as_millis(),
                    error_kind = classify_reqwest_error(&error),
                    error = %safe_reqwest_error(&error),
                    "upstream stream error"
                );
                write_raw_event(
                    &self.raw_log,
                    "upstream_stream_error",
                    json!({
                        "local_id": &self.local_id,
                        "target": self.target.as_str(),
                        "status": self.status,
                        "chunk_count": self.chunk_count,
                        "byte_count": self.byte_count,
                        "elapsed_ms": elapsed.as_millis(),
                        "error_kind": classify_reqwest_error(&error),
                        "error": safe_reqwest_error(&error),
                    }),
                );
                write_raw_content_event(
                    &self.raw_log,
                    "upstream_response_content",
                    json!({
                        "local_id": &self.local_id,
                        "target": self.target.as_str(),
                        "status": self.status,
                        "snapshot": response_content_snapshot(&self.raw_log, &self.captured_preview),
                        "source": "stream_error",
                    }),
                );
                Poll::Ready(Some(Err(std::io::Error::other(error))))
            }
            Poll::Ready(None) => {
                self.completed = true;
                let elapsed = self.started.elapsed();
                info!(
                    local_id = %self.local_id,
                    target = self.target.as_str(),
                    status = self.status,
                    chunk_count = self.chunk_count,
                    byte_count = self.byte_count,
                    elapsed_ms = elapsed.as_millis(),
                    "upstream stream completed"
                );
                write_raw_event(
                    &self.raw_log,
                    "upstream_stream_completed",
                    json!({
                        "local_id": &self.local_id,
                        "target": self.target.as_str(),
                        "status": self.status,
                        "chunk_count": self.chunk_count,
                        "byte_count": self.byte_count,
                        "elapsed_ms": elapsed.as_millis(),
                    }),
                );
                write_raw_content_event(
                    &self.raw_log,
                    "upstream_response_content",
                    json!({
                        "local_id": &self.local_id,
                        "target": self.target.as_str(),
                        "status": self.status,
                        "snapshot": response_content_snapshot(&self.raw_log, &self.captured_preview),
                        "source": "stream_completed",
                    }),
                );
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> Drop for LoggedByteStream<S> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let elapsed = self.started.elapsed();
        warn!(
            local_id = %self.local_id,
            target = self.target.as_str(),
            status = self.status,
            chunk_count = self.chunk_count,
            byte_count = self.byte_count,
            elapsed_ms = elapsed.as_millis(),
            "upstream stream dropped before completion"
        );
        write_raw_event(
            &self.raw_log,
            "upstream_stream_dropped",
            json!({
                "local_id": &self.local_id,
                "target": self.target.as_str(),
                "status": self.status,
                "chunk_count": self.chunk_count,
                "byte_count": self.byte_count,
                "elapsed_ms": elapsed.as_millis(),
            }),
        );
        write_raw_content_event(
            &self.raw_log,
            "upstream_response_content",
            json!({
                "local_id": &self.local_id,
                "target": self.target.as_str(),
                "status": self.status,
                "snapshot": response_content_snapshot(&self.raw_log, &self.captured_preview),
                "source": "stream_dropped",
            }),
        );
    }
}

impl<S> Unpin for LoggedByteStream<S> {}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP_RESPONSE_HEADERS
        .iter()
        .any(|candidate| name.as_str().eq_ignore_ascii_case(candidate))
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    let text = serde_json::to_vec(&body).expect("serializing static JSON response should not fail");
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(text))
        .expect("building static JSON response should not fail")
}

fn header_value(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| truncate_for_log(value, 96))
}

fn classify_reqwest_error(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_request() {
        "request"
    } else {
        "unknown"
    }
}

fn safe_reqwest_error(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    if let Some(url) = error.url() {
        text = text.replace(url.as_str(), &redact_url(url.as_str()));
    }
    truncate_for_log(&text, 256)
}

fn auth_error_class(error: &AuthError) -> &'static str {
    match error {
        AuthError::Missing => "missing",
        AuthError::Expired { .. } => "expired",
        AuthError::ExpiredMissingGithubToken { .. } => "expired_missing_github_token",
        AuthError::TokenFileNotRefreshable { .. } => "token_file_not_refreshable",
        AuthError::ReadTokenFile { .. } => "read_token_file",
        AuthError::ParseTokenFile { .. } => "parse_token_file",
        AuthError::MissingCopilotToken { .. } => "missing_copilot_token",
        AuthError::RefreshStatus { .. } => "refresh_status",
        AuthError::RefreshRequest { .. } => "refresh_request",
        AuthError::WriteTokenFile { .. } => "write_token_file",
        AuthError::MissingRefreshFields => "missing_refresh_fields",
        AuthError::InvalidEndpointMetadata => "invalid_endpoint_metadata",
    }
}

#[derive(Clone, Copy)]
enum Target {
    Models,
    Responses,
    CombinedCopilotResponses,
    Native(Operation),
}

impl Target {
    fn as_str(self) -> &'static str {
        match self {
            Self::Models => "Models",
            Self::Responses => "Responses",
            Self::CombinedCopilotResponses => "CombinedCopilotResponses",
            Self::Native(Operation::Responses) => "NativeResponses",
            Self::Native(Operation::Compact) => "NativeCompact",
            Self::Native(Operation::Lite) => "NativeLite",
        }
    }

    fn url(
        self,
        config: &AppConfig,
        authorization: Option<&ResolvedAuthorization>,
    ) -> Result<String, AuthError> {
        match self {
            Self::Models => copilot_route_url(
                config.upstream_models_url.as_deref(),
                authorization,
                "models",
                DEFAULT_MODELS_URL,
            ),
            Self::Responses | Self::CombinedCopilotResponses => copilot_route_url(
                config.upstream_responses_url.as_deref(),
                authorization,
                "responses",
                DEFAULT_RESPONSES_URL,
            ),
            Self::Native(operation) => {
                let suffix = match operation {
                    Operation::Responses => "/responses",
                    Operation::Compact => "/responses/compact",
                    Operation::Lite => "/responses/lite",
                };
                Ok(format!(
                    "{}{suffix}",
                    config.upstream_native_base_url.trim_end_matches('/')
                ))
            }
        }
    }

    fn is_native(self) -> bool {
        matches!(self, Self::Native(_))
    }

    fn is_combined_copilot(self) -> bool {
        matches!(self, Self::CombinedCopilotResponses)
    }
}
fn copilot_route_url(
    explicit: Option<&str>,
    authorization: Option<&ResolvedAuthorization>,
    route: &str,
    fallback: &str,
) -> Result<String, AuthError> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_owned());
    }
    let Some(endpoint) = authorization
        .map(ResolvedAuthorization::endpoint)
        .transpose()?
        .flatten()
    else {
        return Ok(fallback.to_owned());
    };
    Ok(discovered_copilot_route_url(endpoint, route))
}

fn discovered_copilot_route_url(mut endpoint: reqwest::Url, route: &str) -> String {
    endpoint.set_path(route);
    endpoint.to_string()
}

#[cfg(test)]
mod tests {
    use super::discovered_copilot_route_url;

    #[test]
    fn account_endpoint_routes_models_and_responses() {
        let endpoint = reqwest::Url::parse("https://api.business.githubcopilot.com/").unwrap();

        assert_eq!(
            discovered_copilot_route_url(endpoint.clone(), "models"),
            "https://api.business.githubcopilot.com/models"
        );
        assert_eq!(
            discovered_copilot_route_url(endpoint, "responses"),
            "https://api.business.githubcopilot.com/responses"
        );
    }
}
