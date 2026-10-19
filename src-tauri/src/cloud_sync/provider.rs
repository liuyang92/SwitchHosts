//! Platform adapters: GitHub and Gitee, plus GitHub device-flow helpers.
//!
//! The trait surface is deliberately tiny — verify / has_repo / create_repo /
//! get_file / put_file — so the engine above stays platform-agnostic.
//! Token injection (header vs query param), response field names and the
//! create-repo private flag are the only real differences.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::payload::CLOUD_FILE_NAME;

const GH_API: &str = "https://api.github.com";
const GH_LOGIN: &str = "https://github.com";
const GITEE_API: &str = "https://gitee.com/api/v5";

/// Scope requested by the GitHub device flow. `repo` grants private-repo
/// access, which is required because we create private backup repos.
const GH_DEVICE_SCOPE: &str = "repo";

/// Errors surfaced to the renderer as stable codes (frontend can i18n
/// them); `internal` carries unexpected failures with a log line.
#[derive(Debug)]
pub enum SyncError {
    Network(String),
    InvalidToken,
    /// 403: the token authenticated but the account lacks the required
    /// permission (e.g. a fine-grained PAT without Account read for /user,
    /// or a classic PAT without the repo scope on repo endpoints).
    Forbidden(String),
    /// GitHub refused to create the repo. Fine-grained PATs cannot call
    /// POST /user/repos at all ("Resource not accessible by integration");
    /// classic/device tokens also see this on org policy issues. Needs a
    /// distinct message because no amount of permission tweaking fixes a
    /// fine-grained token here.
    CreateRepoNotAllowed,
    RateLimited,
    InvalidRepoName,
    /// HTTP error from the platform: (status, trimmed body).
    Api(u16, String),
    /// GitHub device-flow poll outcome: "pending" / "slow_down" /
    /// "expired_token" / "access_denied".
    Device(&'static str),
    Internal(String),
}

impl SyncError {
    /// Renderer-facing code. Kept short and stable; the UI maps known
    /// codes to localized strings and falls back to showing the code.
    pub fn code(&self) -> String {
        match self {
            Self::Network(_) => "network".into(),
            Self::InvalidToken => "invalid_token".into(),
            Self::Forbidden(_) => "forbidden".into(),
            Self::CreateRepoNotAllowed => "create_repo_not_allowed".into(),
            Self::RateLimited => "rate_limited".into(),
            Self::InvalidRepoName => "invalid_repo_name".into(),
            Self::Api(status, _) => format!("api_{status}"),
            Self::Device(code) => format!("device_{code}"),
            Self::Internal(_) => "internal".into(),
        }
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(raw) => write!(f, "network error: {raw}"),
            Self::InvalidToken => write!(f, "invalid or expired token"),
            Self::Forbidden(reason) => write!(f, "forbidden by the platform: {reason}"),
            Self::CreateRepoNotAllowed => write!(f, "the platform refused to create the repo"),
            Self::RateLimited => write!(f, "platform rate limit hit, retry later"),
            Self::InvalidRepoName => write!(f, "invalid repository name"),
            Self::Api(status, body) => write!(f, "api {status}: {body}"),
            Self::Device(code) => write!(f, "device flow: {code}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

pub type SyncResult<T> = Result<T, SyncError>;

#[derive(Debug, Clone)]
pub struct RemoteFile {
    pub content: Vec<u8>,
    pub sha: String,
}

#[derive(Debug, Clone)]
pub enum Provider {
    Github,
    Gitee,
}

impl Provider {
    pub fn new(platform: super::account::Platform) -> Self {
        match platform {
            super::account::Platform::Github => Self::Github,
            super::account::Platform::Gitee => Self::Gitee,
        }
    }
}

fn trim_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() > 400 {
        format!("{}…", &trimmed[..trimmed.floor_char_boundary(400)])
    } else {
        trimmed.to_string()
    }
}

async fn send_json(request: reqwest::RequestBuilder) -> SyncResult<(u16, String)> {
    let response = request.send().await.map_err(|e| SyncError::Network(e.to_string()))?;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    Ok((status, body))
}

fn map_status<T>(status: u16, body: &str, ok: u16, ok_value: T) -> SyncResult<T> {
    if status == ok {
        return Ok(ok_value);
    }
    Err(match status {
        401 => SyncError::InvalidToken,
        403 => SyncError::Forbidden(trim_body(body)),
        429 => SyncError::RateLimited,
        _ => SyncError::Api(status, trim_body(body)),
    })
}

// GitHub scopes the token by Authorization header; responses are JSON.
fn gh_request(client: &reqwest::Client, method: reqwest::Method, url: &str, token: &str) -> reqwest::RequestBuilder {
    client
        .request(method, url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

impl Provider {
    /// Verify the token by asking the platform for the current user, and
    /// return the login name. 401/403 → `InvalidToken`.
    pub async fn verify(&self, client: &reqwest::Client, token: &str) -> SyncResult<String> {
        match self {
            Self::Github => {
                let (status, body) =
                    send_json(gh_request(client, reqwest::Method::GET, &format!("{GH_API}/user"), token)).await?;
                if status != 200 {
                    return Err(match status {
                        401 => SyncError::InvalidToken,
                        403 => SyncError::Forbidden(trim_body(&body)),
                        429 => SyncError::RateLimited,
                        _ => SyncError::Api(status, trim_body(&body)),
                    });
                }
                parse_user_login(&body)
            }
            Self::Gitee => {
                let (status, body) = send_json(
                    client
                        .get(format!("{GITEE_API}/user"))
                        .query(&[("access_token", token)]),
                )
                .await?;
                if status != 200 {
                    return Err(match status {
                        401 => SyncError::InvalidToken,
                        403 => SyncError::Forbidden(trim_body(&body)),
                        429 => SyncError::RateLimited,
                        _ => SyncError::Api(status, trim_body(&body)),
                    });
                }
                parse_user_login(&body)
            }
        }
    }

    /// Does the repo exist? 200 → yes, 404 → no. Anything else is an error.
    pub async fn has_repo(
        &self,
        client: &reqwest::Client,
        token: &str,
        owner: &str,
        repo: &str,
    ) -> SyncResult<bool> {
        if owner.is_empty() || repo.is_empty() {
            return Err(SyncError::InvalidRepoName);
        }
        match self {
            Self::Github => {
                let (status, _body) = send_json(
                    gh_request(
                        client,
                        reqwest::Method::GET,
                        &format!("{GH_API}/repos/{owner}/{repo}"),
                        token,
                    ),
                )
                .await?;
                match status {
                    200 => Ok(true),
                    404 => Ok(false),
                    401 => Err(SyncError::InvalidToken),
                    403 => Err(SyncError::Forbidden(trim_body(&_body))),
                    429 => Err(SyncError::RateLimited),
                    _ => Err(SyncError::Api(status, trim_body(&_body))),
                }
            }
            Self::Gitee => {
                let (status, _body) = send_json(
                    client
                        .get(format!("{GITEE_API}/repos/{owner}/{repo}"))
                        .query(&[("access_token", token)]),
                )
                .await?;
                match status {
                    200 => Ok(true),
                    404 => Ok(false),
                    401 => Err(SyncError::InvalidToken),
                    403 => Err(SyncError::Forbidden(trim_body(&_body))),
                    429 => Err(SyncError::RateLimited),
                    _ => Err(SyncError::Api(status, trim_body(&_body))),
                }
            }
        }
    }

    /// Create a private repository owned by the authorized user.
    pub async fn create_repo(
        &self,
        client: &reqwest::Client,
        token: &str,
        repo: &str,
    ) -> SyncResult<()> {
        let body = match self {
            Self::Github => json!({ "name": repo, "private": true, "description": "SwitchHosts cloud backup", "auto_init": false }),
            Self::Gitee => json!({ "access_token": token, "name": repo, "private": true, "auto_init": false }),
        };
        let request = match self {
            Self::Github => gh_request(
                client,
                reqwest::Method::POST,
                &format!("{GH_API}/user/repos"),
                token,
            )
            .json(&body),
            Self::Gitee => client.post(format!("{GITEE_API}/user/repos")).json(&body),
        };
        let (status, body) = send_json(request).await?;
        // GitHub 403 with this message means the token type can't create
        // repos at all — most often a fine-grained PAT. Break it out from
        // the generic forbidden path so the UI can name the real cause.
        if status == 403
            && matches!(self, Self::Github)
            && body.contains("Resource not accessible by integration")
        {
            return Err(SyncError::CreateRepoNotAllowed);
        }
        // 201 Created is the documented success for both platforms; some
        // Gitee variants answer 200 — accept both, anything else is an error.
        map_status(status, &body, 201, ()).or_else(|err| {
            if status == 200 {
                Ok(())
            } else {
                Err(err)
            }
        })
    }

    /// Read the sync file. 404 → `None`. The payload can be large; decoded
    /// into bytes and re-validated by the caller.
    pub async fn get_file(
        &self,
        client: &reqwest::Client,
        token: &str,
        owner: &str,
        repo: &str,
    ) -> SyncResult<Option<RemoteFile>> {
        let path_in_repo = CLOUD_FILE_NAME;
        let (status, body) = match self {
            Self::Github => {
                send_json(
                    gh_request(
                        client,
                        reqwest::Method::GET,
                        &format!("{GH_API}/repos/{owner}/{repo}/contents/{path_in_repo}"),
                        token,
                    ),
                )
                .await?
            }
            Self::Gitee => {
                let (status, body) = send_json(
                    client
                        .get(format!("{GITEE_API}/repos/{owner}/{repo}/contents/{path_in_repo}"))
                        .query(&[("access_token", token)]),
                )
                .await?;
                // Gitee may return a JSON array for directory listing; the
                // file path is fixed, so treat arrays as missing file.
                if status == 200 && body.trim_start().starts_with('[') {
                    return Ok(None);
                }
                (status, body)
            }
        };
        match status {
            200 => {}
            404 => return Ok(None),
            401 => return Err(SyncError::InvalidToken),
            403 => return Err(SyncError::Forbidden(trim_body(&body))),
            429 => return Err(SyncError::RateLimited),
            _ => return Err(SyncError::Api(status, trim_body(&body))),
        }
        let file = parse_content_file(&body)?;
        Ok(Some(file))
    }

    /// Write (create or overwrite) the sync file. Pass the existing `sha`
    /// when overwriting so the platform's update semantics succeed; omit
    /// it to create.
    pub async fn put_file(
        &self,
        client: &reqwest::Client,
        token: &str,
        owner: &str,
        repo: &str,
        content: &[u8],
        existing_sha: Option<String>,
    ) -> SyncResult<()> {
        let b64: String = base64::engine::general_purpose::STANDARD.encode(content);
        let path_in_repo = CLOUD_FILE_NAME;
        let body = json!({
            "message": "chore: update SwitchHosts cloud backup",
            "content": b64,
            "sha": existing_sha,
        });
        let request = match self {
            Self::Github => gh_request(
                client,
                reqwest::Method::PUT,
                &format!("{GH_API}/repos/{owner}/{repo}/contents/{path_in_repo}"),
                token,
            )
            .json(&body),
            Self::Gitee => {
                let mut body = body;
                body["access_token"] = json!(token);
                client
                    .put(format!("{GITEE_API}/repos/{owner}/{repo}/contents/{path_in_repo}"))
                    .json(&body)
            }
        };
        let (status, body) = send_json(request).await?;
        map_status(status, &body, 200, ()).or_else(|err| {
            // 201 for file creation on GitHub.
            if status == 201 {
                Ok(())
            } else {
                Err(err)
            }
        })
    }
}

// ---- GitHub device flow ----------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// Step 1 of the GitHub device flow: request a device + user code.
/// Requires an OAuth App client id whose owner enabled "Enable Device
/// Flow" in the app's settings. No client secret involved.
pub async fn start_device_code(
    client: &reqwest::Client,
    client_id: &str,
) -> SyncResult<DeviceCodeResponse> {
    if client_id.is_empty() {
        return Err(SyncError::Device("client_id_missing"));
    }
    let (status, body) = send_json(
        client
            .post(format!("{GH_LOGIN}/login/device/code"))
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id),
                ("scope", GH_DEVICE_SCOPE),
            ]),
    )
    .await?;
    if status != 200 {
        return Err(map_device_code_error(status, &body));
    }
    #[derive(Deserialize)]
    struct Raw {
        device_code: String,
        user_code: String,
        verification_uri: String,
        #[serde(default = "default_interval")]
        expires_in: u64,
        #[serde(default = "default_interval")]
        interval: u64,
    }
    fn default_interval() -> u64 {
        5
    }
    let raw: Raw = serde_json::from_str(&body)
        .map_err(|e| SyncError::Internal(format!("bad device/code response: {e}")))?;
    Ok(DeviceCodeResponse {
        device_code: raw.device_code,
        user_code: raw.user_code,
        verification_uri: raw.verification_uri,
        expires_in: raw.expires_in,
        interval: raw.interval,
    })
}

/// Outcome of polling the device token endpoint.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DevicePollOutcome {
    /// Authorization still pending — the frontend should wait `interval`
    /// seconds from the start response and poll again.
    Pending { interval: u64 },
    /// Token granted; equivalent to a PAT from here on.
    Token { token: String },
    /// User has 10+ seconds of slack before the code expires.
    Expired,
    /// User denied authorization at the browser prompt.
    Denied,
}

/// Step 2: poll the token endpoint once. Does not sleep internally — the
/// renderer schedules retries per `DevicePollOutcome::Pending::interval`
/// so the UI keeps responding while waiting.
pub async fn poll_device_token(
    client: &reqwest::Client,
    client_id: &str,
    device_code: &str,
) -> SyncResult<DevicePollOutcome> {
    let (status, body) = send_json(
        client
            .post(format!("{GH_LOGIN}/login/oauth/access_token"))
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id),
                ("device_code", device_code),
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:device_code",
                ),
            ]),
    )
    .await?;
    if status != 200 {
        return Err(SyncError::Api(status, trim_body(&body)));
    }
    parse_device_poll(&body)
}

