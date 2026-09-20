use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use codex_code_router::config::{
    AppConfig, AuthConfig, CopilotHeaderConfig, RateLimitConfig, RawLogConfig, RawLogLevel,
    DEFAULT_GITHUB_ACCESS_TOKEN_URL, DEFAULT_GITHUB_DEVICE_CODE_URL,
    DEFAULT_GITHUB_OAUTH_CLIENT_ID, DEFAULT_GITHUB_OAUTH_SCOPE, DEFAULT_REQUEST_BODY_LIMIT_BYTES,
};
use codex_code_router::headers::LOCAL_CLIENT_KEY_HEADER;
use codex_code_router::proxy::{app, AppState};
use futures_util::stream;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;
use tokio::net::TcpListener;

const CLIENT_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn client_token_file() -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(file, "{CLIENT_TOKEN}").unwrap();
    file
}

fn authenticated_client() -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {CLIENT_TOKEN}").parse().unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    headers: HeaderMap,
    body: Vec<u8>,
}

struct TestServer {
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn spawn_router(router: Router) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    TestServer { addr, handle }
}

fn test_config(models_url: String, responses_url: String) -> AppConfig {
    AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        upstream_responses_url: Some(responses_url),
        upstream_models_url: Some(models_url),
        upstream_native_base_url: "http://127.0.0.1/native".to_owned(),
        request_timeout: Duration::from_secs(30),
        request_body_limit_bytes: DEFAULT_REQUEST_BODY_LIMIT_BYTES,
        client_token_file: PathBuf::from("/definitely/not/present/client-token"),
        copilot_catalog_file: PathBuf::new(),
        headers: CopilotHeaderConfig {
            copilot_chat_version: "test-chat".to_owned(),
            copilot_editor_version: "vscode/test".to_owned(),
            github_api_version: "2025-10-01".to_owned(),
        },
        auth: AuthConfig {
            bearer_token: Some("service-owned-token".to_owned()),
            token_file: PathBuf::from("/definitely/not/present"),
            token_expiry_buffer: Duration::from_secs(300),
            refresh_enabled: true,
            copilot_token_url: "http://127.0.0.1/copilot-token".to_owned(),
            github_device_code_url: DEFAULT_GITHUB_DEVICE_CODE_URL.to_owned(),
            github_access_token_url: DEFAULT_GITHUB_ACCESS_TOKEN_URL.to_owned(),
            github_oauth_client_id: DEFAULT_GITHUB_OAUTH_CLIENT_ID.to_owned(),
            github_oauth_scope: DEFAULT_GITHUB_OAUTH_SCOPE.to_owned(),
        },
        rate_limit: RateLimitConfig {
            max_total_wait: Duration::from_secs(30),
            max_sleep: Duration::from_millis(2),
            initial_backoff: Duration::from_millis(1),
            backoff_multiplier: 2.0,
        },
        raw_log: RawLogConfig {
            level: RawLogLevel::Off,
            file: PathBuf::from("/tmp/codex-code-router-test-raw.jsonl"),
            max_bytes: 4096,
            content_max_bytes: 4096,
        },
    }
}

async fn spawn_app(mut config: AppConfig) -> TestServer {
    let token_file = client_token_file();
    let mut catalog = NamedTempFile::new().unwrap();
    if config.copilot_catalog_file.as_os_str().is_empty() {
        catalog.write_all(br#"{"models":[{"slug":"copilot/gpt-6-astra"},{"slug":"copilot/gpt-5.6-sol"},{"slug":"copilot/example-model"}]}"#).unwrap();
        config.copilot_catalog_file = catalog.path().to_path_buf();
    }
    config.client_token_file = token_file.path().to_path_buf();
    spawn_router(app(AppState::new(config).unwrap())).await
}

fn enable_raw_log(config: &mut AppConfig, file: &NamedTempFile) {
    config.raw_log.level = RawLogLevel::Metadata;
    config.raw_log.file = file.path().to_path_buf();
    config.raw_log.max_bytes = 16 * 1024;
    config.raw_log.content_max_bytes = 4 * 1024;
}

fn set_raw_log_level(config: &mut AppConfig, level: RawLogLevel) {
    config.raw_log.level = level;
}

fn raw_log_text(file: &NamedTempFile) -> String {
    fs::read_to_string(file.path()).unwrap_or_default()
}

fn raw_event_fields(text: &str, kind: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("kind").and_then(Value::as_str) == Some(kind))
        .filter_map(|event| event.get("fields").cloned())
        .collect()
}

fn assert_no_log_secret(text: &str, secret: &str) {
    assert!(
        !text.contains(secret),
        "diagnostic log leaked secret `{secret}`: {text}"
    );
}

fn assert_has_terminal_stream_event(text: &str) {
    assert!(
        text.contains("upstream_stream_completed") || text.contains("upstream_stream_dropped"),
        "expected a terminal stream diagnostic event with counts: {text}"
    );
    assert!(text.contains("chunk_count"));
    assert!(text.contains("byte_count"));
}

fn future_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600
}

#[tokio::test]
async fn health_is_unauthenticated_and_does_not_disclose_upstream_urls() {
    let config = test_config(
        "http://127.0.0.1/models?token=models-secret".to_owned(),
        "http://user:password@127.0.0.1/responses?token=responses-secret".to_owned(),
    );
    let server = spawn_app(config).await;

    let response = reqwest::get(server.url("/health")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        serde_json::json!({
            "ok": true,
            "service": "codex-code-router",
        })
    );
}

#[test]
fn missing_client_token_prevents_startup() {
    let config = test_config(
        "http://127.0.0.1/models".into(),
        "http://127.0.0.1/responses".into(),
    );
    assert!(AppState::new(config).is_err());
}

#[test]
fn malformed_client_tokens_prevent_startup_without_disclosing_contents() {
    for token in [
        "",
        "short-secret",
        "secret with spaces that is longer than thirty-two characters",
        "0123456789abcdef0123456789abcdef\nsecond-line",
    ] {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{token}").unwrap();
        let mut config = test_config(
            "http://127.0.0.1/models".into(),
            "http://127.0.0.1/responses".into(),
        );
        config.client_token_file = file.path().to_path_buf();
        let error = match AppState::new(config) {
            Ok(_) => panic!("invalid client token allowed startup"),
            Err(error) => error,
        };
        if !token.is_empty() {
            assert!(!format!("{error:?}").contains(token));
        }
    }
}

#[cfg(unix)]
#[test]
fn unsafe_client_token_files_prevent_startup() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let file = client_token_file();
    let directory = tempfile::tempdir().unwrap();
    let link = directory.path().join("link");
    symlink(file.path(), &link).unwrap();
    for path in [link.as_path(), directory.path()] {
        let mut config = test_config(
            "http://127.0.0.1/models".into(),
            "http://127.0.0.1/responses".into(),
        );
        config.client_token_file = path.to_path_buf();
        assert!(AppState::new(config).is_err());
    }
    fs::set_permissions(file.path(), fs::Permissions::from_mode(0o644)).unwrap();
    let mut config = test_config(
        "http://127.0.0.1/models".into(),
        "http://127.0.0.1/responses".into(),
    );
    config.client_token_file = file.path().to_path_buf();
    assert!(AppState::new(config).is_err());
}

