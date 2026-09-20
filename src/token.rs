use crate::config::{AuthConfig, CopilotHeaderConfig};
use http::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use http::{HeaderName, HeaderValue};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::info;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("missing Copilot bearer token; set COPILOT_BEARER_TOKEN or provide a valid COPILOT_TOKEN_FILE")]
    Missing,
    #[error("Copilot token file is expired or near expiry: {path}")]
    Expired { path: PathBuf },
    #[error("Copilot token file is expired or near expiry and cannot be refreshed because it is missing githubToken: {path}")]
    ExpiredMissingGithubToken { path: PathBuf },
    #[error("Copilot token file cannot be refreshed because token refresh is disabled or githubToken is missing: {path}")]
    TokenFileNotRefreshable { path: PathBuf },
    #[error("failed to read Copilot token file {path}: {source}")]
    ReadTokenFile { path: PathBuf, source: io::Error },
    #[error("failed to parse Copilot token file {path}: {source}")]
    ParseTokenFile {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("Copilot token file is missing copilotToken: {path}")]
    MissingCopilotToken { path: PathBuf },
    #[error("failed to refresh Copilot token: HTTP {status}")]
    RefreshStatus { status: reqwest::StatusCode },
    #[error("failed to refresh Copilot token request")]
    RefreshRequest { source: reqwest::Error },
    #[error("failed to write refreshed Copilot token file {path}: {source}")]
    WriteTokenFile { path: PathBuf, source: io::Error },
    #[error("Copilot refresh response is missing token or expires_at")]
    MissingRefreshFields,
    #[error("Copilot account endpoint metadata is invalid")]
    InvalidEndpointMetadata,
}

#[derive(Debug, Error)]
pub enum DeviceLoginError {
    #[error("failed to start GitHub device login: HTTP {status}")]
    DeviceCodeStatus { status: reqwest::StatusCode },
    #[error("failed to start GitHub device login: {source}")]
    DeviceCodeRequest { source: reqwest::Error },
    #[error("GitHub device-code response is missing device_code, user_code, or verification_uri")]
    MissingDeviceCodeFields,
    #[error("failed to poll GitHub device login: HTTP {status}")]
    AccessTokenStatus { status: reqwest::StatusCode },
    #[error("failed to poll GitHub device login: {source}")]
    AccessTokenRequest { source: reqwest::Error },
    #[error("GitHub device login expired before authorization completed")]
    Expired,
    #[error("GitHub device login was denied")]
    AccessDenied,
    #[error("GitHub device login failed: {0}")]
    OAuth(String),
    #[error("GitHub device login completed but no access_token was returned")]
    MissingAccessToken,
    #[error(transparent)]
    Auth(#[from] AuthError),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CopilotTokenFile {
    #[serde(rename = "githubToken", skip_serializing_if = "Option::is_none")]
    github_token: Option<String>,
    #[serde(rename = "copilotToken")]
    copilot_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<Value>,
    #[serde(rename = "lastUpdated", skip_serializing_if = "Option::is_none")]
    last_updated: Option<Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct CopilotRefreshResponse {
    token: Option<String>,
    expires_at: Option<u64>,
    endpoints: Option<CopilotEndpoints>,
}

#[derive(Debug, Deserialize)]
struct CopilotEndpoints {
    api: Option<String>,
}

struct RefreshedCopilotToken {
    token: String,
    expires_at: u64,
    endpoint: Option<Url>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthSource {
    EnvToken,
    TokenFile {
        path: PathBuf,
        expires_at: Option<u64>,
        refreshed: bool,
        refreshable: bool,
    },
}

#[derive(Clone)]
pub struct ResolvedAuthorization {
    header_value: String,
    source: AuthSource,
    endpoint: Option<String>,
}

impl ResolvedAuthorization {
    pub fn header_value(&self) -> &str {
        &self.header_value
    }

    pub fn source(&self) -> &AuthSource {
        &self.source
    }

    pub fn endpoint(&self) -> Result<Option<Url>, AuthError> {
        validate_copilot_api_endpoint(self.endpoint.as_deref())
    }
}

impl std::fmt::Debug for ResolvedAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedAuthorization")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

struct ConfiguredToken {
    token: String,
    source: AuthSource,
    endpoint: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeviceCodeRequest<'a> {
    client_id: &'a str,
    scope: &'a str,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
}

#[derive(Debug, Serialize)]
struct AccessTokenRequest<'a> {
    client_id: &'a str,
    device_code: &'a str,
    grant_type: &'a str,
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
}

const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub async fn resolve_upstream_authorization(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
) -> Result<ResolvedAuthorization, AuthError> {
    let configured = configured_token(auth, headers, client)
        .await?
        .ok_or(AuthError::Missing)?;
    Ok(ResolvedAuthorization {
        header_value: as_authorization_header(&configured.token),
        source: configured.source,
        endpoint: configured.endpoint,
    })
}

pub async fn printable_token_from_auth_config(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
) -> Result<String, AuthError> {
    configured_token(auth, headers, client)
        .await?
        .map(|configured| configured.token)
        .ok_or(AuthError::Missing)
}

pub async fn refresh_token_file_authorization(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
) -> Result<ResolvedAuthorization, AuthError> {
    let configured = refresh_token_file(auth, headers, client).await?;
    Ok(ResolvedAuthorization {
        header_value: as_authorization_header(&configured.token),
        source: configured.source,
        endpoint: configured.endpoint,
    })
}

pub fn should_try_device_login_after_auth_error(error: &AuthError) -> bool {
    matches!(
        error,
        AuthError::Missing
            | AuthError::Expired { .. }
            | AuthError::ExpiredMissingGithubToken { .. }
            | AuthError::TokenFileNotRefreshable { .. }
            | AuthError::MissingCopilotToken { .. }
            | AuthError::RefreshStatus { .. }
            | AuthError::RefreshRequest { .. }
            | AuthError::MissingRefreshFields
    )
}

pub async fn login_with_device_flow(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
    out: &mut dyn Write,
) -> Result<String, DeviceLoginError> {
    let device = request_device_code(auth, headers, client).await?;
    let device_code = device
        .device_code
        .ok_or(DeviceLoginError::MissingDeviceCodeFields)?;
    let user_code = device
        .user_code
        .ok_or(DeviceLoginError::MissingDeviceCodeFields)?;
    let verification_uri = device
        .verification_uri
        .ok_or(DeviceLoginError::MissingDeviceCodeFields)?;
    let mut interval = device.interval.unwrap_or(5);
    let expires_in = device.expires_in.unwrap_or(900);

    writeln!(out, "GitHub Copilot authentication is required.").ok();
    writeln!(out, "Visit: {verification_uri}").ok();
    writeln!(out, "Enter code: {user_code}").ok();
    writeln!(out, "Waiting for authorization...").ok();

    let deadline = now_epoch_seconds().saturating_add(expires_in);
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = poll_for_github_token(auth, headers, client, &device_code).await?;

        if let Some(access_token) = response
            .access_token
            .map(|value| strip_bearer_prefix(&value))
            .filter(|value| !value.is_empty())
        {
            let refreshed = refresh_copilot_token(auth, headers, client, &access_token).await?;
            let copilot_token = refreshed.token.clone();
            write_device_login_token_file(&auth.token_file, access_token, refreshed)?;
            writeln!(out, "GitHub Copilot authentication saved.").ok();
            return Ok(copilot_token);
        }

        match response.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = interval.saturating_add(5),
            Some("expired_token") => return Err(DeviceLoginError::Expired),
            Some("access_denied") => return Err(DeviceLoginError::AccessDenied),
            Some(error) => return Err(DeviceLoginError::OAuth(error.to_owned())),
            None => return Err(DeviceLoginError::MissingAccessToken),
        }

        if now_epoch_seconds() >= deadline {
            return Err(DeviceLoginError::Expired);
        }
    }
}