/// Map an unknown device-flow error string onto the fixed
/// `SyncError::Device` variants the UI knows about.
fn leak_unknown_device_error(raw: &str) -> &'static str {
    match raw {
        "incorrect_device_code" => "expired",
        "unsupported_grant_type" => "client_id_missing",
        _ => "denied",
    }
}

// ---- pure response parsers (unit-testable offline) -------------------------

pub(super) fn parse_user_login(body: &str) -> SyncResult<String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| SyncError::Internal(format!("bad /user response: {e}")))?;
    value["login"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| SyncError::Internal("missing login in /user response".into()))
}

pub(super) fn parse_content_file(body: &str) -> SyncResult<RemoteFile> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| SyncError::Internal(format!("bad contents response: {e}")))?;
    let sha = value["sha"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| SyncError::Internal("missing sha in contents response".into()))?;
    let content_b64 = value["content"]
        .as_str()
        .ok_or_else(|| SyncError::Internal("missing content in contents response".into()))?;
    // GitHub/Gitee wrap base64 with newlines every 60 chars; strip all
    // whitespace before decoding.
    let cleaned: String = content_b64.chars().filter(|c| !c.is_whitespace()).collect();
    let content = base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| SyncError::Internal(format!("content base64 decode failed: {e}")))?;
    Ok(RemoteFile { content, sha })
}