#[tokio::test]
async fn ingress_rejects_missing_or_wrong_tokens_before_buffering_or_upstream() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().fallback(move || {
            let attempts = attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }
        })
    })
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.request_body_limit_bytes = 1024;
    let server = spawn_app(config).await;
    let client = reqwest::Client::new();
    for token in [
        None,
        Some("Bearer wrong-client-token"),
        Some("Bearer f123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        Some("Basic invalid"),
    ] {
        for path in ["/v1/models", "/v1/responses"] {
            let mut request = if path == "/v1/models" {
                client.get(server.url(path))
            } else {
                client.post(server.url(path)).body(vec![b'x'; 1025])
            };
            if let Some(token) = token {
                request = request.header("authorization", token);
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
    }
    let duplicate = client
        .get(server.url("/v1/models"))
        .header("authorization", format!("Bearer {CLIENT_TOKEN}"))
        .header("authorization", "Bearer wrong-client-token")
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ingress_rejects_browser_and_hostile_authority_with_valid_token() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().fallback(move || {
            let attempts = attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }
        })
    })
    .await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;
    for (header, value) in [
        ("origin", "https://attacker.example"),
        ("origin", "null"),
        ("sec-fetch-site", "same-origin"),
        ("sec-fetch-mode", "no-cors"),
        ("sec-fetch-dest", "empty"),
        ("sec-fetch-user", "?1"),
        ("host", "attacker.example:60001"),
        ("host", "localhost.attacker.example"),
        ("host", "user@localhost:60001"),
        ("host", "localhost:bogus"),
        ("host", "localhost:"),
        ("host", "127.0.0.1:99999"),
        ("host", "127.1"),
        ("host", "192.0.2.1"),
    ] {
        for path in ["/health", "/v1/models", "/v1/responses"] {
            let request = if path == "/v1/responses" {
                authenticated_client().post(server.url(path)).body("{}")
            } else {
                authenticated_client().get(server.url(path))
            };
            assert_eq!(
                request.header(header, value).send().await.unwrap().status(),
                StatusCode::FORBIDDEN,
                "{header}: {value}"
            );
        }
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn loopback_authorities_with_ports_are_accepted() {
    let server = spawn_app(test_config(
        "http://127.0.0.1/models".into(),
        "http://127.0.0.1/responses".into(),
    ))
    .await;
    for host in [
        "localhost",
        "localhost:60001",
        "127.0.0.1:60001",
        "127.0.0.2",
        "[::1]",
        "[::1]:60001",
    ] {
        assert_eq!(
            reqwest::Client::new()
                .get(server.url("/health"))
                .header("host", host)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK,
            "{host}"
        );
    }
}

#[tokio::test]
async fn serve_refuses_non_loopback_binding() {
    let token_file = client_token_file();
    let mut config = test_config(
        "http://127.0.0.1/models".into(),
        "http://127.0.0.1/responses".into(),
    );
    config.client_token_file = token_file.path().to_path_buf();
    config.host = "0.0.0.0".into();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        codex_code_router::proxy::serve(config),
    )
    .await;
    assert!(result
        .expect("non-loopback service must not start")
        .is_err());
}

#[tokio::test]
async fn zero_delay_rate_limits_stop_after_eight_retries() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "0")],
                        "limited",
                    )
                }
            }),
        )
    })
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_total_wait = Duration::ZERO;
    let server = spawn_app(config).await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        authenticated_client()
            .post(server.url("/v1/responses"))
            .body("{}")
            .send(),
    )
    .await
    .expect("zero-delay retries must terminate")
    .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.text().await.unwrap(), "limited");
    assert_eq!(attempts.load(Ordering::SeqCst), 9);
}

#[tokio::test]
async fn retry_budget_bounds_slow_response_headers() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "0")],
                        "limited",
                    )
                }
            }),
        )
    })
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_total_wait = Duration::from_millis(100);
    let server = spawn_app(config).await;
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        authenticated_client()
            .post(server.url("/v1/responses"))
            .body("{}")
            .send(),
    )
    .await
    .expect("send time must count against retry budget")
    .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_budget_counts_cumulative_send_time() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "0")],
                        "limited",
                    )
                }
            }),
        )
    })
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_total_wait = Duration::from_millis(650);
    let server = spawn_app(config).await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        authenticated_client()
            .post(server.url("/v1/responses"))
            .body("{}")
            .send(),
    )
    .await
    .expect("retry sends must share one elapsed-time budget")
    .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retry_budget_does_not_truncate_successful_sse() {
    let mock = spawn_router(Router::new().route(
        "/responses",
        post(|| async {
            let chunks = stream::unfold(0, |index| async move {
                match index {
                    0 => Some((
                        Ok::<_, std::convert::Infallible>(Bytes::from_static(b"event: start\n\n")),
                        1,
                    )),
                    1 => {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Some((Ok(Bytes::from_static(b"event: done\n\n")), 2))
                    }
                    _ => None,
                }
            });
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(chunks))
                .unwrap()
        }),
    ))
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_total_wait = Duration::from_millis(100);
    let server = spawn_app(config).await;
    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        b"event: start\n\nevent: done\n\n"
    );
}

#[tokio::test]
async fn zero_body_limit_cannot_disable_default_protection() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        )
    })
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.request_body_limit_bytes = 0;
    let server = spawn_app(config).await;
    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body(vec![b'x'; 16 * 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn models_proxy_forwards_to_mocked_upstream_with_copilot_headers() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let mock = spawn_router({
        let recorded = recorded.clone();
        Router::new().route(
            "/models",
            get(move |headers: HeaderMap| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(RecordedRequest {
                        headers,
                        body: Vec::new(),
                    });
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        r#"{"data":[{"id":"gpt-test"}]}"#,
                    )
                }
            }),
        )
    })
    .await;

    let config = test_config(mock.url("/models"), mock.url("/responses"));
    let server = spawn_app(config).await;

    let response = authenticated_client()
        .get(server.url("/v1/models"))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"data":[{"id":"gpt-test"}]}"#);

    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let headers = &requests[0].headers;
    assert_eq!(
        headers.get("authorization").unwrap(),
        "Bearer service-owned-token"
    );
    assert_eq!(
        headers.get("copilot-integration-id").unwrap(),
        "vscode-chat"
    );
    assert_eq!(
        headers.get("editor-plugin-version").unwrap(),
        "copilot-chat/test-chat"
    );
    assert!(headers.get("accept").is_some());
}