async fn configured_token(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
) -> Result<Option<ConfiguredToken>, AuthError> {
    if let Some(token) = auth
        .bearer_token
        .as_ref()
        .map(|value| strip_bearer_prefix(value))
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(ConfiguredToken {
            token,
            source: AuthSource::EnvToken,
            endpoint: None,
        }));
    }

    load_token_file(auth, headers, client).await
}

async fn request_device_code(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
) -> Result<DeviceCodeResponse, DeviceLoginError> {
    let response = client
        .post(&auth.github_device_code_url)
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .header(
            USER_AGENT,
            header_value(
                format!("GitHubCopilotChat/{}", headers.copilot_chat_version),
                "user-agent",
            )?,
        )
        .json(&DeviceCodeRequest {
            client_id: &auth.github_oauth_client_id,
            scope: &auth.github_oauth_scope,
        })
        .send()
        .await
        .map_err(|source| DeviceLoginError::DeviceCodeRequest { source })?;

    let status = response.status();
    if !status.is_success() {
        return Err(DeviceLoginError::DeviceCodeStatus { status });
    }

    response
        .json::<DeviceCodeResponse>()
        .await
        .map_err(|source| DeviceLoginError::DeviceCodeRequest { source })
}

async fn poll_for_github_token(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
    device_code: &str,
) -> Result<AccessTokenResponse, DeviceLoginError> {
    let response = client
        .post(&auth.github_access_token_url)
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .header(
            USER_AGENT,
            header_value(
                format!("GitHubCopilotChat/{}", headers.copilot_chat_version),
                "user-agent",
            )?,
        )
        .json(&AccessTokenRequest {
            client_id: &auth.github_oauth_client_id,
            device_code,
            grant_type: DEVICE_CODE_GRANT_TYPE,
        })
        .send()
        .await
        .map_err(|source| DeviceLoginError::AccessTokenRequest { source })?;

    let status = response.status();
    if !status.is_success() {
        return Err(DeviceLoginError::AccessTokenStatus { status });
    }

    response
        .json::<AccessTokenResponse>()
        .await
        .map_err(|source| DeviceLoginError::AccessTokenRequest { source })
}

fn as_authorization_header(token: &str) -> String {
    format!("Bearer {}", strip_bearer_prefix(token))
}

fn strip_bearer_prefix(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed
        .get(..7)
        .map(|prefix| prefix.eq_ignore_ascii_case("bearer "))
        .unwrap_or(false)
    {
        trimmed[7..].trim().to_owned()
    } else {
        trimmed.to_owned()
    }
}

async fn load_token_file(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
) -> Result<Option<ConfiguredToken>, AuthError> {
    let path = &auth.token_file;
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AuthError::ReadTokenFile {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    let data: CopilotTokenFile =
        serde_json::from_str(&text).map_err(|source| AuthError::ParseTokenFile {
            path: path.to_path_buf(),
            source,
        })?;

    let refreshable = auth.refresh_enabled && has_refreshable_github_token(&data);
    let expires_at = data.expires_at.as_ref().and_then(epoch_seconds);
    if !is_expired_or_near_expiry(data.expires_at.as_ref(), auth.token_expiry_buffer) {
        let configured = copilot_token_from_data(path, data, expires_at, false, refreshable)?;
        info!(
            auth_source = "token_file",
            path = %path.display(),
            expires_at,
            refreshed = false,
            refreshable,
            "using Copilot token from token file"
        );
        return Ok(Some(configured));
    }

    if !auth.refresh_enabled {
        return Err(AuthError::Expired {
            path: path.to_path_buf(),
        });
    }

    let github_token = data
        .github_token
        .as_deref()
        .map(strip_bearer_prefix)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::ExpiredMissingGithubToken {
            path: path.to_path_buf(),
        })?;

    let refreshed = refresh_copilot_token(auth, headers, client, &github_token).await?;
    let token = refreshed.token.clone();
    let endpoint = refreshed.endpoint.as_ref().map(Url::to_string);
    let expires_at = Some(refreshed.expires_at);
    write_refreshed_token_file(path, data, refreshed)?;

    info!(
        auth_source = "token_file",
        path = %path.display(),
        expires_at,
        refreshed = true,
        "using refreshed Copilot token from token file"
    );

    Ok(Some(ConfiguredToken {
        token,
        source: AuthSource::TokenFile {
            path: path.to_path_buf(),
            expires_at,
            refreshed: true,
            refreshable: true,
        },
        endpoint,
    }))
}