pub(super) fn map_device_code_error(status: u16, body: &str) -> SyncError {
    // The most common failure here: the OAuth App exists but Device Flow is
    // not enabled in its settings. GitHub answers 400 with an error code;
    // surface that as a dedicated renderer-facing code so the UI can tell the
    // user exactly what to fix. Other errors keep the generic api_<status>
    // shape with a trimmed body in the log.
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        match value["error"].as_str() {
            Some("device_flow_disabled") => return SyncError::Device("flow_disabled"),
            Some(other) => log::info!("device code request error: {other}"),
            None => {}
        }
    }
    SyncError::Api(status, trim_body(body))
}

pub(super) fn parse_device_poll(body: &str) -> SyncResult<DevicePollOutcome> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| SyncError::Internal(format!("bad access_token response: {e}")))?;
    if let Some(token) = value["access_token"].as_str() {
        return Ok(DevicePollOutcome::Token {
            token: token.to_string(),
        });
    }
    match value["error"].as_str() {
        Some("authorization_pending") => Ok(DevicePollOutcome::Pending { interval: 5 }),
        Some("slow_down") => Ok(DevicePollOutcome::Pending { interval: 15 }),
        Some("expired_token") => Ok(DevicePollOutcome::Expired),
        Some("access_denied") => Ok(DevicePollOutcome::Denied),
        Some(other) => Err(SyncError::Device(leak_unknown_device_error(other))),
        None => Err(SyncError::Internal("access_token response without token or error".into())),
    }
}