#[tokio::test]
async fn hostile_account_endpoint_is_rejected_before_upstream_network() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().fallback(move || {
            let attempts = attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                (StatusCode::OK, "unexpected")
            }
        })
    })
    .await;
    let token_file = NamedTempFile::new().unwrap();
    fs::write(
        token_file.path(),
        format!(
            r#"{{"copilotToken":"service-token","expiresAt":{},"endpoint":"https://githubcopilot.com.attacker.example/chat/completions"}}"#,
            future_epoch_seconds()
        ),
    )
    .unwrap();
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.upstream_models_url = None;
    config.upstream_responses_url = None;
    config.auth.bearer_token = None;
    config.auth.token_file = token_file.path().to_path_buf();
    let server = spawn_app(config).await;

    let response = authenticated_client()
        .get(server.url("/v1/models"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error_class"], "invalid_endpoint_metadata");
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_fake_upstream_override_wins_over_stored_endpoint_metadata() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/models",
            get(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "explicit override")
                }
            }),
        )
    })
    .await;
    let token_file = NamedTempFile::new().unwrap();
    fs::write(
        token_file.path(),
        format!(
            r#"{{"copilotToken":"service-token","expiresAt":{},"endpoint":"https://attacker.example/chat/completions"}}"#,
            future_epoch_seconds()
        ),
    )
    .unwrap();
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.auth.bearer_token = None;
    config.auth.token_file = token_file.path().to_path_buf();
    let server = spawn_app(config).await;

    let response = authenticated_client()
        .get(server.url("/v1/models"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "explicit override");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn models_raw_diagnostics_log_lifecycle_without_tokens() {
    let raw_log = NamedTempFile::new().unwrap();
    let mock = spawn_router(Router::new().route(
        "/models",
        get(|| async {
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                r#"{"data":[{"id":"gpt-test"}]}"#,
            )
        }),
    ))
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;

    let response = authenticated_client()
        .get(server.url("/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.bytes().await.unwrap();

    let text = raw_log_text(&raw_log);
    assert!(text.contains("inbound_request"));
    assert!(text.contains("upstream_response_ready"));
    assert_has_terminal_stream_event(&text);
    assert!(text.contains("local_id"));
    assert!(text.contains("Models"));
    assert_no_log_secret(&text, "service-owned-token");

    let inbound = raw_event_fields(&text, "inbound_request");
    assert_eq!(inbound[0]["target"], "Models");
    assert_eq!(inbound[0]["body_len"], 0);
}

#[tokio::test]
async fn responses_proxy_streams_sse_bytes_unchanged() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let upstream_sse = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";

    let mock = spawn_router({
        let recorded = recorded.clone();
        let upstream_sse = upstream_sse.to_owned();
        Router::new().route(
            "/responses",
            post(move |headers: HeaderMap, body: Bytes| {
                let recorded = recorded.clone();
                let upstream_sse = upstream_sse.clone();
                async move {
                    recorded.lock().unwrap().push(RecordedRequest {
                        headers,
                        body: body.to_vec(),
                    });
                    let chunks = stream::iter([
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                            upstream_sse[..48].to_owned(),
                        )),
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                            upstream_sse[48..].to_owned(),
                        )),
                    ]);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(chunks))
                        .unwrap()
                }
            }),
        )
    })
    .await;

    let config = test_config(mock.url("/models"), mock.url("/responses"));
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","stream":true,"input":"hello"}"#;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .header("content-type", "application/json")
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.bytes().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/event-stream"));
    assert_eq!(body.as_ref(), upstream_sse.as_bytes());

    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body, request_body);
    assert_eq!(requests[0].headers.get("accept").unwrap(), "*/*");
}

#[tokio::test]
async fn responses_proxy_accepts_body_larger_than_axum_default_limit() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let mock = spawn_router(
        Router::new()
            .route(
                "/responses",
                post({
                    let recorded = recorded.clone();
                    move |headers: HeaderMap, body: Bytes| {
                        let recorded = recorded.clone();
                        async move {
                            recorded.lock().unwrap().push(RecordedRequest {
                                headers,
                                body: body.to_vec(),
                            });
                            (StatusCode::OK, "ok")
                        }
                    }
                }),
            )
            .layer(DefaultBodyLimit::disable()),
    )
    .await;

    let config = test_config(mock.url("/models"), mock.url("/responses"));
    let server = spawn_app(config).await;
    let request_body = vec![b'x'; 2 * 1024 * 1024 + 1];

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .header("content-type", "application/json")
        .body(request_body.clone())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body, request_body);
}

#[tokio::test]
async fn configured_responses_body_limit_rejects_oversized_body_before_upstream() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "should not be called")
                }
            }),
        )
    })
    .await;

    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.request_body_limit_bytes = 1024;
    let server = spawn_app(config).await;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body(vec![b'x'; 1025])
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn responses_raw_diagnostics_log_lifecycle_and_stream_counts_without_body_or_token() {
    let raw_log = NamedTempFile::new().unwrap();
    let upstream_sse = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
    let mock = spawn_router({
        let upstream_sse = upstream_sse.to_owned();
        Router::new().route(
            "/responses",
            post(move || {
                let upstream_sse = upstream_sse.clone();
                async move {
                    let chunks = stream::iter([
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                            upstream_sse[..48].to_owned(),
                        )),
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                            upstream_sse[48..].to_owned(),
                        )),
                    ]);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(chunks))
                        .unwrap()
                }
            }),
        )
    })
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","input":"body-secret-that-must-not-log"}"#;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .header("content-type", "application/json")
        .header("x-codex-window-id", "window-1")
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.bytes().await.unwrap();
    assert_eq!(body.as_ref(), upstream_sse.as_bytes());

    let text = raw_log_text(&raw_log);
    assert!(text.contains("inbound_request"));
    assert!(text.contains("upstream_response_ready"));
    assert!(text.contains("upstream_stream_completed"));
    assert!(text.contains("local_id"));
    assert!(text.contains("Responses"));
    assert_no_log_secret(&text, "service-owned-token");
    assert_no_log_secret(&text, "body-secret-that-must-not-log");

    let inbound = raw_event_fields(&text, "inbound_request");
    assert_eq!(inbound[0]["target"], "Responses");
    assert_eq!(inbound[0]["body_len"], request_body.len());
    assert_eq!(
        inbound[0]["forwarded_codex_headers"][0],
        "x-codex-window-id"
    );

    let stream_events = raw_event_fields(&text, "upstream_stream_completed");
    assert_eq!(stream_events[0]["byte_count"], upstream_sse.len());
    assert!(stream_events[0]["chunk_count"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn responses_content_redacted_logs_structure_without_prompt_values() {
    let raw_log = NamedTempFile::new().unwrap();
    let upstream_sse =
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"message\":\"provider secret\"}\n\n";
    let mock = spawn_router({
        let upstream_sse = upstream_sse.to_owned();
        Router::new().route(
            "/responses",
            post(move || {
                let upstream_sse = upstream_sse.clone();
                async move {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(upstream_sse))
                        .unwrap()
                }
            }),
        )
    })
    .await;

    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    enable_raw_log(&mut config, &raw_log);
    set_raw_log_level(&mut config, RawLogLevel::ContentRedacted);
    let server = spawn_app(config).await;
    let request_body =
        br#"{"model":"gpt-test","reasoning":{"effort":"high"},"input":"user secret prompt"}"#;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.bytes().await.unwrap();

    let text = raw_log_text(&raw_log);
    assert!(text.contains("inbound_request_content"));
    assert!(text.contains("upstream_response_content"));
    assert!(text.contains("<redacted-content>"));
    assert_no_log_secret(&text, "user secret prompt");
    assert_no_log_secret(&text, "high");
    assert_no_log_secret(&text, "provider secret");

    let request_content = raw_event_fields(&text, "inbound_request_content");
    assert_eq!(request_content[0]["snapshot"]["schema_version"], 1);
    assert_eq!(request_content[0]["snapshot"]["direction"], "request");
    assert_eq!(
        request_content[0]["snapshot"]["extracted"]["reasoning_effort"],
        "<redacted-content>"
    );
}

#[tokio::test]
async fn responses_full_content_logs_effort_and_prompt_content() {
    let raw_log = NamedTempFile::new().unwrap();
    let upstream_sse =
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"message\":\"provider clear text\"}\n\n";
    let mock = spawn_router({
        let upstream_sse = upstream_sse.to_owned();
        Router::new().route(
            "/responses",
            post(move || {
                let upstream_sse = upstream_sse.clone();
                async move {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(upstream_sse))
                        .unwrap()
                }
            }),
        )
    })
    .await;

    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    enable_raw_log(&mut config, &raw_log);
    set_raw_log_level(&mut config, RawLogLevel::FullContent);
    let server = spawn_app(config).await;
    let request_body =
        br#"{"model":"gpt-test","reasoning":{"effort":"high"},"input":"plain prompt"}"#;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.bytes().await.unwrap();

    let text = raw_log_text(&raw_log);
    assert!(text.contains("inbound_request_content"));
    assert!(text.contains("upstream_response_content"));
    assert!(text.contains("\"reasoning_effort\":\"high\""));
    assert!(text.contains("plain prompt"));
    assert!(text.contains("provider clear text"));
    assert_no_log_secret(&text, "service-owned-token");

    let request_content = raw_event_fields(&text, "inbound_request_content");
    assert_eq!(request_content[0]["snapshot"]["schema_version"], 1);
    assert_eq!(
        request_content[0]["snapshot"]["extracted"]["reasoning_effort"],
        "high"
    );
    assert_eq!(
        request_content[0]["snapshot"]["extracted"]["tools"]["count"],
        0
    );
}