async fn refresh_token_file(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
) -> Result<ConfiguredToken, AuthError> {
    let path = &auth.token_file;
    if !auth.refresh_enabled {
        return Err(AuthError::TokenFileNotRefreshable {
            path: path.to_path_buf(),
        });
    }

    let text = fs::read_to_string(path).map_err(|source| AuthError::ReadTokenFile {
        path: path.to_path_buf(),
        source,
    })?;
    let data: CopilotTokenFile =
        serde_json::from_str(&text).map_err(|source| AuthError::ParseTokenFile {
            path: path.to_path_buf(),
            source,
        })?;
    let github_token = github_token_from_data(path, &data)?;

    let refreshed = refresh_copilot_token(auth, headers, client, &github_token).await?;
    let token = refreshed.token.clone();
    let endpoint = refreshed.endpoint.as_ref().map(Url::to_string);
    let expires_at = Some(refreshed.expires_at);
    write_refreshed_token_file(path, data, refreshed)?;

    info!(
        auth_source = "token_file",
        path = %path.display(),
        expires_at,
        refreshed = true,
        refreshable = true,
        "force-refreshed Copilot token from token file"
    );

    Ok(ConfiguredToken {
        token,
        source: AuthSource::TokenFile {
            path: path.to_path_buf(),
            expires_at,
            refreshed: true,
            refreshable: true,
        },
        endpoint,
    })
}

fn copilot_token_from_data(
    path: &Path,
    data: CopilotTokenFile,
    expires_at: Option<u64>,
    refreshed: bool,
    refreshable: bool,
) -> Result<ConfiguredToken, AuthError> {
    let token = data
        .copilot_token
        .map(|value| strip_bearer_prefix(&value))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::MissingCopilotToken {
            path: path.to_path_buf(),
        })?;

    Ok(ConfiguredToken {
        token,
        source: AuthSource::TokenFile {
            path: path.to_path_buf(),
            expires_at,
            refreshed,
            refreshable,
        },
        endpoint: data.endpoint,
    })
}

fn has_refreshable_github_token(data: &CopilotTokenFile) -> bool {
    data.github_token
        .as_deref()
        .map(strip_bearer_prefix)
        .is_some_and(|value| !value.is_empty())
}

fn github_token_from_data(path: &Path, data: &CopilotTokenFile) -> Result<String, AuthError> {
    data.github_token
        .as_deref()
        .map(strip_bearer_prefix)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AuthError::TokenFileNotRefreshable {
            path: path.to_path_buf(),
        })
}

fn is_expired_or_near_expiry(expires_at: Option<&Value>, expiry_buffer: Duration) -> bool {
    let Some(expires_at) = expires_at.and_then(epoch_seconds) else {
        return false;
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let buffer = expiry_buffer.as_secs();

    now >= expires_at.saturating_sub(buffer)
}

async fn refresh_copilot_token(
    auth: &AuthConfig,
    headers: &CopilotHeaderConfig,
    client: &reqwest::Client,
    github_token: &str,
) -> Result<RefreshedCopilotToken, AuthError> {
    let mut authorization = HeaderValue::from_str(&format!("Bearer {github_token}"))
        .map_err(|_| AuthError::MissingRefreshFields)?;
    authorization.set_sensitive(true);

    let response = client
        .get(&auth.copilot_token_url)
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .header(AUTHORIZATION, authorization)
        .header(
            USER_AGENT,
            header_value(
                format!("GitHubCopilotChat/{}", headers.copilot_chat_version),
                "user-agent",
            )?,
        )
        .header(
            HeaderName::from_static("editor-version"),
            header_value(headers.copilot_editor_version.clone(), "editor-version")?,
        )
        .header(
            HeaderName::from_static("editor-plugin-version"),
            header_value(
                format!("copilot-chat/{}", headers.copilot_chat_version),
                "editor-plugin-version",
            )?,
        )
        .send()
        .await
        .map_err(|source| AuthError::RefreshRequest { source })?;

    let status = response.status();
    if !status.is_success() {
        return Err(AuthError::RefreshStatus { status });
    }

    let data = response
        .json::<CopilotRefreshResponse>()
        .await
        .map_err(|source| AuthError::RefreshRequest { source })?;

    let token = data
        .token
        .map(|value| strip_bearer_prefix(&value))
        .filter(|value| !value.is_empty())
        .ok_or(AuthError::MissingRefreshFields)?;
    let expires_at = data.expires_at.ok_or(AuthError::MissingRefreshFields)?;
    let endpoint = data
        .endpoints
        .and_then(|endpoints| endpoints.api)
        .map(|api| validate_copilot_api_endpoint(Some(&api)))
        .transpose()?
        .flatten();

    Ok(RefreshedCopilotToken {
        token,
        expires_at,
        endpoint,
    })
}

fn write_refreshed_token_file(
    path: &Path,
    mut data: CopilotTokenFile,
    refreshed: RefreshedCopilotToken,
) -> Result<(), AuthError> {
    data.copilot_token = Some(refreshed.token);
    data.expires_at = Some(Value::from(refreshed.expires_at));
    data.endpoint = refreshed.endpoint.as_ref().map(persisted_endpoint_value);
    data.last_updated = Some(Value::from(now_epoch_seconds()));

    write_token_file(path, &data)
}

fn write_device_login_token_file(
    path: &Path,
    github_token: String,
    refreshed: RefreshedCopilotToken,
) -> Result<(), AuthError> {
    let data = CopilotTokenFile {
        github_token: Some(github_token),
        copilot_token: Some(refreshed.token),
        endpoint: refreshed.endpoint.as_ref().map(persisted_endpoint_value),
        expires_at: Some(Value::from(refreshed.expires_at)),
        last_updated: Some(Value::from(now_epoch_seconds())),
        extra: Map::new(),
    };

    write_token_file(path, &data)
}

fn validate_copilot_api_endpoint(raw: Option<&str>) -> Result<Option<Url>, AuthError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut url = Url::parse(raw).map_err(|_| AuthError::InvalidEndpointMetadata)?;
    let host = url.host_str().ok_or(AuthError::InvalidEndpointMetadata)?;
    let trusted_host = host == "githubcopilot.com"
        || host
            .strip_suffix(".githubcopilot.com")
            .is_some_and(|prefix| !prefix.is_empty());
    let supported_path = matches!(url.path(), "" | "/" | "/chat/completions");
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || !trusted_host
        || url.port_or_known_default() != Some(443)
        || url.query().is_some()
        || url.fragment().is_some()
        || !supported_path
    {
        return Err(AuthError::InvalidEndpointMetadata);
    }
    url.set_path("/");
    Ok(Some(url))
}

