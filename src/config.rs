use crate::redaction::redact_url;
use std::env;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 60001;
pub const DEFAULT_RESPONSES_URL: &str = "https://api.githubcopilot.com/responses";
pub const DEFAULT_MODELS_URL: &str = "https://api.githubcopilot.com/models";
pub const DEFAULT_NATIVE_OPENAI_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const DEFAULT_COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
pub const DEFAULT_GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
pub const DEFAULT_GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
pub const DEFAULT_GITHUB_OAUTH_CLIENT_ID: &str = "01ab8ac9400c4e429b23";
pub const DEFAULT_GITHUB_OAUTH_SCOPE: &str = "read:user";
pub const DEFAULT_REQUEST_BODY_LIMIT_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_RETRY_BUDGET: Duration = Duration::from_secs(30);
pub const DEFAULT_RAW_LOG_MAX_BYTES: usize = 64 * 1024;
pub const DEFAULT_RAW_LOG_CONTENT_MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawLogLevel {
    Off,
    Metadata,
    ContentRedacted,
    FullContent,
}

impl RawLogLevel {
    pub fn allows_metadata(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn allows_content(self) -> bool {
        matches!(self, Self::ContentRedacted | Self::FullContent)
    }

    pub fn is_full_content(self) -> bool {
        matches!(self, Self::FullContent)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Metadata => "metadata",
            Self::ContentRedacted => "content_redacted",
            Self::FullContent => "full_content",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub upstream_responses_url: Option<String>,
    pub upstream_models_url: Option<String>,
    pub upstream_native_base_url: String,
    pub request_timeout: Duration,
    pub request_body_limit_bytes: usize,
    pub client_token_file: PathBuf,
    pub copilot_catalog_file: PathBuf,
    pub headers: CopilotHeaderConfig,
    pub auth: AuthConfig,
    pub rate_limit: RateLimitConfig,
    pub raw_log: RawLogConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopilotHeaderConfig {
    pub copilot_chat_version: String,
    pub copilot_editor_version: String,
    pub github_api_version: String,
}

#[derive(Clone)]
pub struct AuthConfig {
    pub bearer_token: Option<String>,
    pub token_file: PathBuf,
    pub token_expiry_buffer: Duration,
    pub refresh_enabled: bool,
    pub copilot_token_url: String,
    pub github_device_code_url: String,
    pub github_access_token_url: String,
    pub github_oauth_client_id: String,
    pub github_oauth_scope: String,
}

#[derive(Clone, Debug)]
pub struct RateLimitConfig {
    pub max_total_wait: Duration,
    pub max_sleep: Duration,
    pub initial_backoff: Duration,
    pub backoff_multiplier: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawLogConfig {
    pub level: RawLogLevel,
    pub file: PathBuf,
    pub max_bytes: usize,
    pub content_max_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppConfigSummary {
    pub endpoint: String,
    pub upstream_responses_url: Option<String>,
    pub upstream_models_url: Option<String>,
    pub upstream_native_base_url: String,
    pub request_timeout_ms: u128,
    pub request_body_limit_bytes: usize,
    pub headers: CopilotHeaderConfig,
    pub auth: AuthConfigSummary,
    pub rate_limit: RateLimitConfigSummary,
    pub raw_log: RawLogConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthConfigSummary {
    pub bearer_token_configured: bool,
    pub token_file: PathBuf,
    pub token_expiry_buffer_seconds: u64,
    pub refresh_enabled: bool,
    pub copilot_token_url: String,
    pub github_device_code_url: String,
    pub github_access_token_url: String,
    pub github_oauth_client_id_configured: bool,
    pub github_oauth_scope: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateLimitConfigSummary {
    pub max_total_wait_ms: u128,
    pub max_sleep_ms: u128,
    pub initial_backoff_ms: u128,
    pub backoff_multiplier: String,
}

impl AppConfig {
    pub fn from_env() -> Self {
        Self {
            host: read_string("HOST", DEFAULT_HOST),
            port: read_u16("PORT", DEFAULT_PORT),
            upstream_responses_url: read_optional_string("COPILOT_RESPONSES_URL"),
            upstream_models_url: read_optional_string("COPILOT_MODELS_URL"),
            upstream_native_base_url: read_string(
                "NATIVE_OPENAI_BASE_URL",
                DEFAULT_NATIVE_OPENAI_BASE_URL,
            ),
            request_timeout: read_duration_ms("REQUEST_TIMEOUT_MS", 300_000),
            request_body_limit_bytes: read_request_body_limit_bytes(),
            client_token_file: read_client_token_file_path(),
            copilot_catalog_file: read_copilot_catalog_file_path(),
            headers: CopilotHeaderConfig {
                copilot_chat_version: read_string("COPILOT_CHAT_VERSION", "0.35.0"),
                copilot_editor_version: read_string("COPILOT_EDITOR_VERSION", "vscode/1.109.2"),
                github_api_version: read_string("GITHUB_API_VERSION", "2025-10-01"),
            },
            auth: AuthConfig {
                bearer_token: read_optional_string("COPILOT_BEARER_TOKEN"),
                token_file: read_token_file_path(),
                token_expiry_buffer: read_duration_seconds(
                    "COPILOT_TOKEN_EXPIRY_BUFFER_SECONDS",
                    300,
                ),
                refresh_enabled: read_bool("COPILOT_TOKEN_REFRESH", true),
                copilot_token_url: read_string("COPILOT_TOKEN_URL", DEFAULT_COPILOT_TOKEN_URL),
                github_device_code_url: read_string(
                    "GITHUB_DEVICE_CODE_URL",
                    DEFAULT_GITHUB_DEVICE_CODE_URL,
                ),
                github_access_token_url: read_string(
                    "GITHUB_ACCESS_TOKEN_URL",
                    DEFAULT_GITHUB_ACCESS_TOKEN_URL,
                ),
                github_oauth_client_id: read_string(
                    "GITHUB_OAUTH_CLIENT_ID",
                    DEFAULT_GITHUB_OAUTH_CLIENT_ID,
                ),
                github_oauth_scope: read_string("GITHUB_OAUTH_SCOPE", DEFAULT_GITHUB_OAUTH_SCOPE),
            },
            rate_limit: RateLimitConfig {
                max_total_wait: read_max_total_wait(),
                max_sleep: read_duration_ms("RATE_LIMIT_MAX_SLEEP_MS", 60_000),
                initial_backoff: read_duration_ms("RATE_LIMIT_INITIAL_BACKOFF_MS", 1_000),
                backoff_multiplier: read_f64("RATE_LIMIT_BACKOFF_MULTIPLIER", 2.0, 1.0),
            },
            raw_log: RawLogConfig {
                level: read_raw_log_level("CODEX_CODE_ROUTER_RAW_LOG_LEVEL"),
                file: read_raw_log_file_path(),
                max_bytes: read_usize(
                    "CODEX_CODE_ROUTER_RAW_LOG_MAX_BYTES",
                    DEFAULT_RAW_LOG_MAX_BYTES,
                    1024,
                ),
                content_max_bytes: read_usize(
                    "CODEX_CODE_ROUTER_RAW_LOG_CONTENT_MAX_BYTES",
                    DEFAULT_RAW_LOG_CONTENT_MAX_BYTES,
                    1024,
                ),
            },
        }
    }

    pub fn safe_summary(&self) -> AppConfigSummary {
        AppConfigSummary {
            endpoint: format!("http://{}:{}", self.host, self.port),
            upstream_responses_url: self.upstream_responses_url.as_deref().map(redact_url),
            upstream_models_url: self.upstream_models_url.as_deref().map(redact_url),
            upstream_native_base_url: redact_url(&self.upstream_native_base_url),
            request_timeout_ms: self.request_timeout.as_millis(),
            request_body_limit_bytes: self.request_body_limit_bytes,
            headers: self.headers.clone(),
            auth: self.auth.safe_summary(),
            rate_limit: self.rate_limit.safe_summary(),
            raw_log: self.raw_log.clone(),
        }
    }
}

impl AuthConfig {
    pub fn safe_summary(&self) -> AuthConfigSummary {
        AuthConfigSummary {
            bearer_token_configured: self.bearer_token.is_some(),
            token_file: self.token_file.clone(),
            token_expiry_buffer_seconds: self.token_expiry_buffer.as_secs(),
            refresh_enabled: self.refresh_enabled,
            copilot_token_url: redact_url(&self.copilot_token_url),
            github_device_code_url: redact_url(&self.github_device_code_url),
            github_access_token_url: redact_url(&self.github_access_token_url),
            github_oauth_client_id_configured: !self.github_oauth_client_id.is_empty(),
            github_oauth_scope: self.github_oauth_scope.clone(),
        }
    }
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.safe_summary().fmt(f)
    }
}

impl RateLimitConfig {
    pub fn safe_summary(&self) -> RateLimitConfigSummary {
        RateLimitConfigSummary {
            max_total_wait_ms: self.max_total_wait.as_millis(),
            max_sleep_ms: self.max_sleep.as_millis(),
            initial_backoff_ms: self.initial_backoff.as_millis(),
            backoff_multiplier: self.backoff_multiplier.to_string(),
        }
    }
}

fn read_string(name: &str, fallback: &str) -> String {
    read_optional_string(name).unwrap_or_else(|| fallback.to_owned())
}

fn read_optional_string(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_u16(name: &str, fallback: u16) -> u16 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn read_duration_ms(name: &str, fallback_ms: u64) -> Duration {
    let value = env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback_ms);
    Duration::from_millis(value)
}

fn read_duration_seconds(name: &str, fallback_seconds: u64) -> Duration {
    let value = env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(fallback_seconds);
    Duration::from_secs(value)
}

fn read_request_body_limit_bytes() -> usize {
    env::var("REQUEST_BODY_LIMIT_BYTES")
        .ok()
        .and_then(|raw| parse_request_body_limit_bytes(&raw))
        .unwrap_or(DEFAULT_REQUEST_BODY_LIMIT_BYTES)
}

fn parse_request_body_limit_bytes(raw: &str) -> Option<usize> {
    raw.trim()
        .parse::<usize>()
        .ok()
        .filter(|value| *value >= 1024)
}

fn read_max_total_wait() -> Duration {
    parse_max_total_wait(env::var("RATE_LIMIT_MAX_TOTAL_WAIT_MS").ok().as_deref())
}

fn parse_max_total_wait(raw: Option<&str>) -> Duration {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_RETRY_BUDGET)
}

fn read_f64(name: &str, fallback: f64, minimum: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= minimum)
        .unwrap_or(fallback)
}

fn read_usize(name: &str, fallback: usize, minimum: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= minimum)
        .unwrap_or(fallback)
}

fn read_bool(name: &str, fallback: bool) -> bool {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .and_then(|value| match value.as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(fallback)
}

fn read_raw_log_level(name: &str) -> RawLogLevel {
    let raw = env::var(name).ok().unwrap_or_default();
    if raw.trim().is_empty() {
        return RawLogLevel::Off;
    }

    parse_raw_log_level(&raw).unwrap_or_else(|| {
        panic!(
            "Invalid value for {name}: {}. Expected one of: off, metadata, content_redacted, full_content",
            raw.trim()
        )
    })
}

fn parse_raw_log_level(raw: &str) -> Option<RawLogLevel> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" => Some(RawLogLevel::Off),
        "metadata" => Some(RawLogLevel::Metadata),
        "content_redacted" => Some(RawLogLevel::ContentRedacted),
        "full_content" => Some(RawLogLevel::FullContent),
        _ => None,
    }
}

fn read_token_file_path() -> PathBuf {
    if let Some(path) = read_optional_string("COPILOT_TOKEN_FILE") {
        return PathBuf::from(path);
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".copilot-tokens.json")
}

fn read_client_token_file_path() -> PathBuf {
    if let Some(path) = read_optional_string("CODEX_CODE_ROUTER_CLIENT_TOKEN_FILE") {
        return PathBuf::from(path);
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share/codex-code-router/client-token")
}

fn read_copilot_catalog_file_path() -> PathBuf {
    if let Some(path) = read_optional_string("CODEX_CODE_ROUTER_MODEL_CATALOG") {
        return PathBuf::from(path);
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share/codex-code-router/desktop-models.json")
}

fn read_raw_log_file_path() -> PathBuf {
    if let Some(path) = read_optional_string("CODEX_CODE_ROUTER_RAW_LOG_FILE") {
        return PathBuf::from(path);
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex-code-router")
        .join("raw")
        .join("diagnostics.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_debug_and_summary_do_not_expose_bearer_token() {
        let auth = AuthConfig {
            bearer_token: Some("Bearer secret-token-that-must-not-log".to_owned()),
            token_file: PathBuf::from("/tmp/copilot-tokens.json"),
            token_expiry_buffer: Duration::from_secs(300),
            refresh_enabled: true,
            copilot_token_url: "https://api.example.test/token?access_token=secret".to_owned(),
            github_device_code_url: "https://github.example.test/device?device_code=secret"
                .to_owned(),
            github_access_token_url: "https://github.example.test/access?client_secret=secret"
                .to_owned(),
            github_oauth_client_id: "client-id-not-secret-but-not-needed".to_owned(),
            github_oauth_scope: "read:user".to_owned(),
        };

        let debug = format!("{auth:?}");
        let summary = format!("{:?}", auth.safe_summary());

        for text in [debug, summary] {
            assert!(text.contains("bearer_token_configured: true"));
            assert!(!text.contains("secret-token-that-must-not-log"));
            assert!(!text.contains("access_token=secret"));
            assert!(!text.contains("device_code=secret"));
            assert!(!text.contains("client_secret=secret"));
            assert!(!text.contains("client-id-not-secret-but-not-needed"));
        }
    }

    #[test]
    fn app_config_summary_uses_redacted_urls() {
        let config = AppConfig {
            host: "127.0.0.1".to_owned(),
            port: 60001,
            upstream_responses_url: Some(
                "https://api.example.test/responses?token=secret".to_owned(),
            ),
            upstream_models_url: Some("https://api.example.test/models?secret=value".to_owned()),
            upstream_native_base_url: "https://native.example.test/codex?native_secret=value"
                .to_owned(),
            request_timeout: Duration::from_secs(30),
            request_body_limit_bytes: DEFAULT_REQUEST_BODY_LIMIT_BYTES,
            client_token_file: PathBuf::from("/tmp/client-token"),
            copilot_catalog_file: PathBuf::from("/tmp/model-catalog.json"),
            headers: CopilotHeaderConfig {
                copilot_chat_version: "test-chat".to_owned(),
                copilot_editor_version: "vscode/test".to_owned(),
                github_api_version: "2025-10-01".to_owned(),
            },
            auth: AuthConfig {
                bearer_token: Some("secret-token".to_owned()),
                token_file: PathBuf::from("/tmp/copilot-tokens.json"),
                token_expiry_buffer: Duration::from_secs(300),
                refresh_enabled: true,
                copilot_token_url: DEFAULT_COPILOT_TOKEN_URL.to_owned(),
                github_device_code_url: DEFAULT_GITHUB_DEVICE_CODE_URL.to_owned(),
                github_access_token_url: DEFAULT_GITHUB_ACCESS_TOKEN_URL.to_owned(),
                github_oauth_client_id: DEFAULT_GITHUB_OAUTH_CLIENT_ID.to_owned(),
                github_oauth_scope: DEFAULT_GITHUB_OAUTH_SCOPE.to_owned(),
            },
            rate_limit: RateLimitConfig {
                max_total_wait: DEFAULT_RETRY_BUDGET,
                max_sleep: Duration::from_secs(60),
                initial_backoff: Duration::from_secs(1),
                backoff_multiplier: 2.0,
            },
            raw_log: RawLogConfig {
                level: RawLogLevel::Metadata,
                file: PathBuf::from("/tmp/raw.jsonl"),
                max_bytes: 4096,
                content_max_bytes: 2048,
            },
        };

        let summary = format!("{:?}", config.safe_summary());

        assert!(summary.contains("https://api.example.test/responses"));
        assert!(summary.contains("https://api.example.test/models"));
        assert!(summary.contains("https://native.example.test/codex"));
        assert!(!summary.contains("native_secret=value"));
        assert!(!summary.contains("secret-token"));
        assert!(!summary.contains("token=secret"));
        assert!(!summary.contains("secret=value"));
    }

    #[test]
    fn parse_raw_log_level_accepts_documented_values() {
        assert_eq!(parse_raw_log_level("off"), Some(RawLogLevel::Off));
        assert_eq!(parse_raw_log_level("metadata"), Some(RawLogLevel::Metadata));
        assert_eq!(
            parse_raw_log_level("content_redacted"),
            Some(RawLogLevel::ContentRedacted)
        );
        assert_eq!(
            parse_raw_log_level("full_content"),
            Some(RawLogLevel::FullContent)
        );
        assert_eq!(
            parse_raw_log_level("  FULL_CONTENT  "),
            Some(RawLogLevel::FullContent)
        );
    }

    #[test]
    fn parse_raw_log_level_rejects_invalid_values() {
        assert_eq!(parse_raw_log_level(""), None);
        assert_eq!(parse_raw_log_level("enabled"), None);
        assert_eq!(parse_raw_log_level("metadata-plus"), None);
    }

    #[test]
    fn parse_request_body_limit_bytes_accepts_finite_overrides() {
        assert_eq!(
            parse_request_body_limit_bytes("67108864"),
            Some(64 * 1024 * 1024)
        );
        assert_eq!(parse_request_body_limit_bytes(" 2048 "), Some(2048));
    }

    #[test]
    fn parse_request_body_limit_bytes_rejects_invalid_or_tiny_values() {
        assert_eq!(parse_request_body_limit_bytes(""), None);
        assert_eq!(parse_request_body_limit_bytes("0"), None);
        assert_eq!(parse_request_body_limit_bytes("not-a-number"), None);
        assert_eq!(parse_request_body_limit_bytes("1023"), None);
    }

    #[test]
    fn retry_budget_configuration_cannot_be_unbounded() {
        for value in [None, Some("0"), Some(""), Some("invalid"), Some("-1")] {
            assert_eq!(parse_max_total_wait(value), Duration::from_secs(30));
        }
        assert_eq!(
            parse_max_total_wait(Some(" 2500 ")),
            Duration::from_millis(2500)
        );
    }
}