#[tokio::test]
async fn codex_responses_body_is_not_normalized_before_forwarding() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let mock = spawn_router({
        let recorded = recorded.clone();
        Router::new().route(
            "/responses",
            post(move |headers: HeaderMap, body: Bytes| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(RecordedRequest {
                        headers,
                        body: body.to_vec(),
                    });
                    (StatusCode::OK, "ok")
                }
            }),
        )
    })
    .await;
    let config = test_config(mock.url("/models"), mock.url("/responses"));
    let server = spawn_app(config).await;
    let codex_body = br#"{
  "model": "gpt-test",
  "stream": true,
  "store": true,
  "previous_response_id": "resp_should_remain_if_codex_sent_it",
  "include": ["reasoning.encrypted_content"],
  "reasoning": {"effort": "medium", "summary": "auto"},
  "tools": [{"type": "namespace", "name": "mcp__memory__recall"}],
  "input": [{"type": "reasoning", "encrypted_content": "opaque-provider-state"}]
}"#;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .header("content-type", "application/json")
        .body(codex_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body, codex_body,
        "The adapter is a provider shim; it must not rewrite Codex Responses fields unless a proven compatibility fix is added."
    );
}

#[tokio::test]
async fn in_band_rate_limit_text_is_streamed_without_retrying() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let upstream_sse =
        "event: response.failed\ndata: {\"error\":{\"message\":\"rate limit exceeded\"}}\n\n";
    let mock = spawn_router({
        let attempts = attempts.clone();
        let upstream_sse = upstream_sse.to_owned();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                let upstream_sse = upstream_sse.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(upstream_sse))
                        .unwrap()
                }
            }),
        )
    })
    .await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, upstream_sse);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "Only HTTP 429 status responses should trigger provider rate-limit retry."
    );
}

#[tokio::test]
async fn upstream_hop_by_hop_headers_are_not_exposed_to_codex() {
    let mock = spawn_router(Router::new().route(
        "/models",
        get(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header("connection", "close")
                .header("keep-alive", "timeout=5")
                .header("x-provider-trace", "visible")
                .body(Body::from(r#"{"data":[]}"#))
                .unwrap()
        }),
    ))
    .await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;

    let response = authenticated_client()
        .get(server.url("/v1/models"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-provider-trace").unwrap(),
        "visible"
    );
    assert!(
        response.headers().get("connection").is_none(),
        "Connection-specific upstream headers must not leak across the local proxy boundary."
    );
    assert!(
        response.headers().get("keep-alive").is_none(),
        "Hop-by-hop keep-alive metadata belongs to one network leg only."
    );
}

#[tokio::test]
async fn responses_retry_uses_retry_after_and_preserves_body() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));

    let mock = retry_mock(
        recorded.clone(),
        attempts.clone(),
        RetryMode::RetryAfterThenOk,
    )
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_sleep = Duration::from_millis(1);
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","input":"retry me"}"#;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok after retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    let requests = recorded.lock().unwrap();
    assert_eq!(requests[0].body, request_body);
    assert_eq!(requests[1].body, request_body);
    assert_ne!(
        requests[0].headers.get("x-request-id"),
        requests[1].headers.get("x-request-id")
    );
}

#[tokio::test]
async fn token_file_auth_refreshes_and_replays_once_after_upstream_401() {
    let raw_log = NamedTempFile::new().unwrap();
    let token_directory = tempfile::tempdir().unwrap();
    let token_file = NamedTempFile::new_in(token_directory.path().canonicalize().unwrap()).unwrap();
    fs::write(
        token_file.path(),
        format!(
            r#"{{"githubToken":"github-secret-that-must-not-log","copilotToken":"stale-copilot-token-that-must-not-log","expiresAt":{}}}"#,
            future_epoch_seconds()
        ),
    )
    .unwrap();
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let refreshed_future = future_epoch_seconds();
    let upstream_sse = "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
    let mock = spawn_router({
        let recorded = recorded.clone();
        let attempts = attempts.clone();
        let upstream_sse = upstream_sse.to_owned();
        Router::new()
            .route(
                "/responses",
                post(move |headers: HeaderMap, body: Bytes| {
                    let recorded = recorded.clone();
                    let attempts = attempts.clone();
                    let upstream_sse = upstream_sse.clone();
                    async move {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        recorded.lock().unwrap().push(RecordedRequest {
                            headers,
                            body: body.to_vec(),
                        });
                        if attempt == 0 {
                            Response::builder()
                                .status(StatusCode::UNAUTHORIZED)
                                .body(Body::from("old token rejected"))
                                .unwrap()
                        } else {
                            Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "text/event-stream")
                                .body(Body::from(upstream_sse))
                                .unwrap()
                        }
                    }
                }),
            )
            .route(
                "/copilot_internal/v2/token",
                get(move || async move {
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        format!(
                            r#"{{"token":"fresh-copilot-token-that-must-not-log","expires_at":{refreshed_future},"endpoints":{{"api":"https://api.business.githubcopilot.com"}}}}"#
                        ),
                    )
                }),
            )
    })
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.auth.bearer_token = None;
    config.auth.token_file = token_file.path().to_path_buf();
    config.auth.copilot_token_url = mock.url("/copilot_internal/v2/token");
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","input":"auth-retry-body-secret"}"#;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, upstream_sse);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, request_body);
    assert_eq!(requests[1].body, request_body);
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        "Bearer stale-copilot-token-that-must-not-log"
    );
    assert_eq!(
        requests[1].headers.get("authorization").unwrap(),
        "Bearer fresh-copilot-token-that-must-not-log"
    );

    let saved: Value =
        serde_json::from_str(&fs::read_to_string(token_file.path()).unwrap()).unwrap();
    assert_eq!(saved["githubToken"], "github-secret-that-must-not-log");
    assert_eq!(
        saved["copilotToken"],
        "fresh-copilot-token-that-must-not-log"
    );
    assert_eq!(saved["expiresAt"], refreshed_future);
    assert_eq!(
        saved["endpoint"],
        "https://api.business.githubcopilot.com/chat/completions"
    );

    let text = raw_log_text(&raw_log);
    assert!(text.contains("upstream_auth_refresh_retry"));
    assert!(text.contains("auth_refreshed"));
    assert_no_log_secret(&text, "github-secret-that-must-not-log");
    assert_no_log_secret(&text, "stale-copilot-token-that-must-not-log");
    assert_no_log_secret(&text, "fresh-copilot-token-that-must-not-log");
    assert_no_log_secret(&text, "auth-retry-body-secret");
    let inbound = raw_event_fields(&text, "inbound_request");
    let retry = raw_event_fields(&text, "upstream_auth_refresh_retry");
    let ready = raw_event_fields(&text, "upstream_response_ready");
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0]["status"], StatusCode::UNAUTHORIZED.as_u16());
    assert_eq!(retry[0]["local_id"], inbound[0]["local_id"]);
    assert_eq!(ready[0]["local_id"], inbound[0]["local_id"]);
    assert_eq!(ready[0]["status"], StatusCode::OK.as_u16());
    assert_eq!(ready[0]["attempt_count"], 2);
}