fn persisted_endpoint_value(endpoint: &Url) -> String {
    let mut endpoint = endpoint.clone();
    endpoint.set_path("/chat/completions");
    endpoint.to_string().trim_end_matches('/').to_owned()
}

fn write_token_file(path: &Path, data: &CopilotTokenFile) -> Result<(), AuthError> {
    persist_token_file(path, data).map_err(|source| AuthError::WriteTokenFile {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn persist_token_file(path: &Path, data: &CopilotTokenFile) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // SAFETY: geteuid has no preconditions and does not retain any pointers.
    let uid = unsafe { libc::geteuid() };
    let path = prepare_token_path(path, uid)?;
    validate_token_destination(&path, uid)?;
    let parent = path.parent().expect("validated token path has a parent");

    // NamedTempFile uses create_new and mode 0600 before any secret is written.
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer_pretty(&mut file, data).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.as_file().sync_all()?;
    validate_token_destination(&path, uid)?;
    file.persist(&path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(not(unix))]
fn persist_token_file(_path: &Path, _data: &CopilotTokenFile) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure token persistence requires Unix file permissions",
    ))
}

#[cfg(unix)]
fn prepare_token_path(path: &Path, uid: u32) -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::Component;

    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid token file path",
        ));
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = path
        .parent()
        .filter(|_| path.file_name().is_some())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "token path must name a file")
        })?;
    let mut directory = PathBuf::new();
    for component in parent.components() {
        directory.push(component);
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::DirBuilder::new().mode(0o700).create(&directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                fs::symlink_metadata(&directory)?
            }
            Err(error) => return Err(error),
        };
        let is_parent = directory == parent;
        // A root/current-user sticky ancestor permits private temp directories,
        // but the token's immediate parent must never be shared-writable.
        let unsafe_writes =
            metadata.mode() & 0o022 != 0 && (is_parent || metadata.mode() & 0o1000 == 0);
        if !metadata.is_dir()
            || (metadata.uid() != uid && (is_parent || metadata.uid() != 0))
            || unsafe_writes
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "token parent must be a trusted directory without symlinks or unsafe write permissions",
            ));
        }
    }
    Ok(path)
}