#[tokio::test]
async fn token_file_auth_refresh_after_401_is_bounded_to_one_retry() {
    let token_directory = tempfile::tempdir().unwrap();
    let token_file = NamedTempFile::new_in(token_directory.path().canonicalize().unwrap()).unwrap();
    fs::write(
        token_file.path(),
        format!(
            r#"{{"githubToken":"github-secret","copilotToken":"stale-copilot-token","expiresAt":{}}}"#,
            future_epoch_seconds()
        ),
    )
    .unwrap();
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let refreshed_future = future_epoch_seconds();
    let mock = spawn_router({
        let recorded = recorded.clone();
        let attempts = attempts.clone();
        Router::new()
            .route(
                "/responses",
                post(move |headers: HeaderMap, body: Bytes| {
                    let recorded = recorded.clone();
                    let attempts = attempts.clone();
                    async move {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        recorded.lock().unwrap().push(RecordedRequest {
                            headers,
                            body: body.to_vec(),
                        });
                        Response::builder()
                            .status(StatusCode::UNAUTHORIZED)
                            .body(Body::from(format!("rejected attempt {attempt}")))
                            .unwrap()
                    }
                }),
            )
            .route(
                "/copilot_internal/v2/token",
                get(move || async move {
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        format!(
                            r#"{{"token":"fresh-but-rejected","expires_at":{refreshed_future}}}"#
                        ),
                    )
                }),
            )
    })
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.auth.bearer_token = None;
    config.auth.token_file = token_file.path().to_path_buf();
    config.auth.copilot_token_url = mock.url("/copilot_internal/v2/token");
    let server = spawn_app(config).await;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, "rejected attempt 1");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        "Bearer stale-copilot-token"
    );
    assert_eq!(
        requests[1].headers.get("authorization").unwrap(),
        "Bearer fresh-but-rejected"
    );
}

#[tokio::test]
async fn non_token_file_auth_returns_401_without_reactive_refresh() {
    let env_attempts = Arc::new(AtomicUsize::new(0));
    let env_mock = spawn_router({
        let env_attempts = env_attempts.clone();
        Router::new().route(
            "/responses",
            post(move || {
                let env_attempts = env_attempts.clone();
                async move {
                    env_attempts.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::UNAUTHORIZED, "env token rejected")
                }
            }),
        )
    })
    .await;
    let env_server = spawn_app(test_config(
        env_mock.url("/models"),
        env_mock.url("/responses"),
    ))
    .await;

    let env_response = authenticated_client()
        .post(env_server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(env_response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(env_response.text().await.unwrap(), "env token rejected");
    assert_eq!(env_attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_raw_diagnostics_include_wait_source_budget_and_correlation_without_body() {
    let raw_log = NamedTempFile::new().unwrap();
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = retry_mock(recorded, attempts.clone(), RetryMode::RetryAfterThenOk).await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_sleep = Duration::from_millis(1);
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","input":"retry-body-secret"}"#;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok after retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    let text = raw_log_text(&raw_log);
    assert!(text.contains("upstream_rate_limited"));
    assert!(text.contains("RetryAfter"));
    assert!(text.contains("local_id"));
    assert_no_log_secret(&text, "service-owned-token");
    assert_no_log_secret(&text, "retry-body-secret");
    let retry = raw_event_fields(&text, "upstream_rate_limited");
    assert_eq!(retry[0]["status"], StatusCode::TOO_MANY_REQUESTS.as_u16());
    assert_eq!(retry[0]["retry_budget_exceeded"], false);
    assert!(retry[0].get("budget_ms").is_some());
    assert!(retry[0].get("upstream_request_id").is_some());
}

#[tokio::test]
async fn responses_retry_uses_fallback_backoff_without_headers() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));

    let mock = retry_mock(recorded, attempts.clone(), RetryMode::BackoffThenOk).await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok after retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn responses_retry_uses_epoch_reset_header_and_preserves_body() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));

    let mock = retry_mock(
        recorded.clone(),
        attempts.clone(),
        RetryMode::ResetHeaderThenOk,
    )
    .await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_sleep = Duration::from_millis(1);
    let server = spawn_app(config).await;
    let request_body = br#"{"model":"gpt-test","input":"retry at reset"}"#;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body(request_body.as_slice().to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok after retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests[0].body, request_body);
    assert_eq!(requests[1].body, request_body);
}

#[tokio::test]
async fn responses_retry_returns_429_after_positive_wait_budget_is_exceeded() {
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));

    let mock = retry_mock(recorded, attempts.clone(), RetryMode::AlwaysRateLimited).await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_total_wait = Duration::from_secs(1);
    config.rate_limit.initial_backoff = Duration::from_secs(10);
    config.rate_limit.max_sleep = Duration::from_secs(10);
    let server = spawn_app(config).await;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body, "still limited");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn budget_exhaustion_raw_diagnostics_record_decision_without_secrets() {
    let raw_log = NamedTempFile::new().unwrap();
    let recorded = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = retry_mock(recorded, attempts.clone(), RetryMode::AlwaysRateLimited).await;
    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.rate_limit.max_total_wait = Duration::from_secs(1);
    config.rate_limit.initial_backoff = Duration::from_secs(10);
    config.rate_limit.max_sleep = Duration::from_secs(10);
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body("budget-body-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.text().await.unwrap(), "still limited");

    let text = raw_log_text(&raw_log);
    assert!(text.contains("upstream_rate_limited"));
    assert_has_terminal_stream_event(&text);
    assert_no_log_secret(&text, "service-owned-token");
    assert_no_log_secret(&text, "budget-body-secret");
    let retry = raw_event_fields(&text, "upstream_rate_limited");
    assert_eq!(retry[0]["retry_budget_exceeded"], true);
}

#[tokio::test]
async fn send_failure_raw_diagnostics_are_correlated_and_redact_url_queries() {
    let raw_log = NamedTempFile::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut config = test_config(
        format!("http://{addr}/models?token=query-secret"),
        format!("http://{addr}/responses?token=query-secret"),
    );
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body("send-failure-body-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let response_body = response.text().await.unwrap();

    let text = raw_log_text(&raw_log);
    assert!(text.contains("upstream_request_failed"));
    assert!(text.contains("local_id"));
    assert_no_log_secret(&text, "service-owned-token");
    assert_no_log_secret(&text, "query-secret");
    assert_no_log_secret(&text, "send-failure-body-secret");
    assert_no_log_secret(&response_body, "query-secret");
    assert_no_log_secret(&response_body, "send-failure-body-secret");
}

#[tokio::test]
async fn non_429_errors_are_not_rate_limit_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::SERVICE_UNAVAILABLE, "not a rate limit")
                }
            }),
        )
    })
    .await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;

    let response = authenticated_client()
        .post(server.url("/v1/responses"))
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn local_client_token_never_falls_back_to_upstream_authorization() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/models",
            get(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "should not be called")
                }
            }),
        )
    })
    .await;

    let mut config = test_config(mock.url("/models"), mock.url("/responses"));
    config.auth.bearer_token = None;
    let server = spawn_app(config).await;

    let response = authenticated_client()
        .get(server.url("/v1/models"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn wrong_method_for_responses_is_rejected_without_calling_upstream() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().route(
            "/responses",
            post(move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "should not be called")
                }
            }),
        )
    })
    .await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;

    let response = authenticated_client()
        .get(server.url("/v1/responses"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "Unsupported local methods should be rejected before any upstream provider call."
    );
}

#[tokio::test]
async fn unsupported_routes_return_not_found_without_calling_upstream() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mock = spawn_router({
        let attempts = attempts.clone();
        Router::new().fallback(move || {
            let attempts = attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }
        })
    })
    .await;
    let server = spawn_app(test_config(mock.url("/models"), mock.url("/responses"))).await;
    let response = authenticated_client()
        .get(server.url("/v1/chat/completions"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

fn combined_client() -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(LOCAL_CLIENT_KEY_HEADER, CLIENT_TOKEN.parse().unwrap());
    headers.insert(
        "authorization",
        "Bearer native-dummy-token".parse().unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

type CombinedRequests = Arc<Mutex<Vec<(String, RecordedRequest)>>>;

async fn combined_upstream(
    status: StatusCode,
    response_headers: &'static [(&'static str, &'static str)],
    response_body: &'static str,
) -> (TestServer, CombinedRequests) {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mock = spawn_router({
        let recorded = recorded.clone();
        Router::new().fallback(
            move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push((
                        uri.to_string(),
                        RecordedRequest {
                            headers,
                            body: body.to_vec(),
                        },
                    ));
                    let mut response = Response::builder().status(status);
                    for (name, value) in response_headers {
                        response = response.header(*name, *value);
                    }
                    response.body(Body::from(response_body)).unwrap()
                }
            },
        )
    })
    .await;
    (mock, recorded)
}

fn combined_config(mock: &TestServer) -> AppConfig {
    let mut config = test_config(mock.url("/copilot/models"), mock.url("/copilot/responses"));
    config.upstream_native_base_url = mock.url("/native/");
    config.auth.copilot_token_url = mock.url("/github/token");
    config
}

#[tokio::test]
async fn combined_catalog_is_loaded_once_and_new_alias_routes_with_isolated_credentials() {
    let (mock, recorded) = combined_upstream(StatusCode::OK, &[], "result").await;
    let mut catalog = NamedTempFile::new().unwrap();
    catalog
        .write_all(br#"{"models":[{"slug":"copilot/future-eligible"}]}"#)
        .unwrap();
    let mut config = combined_config(&mock);
    config.copilot_catalog_file = catalog.path().to_path_buf();
    let server = spawn_app(config).await;
    catalog.as_file().set_len(0).unwrap();
    let response = combined_client()
        .post(server.url("/combined/v1/responses"))
        .body(r#" { "model":"copilot/future-eligible", "opaque":1e400 } "#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/copilot/responses");
    assert_eq!(
        requests[0].1.body,
        br#" { "model":"future-eligible", "opaque":1e400 } "#
    );
    assert_eq!(
        requests[0].1.headers.get("authorization").unwrap(),
        "Bearer service-owned-token"
    );
    assert!(!requests[0].1.headers.contains_key(LOCAL_CLIENT_KEY_HEADER));
}

#[tokio::test]
async fn missing_or_invalid_catalog_keeps_native_and_pure_copilot_usable() {
    for contents in [
        None,
        Some("invalid catalog containing a secret"),
        Some(r#"{"models":[]}"#),
    ] {
        let (mock, recorded) = combined_upstream(StatusCode::OK, &[], "result").await;
        let dir = tempfile::tempdir().unwrap();
        let mut file = NamedTempFile::new_in(dir.path()).unwrap();
        let mut config = combined_config(&mock);
        config.copilot_catalog_file = dir.path().join("catalog.json");
        if let Some(contents) = contents {
            config.copilot_catalog_file = file.path().to_path_buf();
            file.write_all(contents.as_bytes()).unwrap();
        }
        let server = spawn_app(config).await;
        let response = combined_client()
            .post(server.url("/combined/v1/responses"))
            .body(r#"{"model":"copilot/gpt-6-astra"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if contents == Some(r#"{"models":[]}"#) {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
        );
        assert!(recorded.lock().unwrap().is_empty());
        let error: Value = response.json().await.unwrap();
        assert_eq!(
            error["error"],
            if contents == Some(r#"{"models":[]}"#) {
                "unknown_copilot_model"
            } else {
                "copilot_catalog_unavailable"
            }
        );
        let native = combined_client()
            .post(server.url("/combined/v1/responses"))
            .body(r#" {"model":"native","opaque":1e400} "#)
            .send()
            .await
            .unwrap();
        assert_eq!(native.status(), StatusCode::OK);
        let pure_copilot = authenticated_client()
            .post(server.url("/v1/responses"))
            .body(r#"{"model":"raw-copilot-model"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(pure_copilot.status(), StatusCode::OK);
        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "/native/responses");
        assert_eq!(
            requests[0].1.body,
            br#" {"model":"native","opaque":1e400} "#
        );
        assert_eq!(requests[1].0, "/copilot/responses");
    }
}

#[tokio::test]
async fn combined_native_preserves_body_auth_and_account_without_copilot_credentials() {
    let sse = "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
    let (mock, recorded) = combined_upstream(
        StatusCode::OK,
        &[("content-type", "text/event-stream")],
        sse,
    )
    .await;
    let mut config = combined_config(&mock);
    config.auth.bearer_token = None;
    let server = spawn_app(config).await;
    let body = b"{ \"model\" : \"gpt-6-astra\", \"input\": [{\"type\":\"message\",\"content\":\"hello\"}], \"opaque\":1e+04 }\n";
    let response = combined_client()
        .post(server.url("/combined/v1/responses"))
        .header("chatgpt-account-id", "native-account")
        .header("openai-beta", "responses=experimental")
        .header("content-type", "application/json")
        .body(body.as_slice().to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), sse.as_bytes());
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/native/responses");
    assert_eq!(requests[0].1.body, body);
    let headers = &requests[0].1.headers;
    assert_eq!(
        headers.get("authorization").unwrap(),
        "Bearer native-dummy-token"
    );
    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "native-account");
    assert_eq!(
        headers.get("openai-beta").unwrap(),
        "responses=experimental"
    );
    assert!(!headers.contains_key(LOCAL_CLIENT_KEY_HEADER));
    assert!(!headers.contains_key("copilot-integration-id"));
}

#[tokio::test]
async fn combined_copilot_rewrites_only_alias_and_uses_only_service_credentials() {
    let (mock, recorded) = combined_upstream(StatusCode::OK, &[], "copilot result").await;
    let server = spawn_app(combined_config(&mock)).await;
    for model in ["gpt-6-astra", "gpt-5.6-sol", "example-model"] {
        let body = format!("{{ \"input\":\"copilot/keep-me\", \"model\" : \"copilot/{model}\", \"extra\":{{\"model\":\"copilot/nested\"}}, \"n\":1e+04 }}\n");
        let expected = format!("{{ \"input\":\"copilot/keep-me\", \"model\" : \"{model}\", \"extra\":{{\"model\":\"copilot/nested\"}}, \"n\":1e+04 }}\n");
        let response = combined_client()
            .post(server.url("/combined/v1/responses"))
            .header("chatgpt-account-id", "native-account")
            .header("openai-organization", "native-organization")
            .header("openai-project", "native-project")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "copilot result");
        let requests = recorded.lock().unwrap();
        let (path, request) = requests.last().unwrap();
        assert_eq!(path, "/copilot/responses");
        assert_eq!(request.body, expected.as_bytes());
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            "Bearer service-owned-token"
        );
        for name in [
            LOCAL_CLIENT_KEY_HEADER,
            "chatgpt-account-id",
            "openai-organization",
            "openai-project",
        ] {
            assert!(!request.headers.contains_key(name), "leaked {name}");
        }
    }
    assert_eq!(recorded.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn combined_local_key_is_unique_and_checked_before_body_or_upstream() {
    let (mock, recorded) = combined_upstream(StatusCode::OK, &[], "unexpected").await;
    let mut config = combined_config(&mock);
    config.request_body_limit_bytes = 1024;
    let server = spawn_app(config).await;
    let client = reqwest::Client::new();
    for key in [
        None,
        Some("wrong-key"),
        Some("f123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
    ] {
        let mut request = client
            .post(server.url("/combined/v1/responses"))
            .header("authorization", format!("Bearer {CLIENT_TOKEN}"))
            .header("content-encoding", "gzip")
            .body(vec![b'x'; 1025]);
        if let Some(key) = key {
            request = request.header(LOCAL_CLIENT_KEY_HEADER, key);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let response = combined_client()
        .post(server.url("/combined/v1/responses"))
        .header(LOCAL_CLIENT_KEY_HEADER, CLIENT_TOKEN)
        .header(LOCAL_CLIENT_KEY_HEADER, CLIENT_TOKEN)
        .body("{\"model\":\"gpt-6-astra\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn combined_rejects_browser_host_body_limit_and_encoded_payloads() {
    let (mock, recorded) = combined_upstream(StatusCode::OK, &[], "unexpected").await;
    let mut config = combined_config(&mock);
    config.request_body_limit_bytes = 1024;
    let server = spawn_app(config).await;
    for (name, value) in [
        ("origin", "null"),
        ("sec-fetch-site", "same-origin"),
        ("host", "attacker.example"),
    ] {
        let response = combined_client()
            .post(server.url("/combined/v1/responses"))
            .header(name, value)
            .body("{\"model\":\"gpt-6-astra\"}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let response = combined_client()
        .post(server.url("/combined/v1/responses"))
        .body(vec![b'x'; 1025])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    for encoding in ["gzip", "br", "identity, gzip"] {
        let response = combined_client()
            .post(server.url("/combined/v1/responses"))
            .header("content-encoding", encoding)
            .body("opaque encoded data")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn combined_native_requires_one_valid_native_bearer() {
    let (mock, recorded) = combined_upstream(StatusCode::OK, &[], "unexpected").await;
    let server = spawn_app(combined_config(&mock)).await;
    for authorization in [None, Some("Basic native-dummy-token"), Some("Bearer ")] {
        let mut request = reqwest::Client::new()
            .post(server.url("/combined/v1/responses"))
            .header(LOCAL_CLIENT_KEY_HEADER, CLIENT_TOKEN)
            .body("{\"model\":\"gpt-6-astra\"}");
        if let Some(value) = authorization {
            request = request.header("authorization", value);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let response = combined_client()
        .post(server.url("/combined/v1/responses"))
        .header("authorization", "Bearer native-dummy-token")
        .header("authorization", "Bearer another-native-token")
        .body("{\"model\":\"gpt-6-astra\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn combined_routing_denials_and_unknown_paths_never_fall_back() {
    let (mock, recorded) = combined_upstream(StatusCode::OK, &[], "unexpected").await;
    let server = spawn_app(combined_config(&mock)).await;
    for body in [
        "not-json",
        "{}",
        "{\"model\":\"copilot/unknown\"}",
        "{\"model\":\"gpt-6-astra\",\"model\":\"copilot/gpt-6-astra\"}",
    ] {
        let response = combined_client()
            .post(server.url("/combined/v1/responses"))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    for path in [
        "/combined/v1/models",
        "/combined/v1/chat/completions",
        "/combined/v1/responses/unknown",
    ] {
        let response = combined_client()
            .post(server.url(path))
            .body("{\"model\":\"gpt-6-astra\"}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn combined_native_auxiliary_operations_have_fixed_targets_and_reject_copilot() {
    let (mock, recorded) = combined_upstream(StatusCode::OK, &[], "native result").await;
    let server = spawn_app(combined_config(&mock)).await;
    for operation in ["compact", "lite"] {
        let response = combined_client()
            .post(server.url(&format!(
                "/combined/v1/responses/{operation}?target=ignored"
            )))
            .header("content-encoding", "identity")
            .body("{ \"model\": \"gpt-6-astra\", \"input\": [] }\n")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = combined_client()
            .post(server.url(&format!("/combined/v1/responses/{operation}")))
            .body("{\"model\":\"copilot/gpt-6-astra\"}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0, "/native/responses/compact");
    assert_eq!(requests[1].0, "/native/responses/lite");
    for (_, request) in requests.iter() {
        assert_eq!(
            request.body,
            b"{ \"model\": \"gpt-6-astra\", \"input\": [] }\n"
        );
    }
}

#[tokio::test]
async fn combined_native_401_reaches_codex_without_copilot_refresh() {
    let (mock, recorded) = combined_upstream(
        StatusCode::UNAUTHORIZED,
        &[("www-authenticate", "Bearer error=invalid_token")],
        "native login expired",
    )
    .await;
    let server = spawn_app(combined_config(&mock)).await;
    let response = combined_client()
        .post(server.url("/combined/v1/responses"))
        .body("{\"model\":\"gpt-6-astra\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get("www-authenticate").unwrap(),
        "Bearer error=invalid_token"
    );
    assert_eq!(response.text().await.unwrap(), "native login expired");
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/native/responses");
}

#[tokio::test]
async fn combined_copilot_auth_failures_never_signal_native_refresh_or_fall_back() {
    let (mock, recorded) = combined_upstream(
        StatusCode::UNAUTHORIZED,
        &[("www-authenticate", "Bearer error=invalid_token")],
        "provider token detail",
    )
    .await;
    let expired = NamedTempFile::new().unwrap();
    fs::write(
        expired.path(),
        r#"{"copilotToken":"expired-dummy-token","expiresAt":1}"#,
    )
    .unwrap();
    for mode in ["missing", "expired", "rejected"] {
        let mut config = combined_config(&mock);
        if mode != "rejected" {
            config.auth.bearer_token = None;
        }
        if mode == "expired" {
            config.auth.token_file = expired.path().to_path_buf();
        }
        let server = spawn_app(config).await;
        let response = combined_client()
            .post(server.url("/combined/v1/responses"))
            .body("{\"model\":\"copilot/gpt-6-astra\"}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{mode}");
        assert!(!response.headers().contains_key("www-authenticate"));
        let body = response.text().await.unwrap();
        assert!(body.contains("copilot_auth_"));
        assert!(!body.contains("provider token detail"));
        assert!(!body.contains("expired-dummy-token"));
    }
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/copilot/responses");
}

#[tokio::test]
async fn combined_malformed_copilot_auth_never_exposes_token_file_values() {
    let (mock, recorded) = combined_upstream(StatusCode::OK, &[], "unexpected").await;
    let token_file = NamedTempFile::new().unwrap();
    let raw_log = NamedTempFile::new().unwrap();
    fs::write(token_file.path(), r#""private-token-value""#).unwrap();
    let mut config = combined_config(&mock);
    config.auth.bearer_token = None;
    config.auth.token_file = token_file.path().to_path_buf();
    enable_raw_log(&mut config, &raw_log);
    let server = spawn_app(config).await;
    let response = combined_client()
        .post(server.url("/combined/v1/responses"))
        .body("{\"model\":\"copilot/gpt-6-astra\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!response
        .text()
        .await
        .unwrap()
        .contains("private-token-value"));
    assert_no_log_secret(&raw_log_text(&raw_log), "private-token-value");
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn combined_copilot_terminal_401_refreshes_only_copilot_once() {
    let token_directory = tempfile::tempdir().unwrap();
    let token_file = NamedTempFile::new_in(token_directory.path().canonicalize().unwrap()).unwrap();
    fs::write(
        token_file.path(),
        format!(
            r#"{{"githubToken":"github-dummy","copilotToken":"old-dummy","expiresAt":{}}}"#,
            future_epoch_seconds()
        ),
    )
    .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mock = spawn_router({
        let calls = calls.clone();
        Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap| {
            let calls = calls.clone();
            async move {
                calls.lock().unwrap().push((uri.path().to_owned(), headers));
                if uri.path() == "/github/token" {
                    (
                        StatusCode::OK,
                        format!(
                            r#"{{"token":"fresh-dummy","expires_at":{}}}"#,
                            future_epoch_seconds()
                        ),
                    )
                        .into_response()
                } else {
                    (StatusCode::UNAUTHORIZED, "copilot rejection").into_response()
                }
            }
        })
    })
    .await;
    let mut config = combined_config(&mock);
    config.auth.bearer_token = None;
    config.auth.token_file = token_file.path().to_path_buf();
    let server = spawn_app(config).await;
    let response = combined_client()
        .post(server.url("/combined/v1/responses"))
        .body("{\"model\":\"copilot/gpt-6-astra\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].0, "/copilot/responses");
    assert_eq!(calls[1].0, "/github/token");
    assert_eq!(calls[2].0, "/copilot/responses");
    assert_eq!(calls[0].1.get("authorization").unwrap(), "Bearer old-dummy");
    assert_eq!(
        calls[1].1.get("authorization").unwrap(),
        "Bearer github-dummy"
    );
    assert_eq!(
        calls[2].1.get("authorization").unwrap(),
        "Bearer fresh-dummy"
    );
}

#[tokio::test]
async fn upstream_redirects_are_never_followed_or_exposed_on_either_api() {
    let (destination, destination_requests) =
        combined_upstream(StatusCode::OK, &[], "redirect leaked").await;
    let location = destination.url("/must-not-receive-credentials");
    let mock = spawn_router(Router::new().fallback(move || {
        let location = location.clone();
        async move {
            Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header("location", location)
                .body(Body::empty())
                .unwrap()
        }
    }))
    .await;
    let server = spawn_app(combined_config(&mock)).await;
    for (path, model, client) in [
        ("/v1/responses", "gpt-6-astra", authenticated_client()),
        ("/combined/v1/responses", "gpt-6-astra", combined_client()),
        (
            "/combined/v1/responses",
            "copilot/gpt-6-astra",
            combined_client(),
        ),
    ] {
        let response = client
            .post(server.url(path))
            .body(format!(r#"{{"model":"{model}"}}"#))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(!response.headers().contains_key("location"));
    }
    assert!(destination_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn combined_native_zero_delay_retries_remain_bounded_with_identical_body() {
    let (mock, recorded) = combined_upstream(
        StatusCode::TOO_MANY_REQUESTS,
        &[("retry-after", "0")],
        "rate limited",
    )
    .await;
    let server = spawn_app(combined_config(&mock)).await;
    let body = "{ \"model\" : \"gpt-6-astra\", \"input\" : [] }\n";
    let response = combined_client()
        .post(server.url("/combined/v1/responses"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 9);
    for (path, request) in requests.iter() {
        assert_eq!(path, "/native/responses");
        assert_eq!(request.body, body.as_bytes());
    }
}

struct CliServer(std::process::Child);

impl Drop for CliServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn serve_cli_starts_native_requests_without_github_login_or_copilot_file() {
    let (mock, recorded) = combined_upstream(StatusCode::OK, &[], "native from CLI").await;
    let token_file = client_token_file();
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_ccrx"))
        .arg("serve")
        .env_clear()
        .env("HOST", "127.0.0.1")
        .env("PORT", addr.port().to_string())
        .env("HOME", directory.path())
        .env("CODEX_CODE_ROUTER_CLIENT_TOKEN_FILE", token_file.path())
        .env(
            "COPILOT_TOKEN_FILE",
            directory.path().join("missing-copilot.json"),
        )
        .env("NATIVE_OPENAI_BASE_URL", mock.url("/native"))
        .env("COPILOT_RESPONSES_URL", mock.url("/copilot/responses"))
        .env("COPILOT_TOKEN_URL", mock.url("/github/token"))
        .env("GITHUB_DEVICE_CODE_URL", mock.url("/github/device"))
        .env("GITHUB_ACCESS_TOKEN_URL", mock.url("/github/access"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut child = CliServer(child);
    let client = combined_client();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let response = loop {
        let result = client
            .post(format!("http://{addr}/combined/v1/responses"))
            .body("{\"model\":\"gpt-6-astra\"}")
            .timeout(Duration::from_millis(250))
            .send()
            .await;
        if let Ok(response) = result {
            break response;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "serve exited before accepting native requests"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "serve did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "native from CLI");
    let requests = recorded.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/native/responses");
}

#[derive(Clone, Copy)]
enum RetryMode {
    RetryAfterThenOk,
    ResetHeaderThenOk,
    BackoffThenOk,
    AlwaysRateLimited,
}

async fn retry_mock(
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    attempts: Arc<AtomicUsize>,
    mode: RetryMode,
) -> TestServer {
    spawn_router(Router::new().route(
        "/responses",
        post(move |headers: HeaderMap, body: Bytes| {
            let recorded = recorded.clone();
            let attempts = attempts.clone();
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                recorded.lock().unwrap().push(RecordedRequest {
                    headers,
                    body: body.to_vec(),
                });

                match (mode, attempt) {
                    (RetryMode::RetryAfterThenOk, 0) => Response::builder()
                        .status(StatusCode::TOO_MANY_REQUESTS)
                        .header("retry-after", "1")
                        .body(Body::from("limited once"))
                        .unwrap(),
                    (RetryMode::ResetHeaderThenOk, 0) => {
                        let reset_epoch_seconds = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            + 60;
                        Response::builder()
                            .status(StatusCode::TOO_MANY_REQUESTS)
                            .header("x-ratelimit-reset", reset_epoch_seconds.to_string())
                            .body(Body::from("limited until reset"))
                            .unwrap()
                    }
                    (RetryMode::BackoffThenOk, 0) => {
                        (StatusCode::TOO_MANY_REQUESTS, "limited once").into_response()
                    }
                    (RetryMode::AlwaysRateLimited, _) => {
                        (StatusCode::TOO_MANY_REQUESTS, "still limited").into_response()
                    }
                    _ => (StatusCode::OK, "ok after retry").into_response(),
                }
            }
        }),
    ))
    .await
}