#[cfg(unix)]
fn validate_token_destination(path: &Path, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.uid() == uid => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "token destination must be an owned regular file, not a symlink",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn header_value(value: String, _label: &'static str) -> Result<HeaderValue, AuthError> {
    HeaderValue::from_str(&value).map_err(|_| AuthError::MissingRefreshFields)
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn epoch_seconds(value: &Value) -> Option<u64> {
    let raw = match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse::<u64>().ok(),
        _ => None,
    }?;

    if raw > 10_000_000_000 {
        Some(raw / 1_000)
    } else {
        Some(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        DEFAULT_COPILOT_TOKEN_URL, DEFAULT_GITHUB_ACCESS_TOKEN_URL, DEFAULT_GITHUB_DEVICE_CODE_URL,
        DEFAULT_GITHUB_OAUTH_CLIENT_ID, DEFAULT_GITHUB_OAUTH_SCOPE,
    };
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use http::{HeaderMap, StatusCode};
    use serde_json::json;
    use std::net::SocketAddr;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::NamedTempFile;
    use tokio::net::TcpListener;

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

    fn auth_config(token_file: PathBuf) -> AuthConfig {
        AuthConfig {
            bearer_token: None,
            token_file,
            token_expiry_buffer: Duration::from_secs(300),
            refresh_enabled: true,
            copilot_token_url: DEFAULT_COPILOT_TOKEN_URL.to_owned(),
            github_device_code_url: DEFAULT_GITHUB_DEVICE_CODE_URL.to_owned(),
            github_access_token_url: DEFAULT_GITHUB_ACCESS_TOKEN_URL.to_owned(),
            github_oauth_client_id: DEFAULT_GITHUB_OAUTH_CLIENT_ID.to_owned(),
            github_oauth_scope: DEFAULT_GITHUB_OAUTH_SCOPE.to_owned(),
        }
    }

    fn header_config() -> CopilotHeaderConfig {
        CopilotHeaderConfig {
            copilot_chat_version: "test-chat".to_owned(),
            copilot_editor_version: "vscode/test".to_owned(),
            github_api_version: "2025-10-01".to_owned(),
        }
    }

    async fn printable_token(auth: &AuthConfig) -> Result<String, AuthError> {
        let headers = header_config();
        let client = reqwest::Client::new();
        printable_token_from_auth_config(auth, &headers, &client).await
    }

    async fn resolved_authorization(auth: &AuthConfig) -> Result<ResolvedAuthorization, AuthError> {
        let headers = header_config();
        let client = reqwest::Client::new();
        resolve_upstream_authorization(auth, &headers, &client).await
    }

    #[cfg(unix)]
    fn private_token_dir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;

        tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap()
    }

    #[cfg(unix)]
    fn token_data() -> CopilotTokenFile {
        serde_json::from_value(json!({
            "githubToken": "saved-github-token",
            "copilotToken": "previous-copilot-token",
            "expiresAt": 123,
            "endpoint": "https://api.example.test",
            "extraField": {"preserved": true}
        }))
        .unwrap()
    }

    #[cfg(unix)]
    fn refreshed_token() -> RefreshedCopilotToken {
        RefreshedCopilotToken {
            token: "replacement-copilot-token".to_owned(),
            expires_at: 456,
            endpoint: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn device_login_creates_token_file_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = private_token_dir();
        let path = dir.path().join("tokens.json");
        write_device_login_token_file(&path, "github-token".to_owned(), refreshed_token()).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["githubToken"], "github-token");
        assert_eq!(saved["copilotToken"], "replacement-copilot-token");
    }

    #[cfg(unix)]
    #[test]
    fn token_save_creates_private_parent_directories() {
        use std::os::unix::fs::PermissionsExt;

        let dir = private_token_dir();
        let parent = dir.path().join("new").join("nested");
        write_token_file(&parent.join("tokens.json"), &token_data()).unwrap();

        for path in [dir.path().join("new"), parent] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn token_save_preserves_existing_parent_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = private_token_dir();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        write_token_file(&dir.path().join("tokens.json"), &token_data()).unwrap();

        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn device_login_replaces_existing_token_atomically() {
        use std::io::Read;

        let dir = private_token_dir();
        let path = dir.path().join("tokens.json");
        fs::write(&path, "previous token payload").unwrap();
        let mut old_file = fs::File::open(&path).unwrap();

        write_device_login_token_file(&path, "github-token".to_owned(), refreshed_token()).unwrap();

        let mut old_payload = String::new();
        old_file.read_to_string(&mut old_payload).unwrap();
        assert_eq!(old_payload, "previous token payload");
        let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["copilotToken"], "replacement-copilot-token");
    }

    #[cfg(unix)]
    #[test]
    fn refresh_replaces_token_atomically_and_preserves_extra_fields() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;

        let dir = private_token_dir();
        let path = dir.path().join("tokens.json");
        let previous = serde_json::to_vec(&token_data()).unwrap();
        fs::write(&path, &previous).unwrap();
        let mut old_file = fs::File::open(&path).unwrap();

        write_refreshed_token_file(&path, token_data(), refreshed_token()).unwrap();

        let mut old_payload = Vec::new();
        old_file.read_to_end(&mut old_payload).unwrap();
        assert_eq!(old_payload, previous);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["copilotToken"], "replacement-copilot-token");
        assert_eq!(saved["githubToken"], "saved-github-token");
        assert!(saved.get("endpoint").is_none());
        assert_eq!(saved["extraField"], json!({"preserved": true}));
        assert_eq!(saved["expiresAt"], 456);
    }

    #[cfg(unix)]
    #[test]
    fn token_save_rejects_symlink_destination_without_changing_target() {
        use std::os::unix::fs::symlink;

        let dir = private_token_dir();
        let target = dir.path().join("target.json");
        let path = dir.path().join("tokens.json");
        fs::write(&target, "previous token payload").unwrap();
        symlink(&target, &path).unwrap();

        let error = write_token_file(&path, &token_data()).unwrap_err();

        assert!(matches!(error, AuthError::WriteTokenFile { .. }));
        assert!(!error.to_string().contains("saved-github-token"));
        assert_eq!(
            fs::read_to_string(target).unwrap(),
            "previous token payload"
        );
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn token_save_rejects_nonregular_destination() {
        let dir = private_token_dir();
        let path = dir.path().join("tokens.json");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), "unchanged").unwrap();

        assert!(matches!(
            write_token_file(&path, &token_data()),
            Err(AuthError::WriteTokenFile { .. })
        ));
        assert_eq!(fs::read_to_string(path.join("keep")).unwrap(), "unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn token_save_rejects_symlink_parent_without_changing_target() {
        use std::os::unix::fs::symlink;

        let dir = private_token_dir();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("tokens.json"), "previous token payload").unwrap();
        let parent = dir.path().join("linked-parent");
        symlink(&target, &parent).unwrap();

        assert!(write_token_file(&parent.join("tokens.json"), &token_data()).is_err());
        assert_eq!(
            fs::read_to_string(target.join("tokens.json")).unwrap(),
            "previous token payload"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_refresh_save_preserves_previous_token_in_unsafe_parent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = private_token_dir();
        let path = dir.path().join("tokens.json");
        let previous = serde_json::to_vec(&token_data()).unwrap();
        fs::write(&path, &previous).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o770)).unwrap();

        let result = write_refreshed_token_file(&path, token_data(), refreshed_token());

        assert!(matches!(result, Err(AuthError::WriteTokenFile { .. })));
        assert_eq!(fs::read(path).unwrap(), previous);
        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o770
        );
    }

    #[cfg(unix)]
    #[test]
    fn token_save_rejects_writable_ancestor_above_private_parent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = private_token_dir();
        let parent = dir.path().join("private");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();

        assert!(write_token_file(&parent.join("tokens.json"), &token_data()).is_err());
        assert!(!parent.join("tokens.json").exists());
    }

    #[tokio::test]
    async fn reads_copilot_token_file_for_printing() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), r#"{"copilotToken":"secret-token"}"#).unwrap();

        let token = printable_token(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(token, "secret-token");
    }

    #[tokio::test]
    async fn token_file_expiry_errors_do_not_include_secret() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"copilotToken":"secret-token","expiresAt":1}"#,
        )
        .unwrap();

        let error = printable_token(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("expired"));
        assert!(!message.contains("secret-token"));
    }

    #[tokio::test]
    async fn env_token_wins_and_is_normalized() {
        let auth = AuthConfig {
            bearer_token: Some("Bearer secret-token".to_owned()),

            token_file: PathBuf::from("/definitely/not/present"),
            token_expiry_buffer: Duration::from_secs(300),
            refresh_enabled: true,
            copilot_token_url: DEFAULT_COPILOT_TOKEN_URL.to_owned(),
            github_device_code_url: DEFAULT_GITHUB_DEVICE_CODE_URL.to_owned(),
            github_access_token_url: DEFAULT_GITHUB_ACCESS_TOKEN_URL.to_owned(),
            github_oauth_client_id: DEFAULT_GITHUB_OAUTH_CLIENT_ID.to_owned(),
            github_oauth_scope: DEFAULT_GITHUB_OAUTH_SCOPE.to_owned(),
        };

        let auth_header = resolved_authorization(&auth).await.unwrap();

        assert_eq!(auth_header.header_value(), "Bearer secret-token");
        assert_eq!(auth_header.source(), &AuthSource::EnvToken);
        assert!(!format!("{auth_header:?}").contains("secret-token"));
        assert_eq!(printable_token(&auth).await.unwrap(), "secret-token");
    }
    #[tokio::test]
    async fn token_file_preserves_valid_account_endpoint_metadata() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"copilotToken":"file-token","endpoint":"https://api.business.githubcopilot.com/chat/completions"}"#,
        )
        .unwrap();

        let authorization = resolved_authorization(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(
            authorization.endpoint().unwrap().unwrap().as_str(),
            "https://api.business.githubcopilot.com/"
        );
    }

    #[test]
    fn account_endpoint_validation_rejects_untrusted_destinations() {
        for endpoint in [
            "http://api.githubcopilot.com",
            "https://githubcopilot.com.attacker.example",
            "https://user@api.githubcopilot.com",
            "https://api.githubcopilot.com:8443",
            "https://api.githubcopilot.com/other/path",
            "https://api.githubcopilot.com?redirect=attacker",
            "https://api.githubcopilot.com#attacker",
        ] {
            let error = validate_copilot_api_endpoint(Some(endpoint)).unwrap_err();
            assert!(matches!(error, AuthError::InvalidEndpointMetadata));
            assert!(!error.to_string().contains(endpoint));
        }
    }

    #[test]
    fn account_endpoint_validation_accepts_default_ports_and_legacy_paths() {
        for endpoint in [
            "https://githubcopilot.com",
            "https://api.githubcopilot.com:443/",
            "https://api.enterprise.githubcopilot.com/chat/completions",
        ] {
            let validated = validate_copilot_api_endpoint(Some(endpoint))
                .unwrap()
                .unwrap();
            assert_eq!(validated.scheme(), "https");
            assert_eq!(validated.path(), "/");
        }
    }

    #[tokio::test]
    async fn upstream_authorization_requires_service_credentials() {
        let auth = auth_config(PathBuf::from("/definitely/not/present"));

        assert!(matches!(
            resolved_authorization(&auth).await,
            Err(AuthError::Missing)
        ));
    }

    #[tokio::test]
    async fn token_file_resolves_upstream_authorization() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), r#"{"copilotToken":"file-token"}"#).unwrap();

        let auth_header = resolved_authorization(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(auth_header.header_value(), "Bearer file-token");
        assert!(matches!(
            auth_header.source(),
            AuthSource::TokenFile {
                refreshed: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn accepts_future_expires_at() {
        let file = NamedTempFile::new().unwrap();
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600;
        fs::write(
            file.path(),
            format!(r#"{{"copilotToken":"secret-token","expiresAt":{future}}}"#),
        )
        .unwrap();

        let token = printable_token(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(token, "secret-token");
    }

    #[tokio::test]
    async fn accepts_future_expires_at_in_epoch_milliseconds() {
        let file = NamedTempFile::new().unwrap();
        let future_ms = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600)
            * 1_000;
        fs::write(
            file.path(),
            format!(r#"{{"copilotToken":"secret-token","expiresAt":{future_ms}}}"#),
        )
        .unwrap();

        let token = printable_token(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(
            token, "secret-token",
            "Existing Copilot token files may store expiresAt as epoch milliseconds."
        );
    }

    #[tokio::test]
    async fn malformed_token_file_errors_do_not_echo_file_contents() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"copilotToken":"secret-token-that-must-not-leak","expiresAt":"not closed""#,
        )
        .unwrap();

        let error = printable_token(&auth_config(file.path().to_path_buf()))
            .await
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("failed to parse"));
        assert!(
            !message.contains("secret-token-that-must-not-leak"),
            "Parse diagnostics should identify the bad token file without echoing secret-bearing contents."
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn expired_token_file_refreshes_from_saved_github_token() {
        let dir = private_token_dir();
        let file = NamedTempFile::new_in(dir.path()).unwrap();
        fs::write(
            file.path(),
            r#"{"githubToken":"github-secret","copilotToken":"stale-copilot-secret","expiresAt":1}"#,
        )
        .unwrap();
        let recorded = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
        let future = now_epoch_seconds() + 3_600;
        let server = spawn_router({
            let recorded = recorded.clone();
            Router::new().route(
                "/copilot_internal/v2/token",
                get(move |headers: HeaderMap| {
                    let recorded = recorded.clone();
                    async move {
                        recorded.lock().unwrap().push(headers);
                        Json(json!({
                            "token": "fresh-copilot-token",
                            "expires_at": future,
                            "endpoints": {"api": "https://api.enterprise.githubcopilot.com"}
                        }))
                    }
                }),
            )
        })
        .await;
        let mut auth = auth_config(file.path().to_path_buf());
        auth.copilot_token_url = server.url("/copilot_internal/v2/token");

        let token = printable_token(&auth).await.unwrap();

        assert_eq!(token, "fresh-copilot-token");
        let saved: Value = serde_json::from_str(&fs::read_to_string(file.path()).unwrap()).unwrap();
        assert_eq!(saved["copilotToken"], "fresh-copilot-token");
        assert_eq!(saved["githubToken"], "github-secret");
        assert_eq!(saved["expiresAt"], future);
        assert_eq!(
            saved["endpoint"],
            "https://api.enterprise.githubcopilot.com/chat/completions"
        );
        assert!(
            !fs::read_to_string(file.path())
                .unwrap()
                .contains("stale-copilot-secret"),
            "Refreshing should replace the expired Copilot session token rather than keeping stale token material."
        );

        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].get(AUTHORIZATION).unwrap(),
            "Bearer github-secret"
        );
        assert_eq!(
            requests[0].get(USER_AGENT).unwrap(),
            "GitHubCopilotChat/test-chat"
        );
        assert_eq!(requests[0].get("editor-version").unwrap(), "vscode/test");
        assert_eq!(
            requests[0].get("editor-plugin-version").unwrap(),
            "copilot-chat/test-chat"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn force_refresh_token_file_replaces_locally_valid_copilot_token() {
        let dir = private_token_dir();
        let file = NamedTempFile::new_in(dir.path()).unwrap();
        let original_future = now_epoch_seconds() + 600;
        fs::write(
            file.path(),
            format!(
                r#"{{"githubToken":"github-secret","copilotToken":"locally-valid-but-rejected","expiresAt":{original_future},"extraField":"preserved"}}"#
            ),
        )
        .unwrap();
        let recorded = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
        let refreshed_future = now_epoch_seconds() + 3_600;
        let server = spawn_router({
            let recorded = recorded.clone();
            Router::new().route(
                "/copilot_internal/v2/token",
                get(move |headers: HeaderMap| {
                    let recorded = recorded.clone();
                    async move {
                        recorded.lock().unwrap().push(headers);
                        Json(json!({
                            "token": "fresh-copilot-token",
                            "expires_at": refreshed_future,
                            "endpoints": {"api": "https://api.enterprise.githubcopilot.com"}
                        }))
                    }
                }),
            )
        })
        .await;
        let mut auth = auth_config(file.path().to_path_buf());
        auth.copilot_token_url = server.url("/copilot_internal/v2/token");
        let headers = header_config();
        let client = reqwest::Client::new();

        let authorization = refresh_token_file_authorization(&auth, &headers, &client)
            .await
            .unwrap();

        assert_eq!(authorization.header_value(), "Bearer fresh-copilot-token");
        assert_eq!(
            authorization.endpoint().unwrap().unwrap().as_str(),
            "https://api.enterprise.githubcopilot.com/"
        );
        assert_eq!(
            authorization.source(),
            &AuthSource::TokenFile {
                path: file.path().to_path_buf(),
                expires_at: Some(refreshed_future),
                refreshed: true,
                refreshable: true,
            }
        );
        assert!(!format!("{authorization:?}").contains("fresh-copilot-token"));
        let saved_text = fs::read_to_string(file.path()).unwrap();
        let saved: Value = serde_json::from_str(&saved_text).unwrap();
        assert_eq!(saved["copilotToken"], "fresh-copilot-token");
        assert_eq!(saved["githubToken"], "github-secret");
        assert_eq!(saved["expiresAt"], refreshed_future);
        assert_eq!(saved["extraField"], "preserved");
        assert!(saved["lastUpdated"].as_u64().unwrap() > 0);
        assert_eq!(
            saved["endpoint"],
            "https://api.enterprise.githubcopilot.com/chat/completions"
        );
        assert!(!saved_text.contains("locally-valid-but-rejected"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(file.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].get(AUTHORIZATION).unwrap(),
            "Bearer github-secret"
        );
    }

    #[tokio::test]
    async fn force_refresh_token_file_missing_github_token_fails_without_secret_leakage() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"copilotToken":"copilot-secret-that-must-not-leak","expiresAt":9999999999}"#,
        )
        .unwrap();
        let auth = auth_config(file.path().to_path_buf());
        let headers = header_config();
        let client = reqwest::Client::new();

        let error = refresh_token_file_authorization(&auth, &headers, &client)
            .await
            .unwrap_err();
        let message = error.to_string();

        assert!(matches!(error, AuthError::TokenFileNotRefreshable { .. }));
        assert!(message.contains("cannot be refreshed"));
        assert!(!message.contains("copilot-secret-that-must-not-leak"));
    }

    #[tokio::test]
    async fn refresh_http_errors_do_not_echo_saved_tokens() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"githubToken":"github-secret-that-must-not-leak","copilotToken":"stale-secret-that-must-not-leak","expiresAt":1}"#,
        )
        .unwrap();
        let server = spawn_router(Router::new().route(
            "/copilot_internal/v2/token",
            get(|| async { (StatusCode::FORBIDDEN, "forbidden") }),
        ))
        .await;
        let mut auth = auth_config(file.path().to_path_buf());
        auth.copilot_token_url = server.url("/copilot_internal/v2/token");

        let error = printable_token(&auth).await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("HTTP 403"));
        assert!(!message.contains("github-secret-that-must-not-leak"));
        assert!(!message.contains("stale-secret-that-must-not-leak"));
    }

    #[tokio::test]
    async fn refresh_request_errors_do_not_echo_url_queries_or_saved_tokens() {
        let file = NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            r#"{"githubToken":"github-secret-that-must-not-leak","copilotToken":"copilot-secret-that-must-not-leak","expiresAt":9999999999}"#,
        )
        .unwrap();
        let mut auth = auth_config(file.path().to_path_buf());
        auth.copilot_token_url =
            "http://127.0.0.1:1/copilot_internal/v2/token?token=refresh-url-secret".to_owned();
        let headers = header_config();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        let error = refresh_token_file_authorization(&auth, &headers, &client)
            .await
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("failed to refresh Copilot token request"));
        assert!(!message.contains("refresh-url-secret"));
        assert!(!message.contains("github-secret-that-must-not-leak"));
        assert!(!message.contains("copilot-secret-that-must-not-leak"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn device_login_saves_github_and_copilot_tokens_without_printing_secrets() {
        let dir = private_token_dir();
        let file = NamedTempFile::new_in(dir.path()).unwrap();
        let _ = fs::remove_file(file.path());
        let token_polls = Arc::new(AtomicUsize::new(0));
        let refresh_headers = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
        let future = now_epoch_seconds() + 3_600;
        let server = spawn_router({
            let token_polls = token_polls.clone();
            let refresh_headers = refresh_headers.clone();
            Router::new()
                .route(
                    "/login/device/code",
                    post(|| async {
                        Json(json!({
                            "device_code": "device-secret-that-must-not-print",
                            "user_code": "ABCD-EFGH",
                            "verification_uri": "https://github.com/login/device",
                            "expires_in": 30,
                            "interval": 0
                        }))
                    }),
                )
                .route(
                    "/login/oauth/access_token",
                    post(move || {
                        let token_polls = token_polls.clone();
                        async move {
                            if token_polls.fetch_add(1, Ordering::SeqCst) == 0 {
                                Json(json!({"error": "authorization_pending"}))
                            } else {
                                Json(json!({"access_token": "github-login-token"}))
                            }
                        }
                    }),
                )
                .route(
                    "/copilot_internal/v2/token",
                    get(move |headers: HeaderMap| {
                        let refresh_headers = refresh_headers.clone();
                        async move {
                            refresh_headers.lock().unwrap().push(headers);
                            Json(json!({
                                "token": "fresh-copilot-token",
                                "expires_at": future,
                                "endpoints": {"api": "https://api.enterprise.githubcopilot.com"}
                            }))
                        }
                    }),
                )
        })
        .await;
        let mut auth = auth_config(file.path().to_path_buf());
        auth.github_device_code_url = server.url("/login/device/code");
        auth.github_access_token_url = server.url("/login/oauth/access_token");
        auth.copilot_token_url = server.url("/copilot_internal/v2/token");
        let headers = header_config();
        let client = reqwest::Client::new();
        let mut output = Vec::new();

        let token = login_with_device_flow(&auth, &headers, &client, &mut output)
            .await
            .unwrap();

        assert_eq!(token, "fresh-copilot-token");
        assert_eq!(token_polls.load(Ordering::SeqCst), 2);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("https://github.com/login/device"));
        assert!(output.contains("ABCD-EFGH"));
        assert!(!output.contains("device-secret-that-must-not-print"));
        assert!(!output.contains("github-login-token"));
        assert!(!output.contains("fresh-copilot-token"));

        let saved: Value = serde_json::from_str(&fs::read_to_string(file.path()).unwrap()).unwrap();
        assert_eq!(saved["githubToken"], "github-login-token");
        assert_eq!(saved["copilotToken"], "fresh-copilot-token");
        assert_eq!(saved["expiresAt"], future);
        assert_eq!(
            saved["endpoint"],
            "https://api.enterprise.githubcopilot.com/chat/completions"
        );

        let refresh_headers = refresh_headers.lock().unwrap();
        assert_eq!(refresh_headers.len(), 1);
        assert_eq!(
            refresh_headers[0].get(AUTHORIZATION).unwrap(),
            "Bearer github-login-token"
        );
    }

    #[tokio::test]
    async fn device_login_access_denied_fails_without_writing_token_file() {
        let file = NamedTempFile::new().unwrap();
        let _ = fs::remove_file(file.path());
        let server = spawn_router(
            Router::new()
                .route(
                    "/login/device/code",
                    post(|| async {
                        Json(json!({
                            "device_code": "device-secret-that-must-not-print",
                            "user_code": "ABCD-EFGH",
                            "verification_uri": "https://github.com/login/device",
                            "expires_in": 30,
                            "interval": 0
                        }))
                    }),
                )
                .route(
                    "/login/oauth/access_token",
                    post(|| async { Json(json!({"error": "access_denied"})) }),
                ),
        )
        .await;
        let mut auth = auth_config(file.path().to_path_buf());
        auth.github_device_code_url = server.url("/login/device/code");
        auth.github_access_token_url = server.url("/login/oauth/access_token");
        let headers = header_config();
        let client = reqwest::Client::new();
        let mut output = Vec::new();

        let error = login_with_device_flow(&auth, &headers, &client, &mut output)
            .await
            .unwrap_err();

        assert!(matches!(error, DeviceLoginError::AccessDenied));
        assert!(
            !file.path().exists(),
            "Denied device login should not create a token file."
        );
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("device-secret-that-must-not-print"));
    }
}
