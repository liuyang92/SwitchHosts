//! `~/.SwitchHosts/internal/sync_account.json` — cloud sync credentials.
//!
//! Stores the chosen platform, authentication method, login name, repo,
//! token (PAT or device-flow access token) and the last-sync bookkeeping.
//! The file is written with 0600 permissions on Unix so the token is
//! readable only by the owning user. Known trade-off: the token is still
//! plaintext on disk; migrating to the OS keychain is future work.
//!
//! The token is never echoed back to the renderer: `public_view()` strips
//! it before serializing.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::storage::atomic::atomic_write;
use crate::storage::StorageError;

pub const ACCOUNT_FILE_NAME: &str = "sync_account.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Github,
    Gitee,
}

impl Platform {
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "github" => Some(Self::Github),
            "gitee" => Some(Self::Gitee),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitee => "gitee",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Github => "GitHub",
            Self::Gitee => "Gitee",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Pat,
    Device,
}

impl AuthMethod {
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "pat" => Some(Self::Pat),
            "device" => Some(Self::Device),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pat => "pat",
            Self::Device => "device",
        }
    }
}

/// Opaque string for the token: PAT or a device-flow access token. Kept
/// as `String` deliberately — we never need to interpret it, only forward
/// it to the provider's HTTP layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub platform: Platform,
    pub auth_method: AuthMethod,
    pub login: String,
    pub repo: String,
    pub token: String,
    /// OAuth App client id used for the GitHub device flow. Empty for PAT
    /// logins. Public value; safe to expose to the renderer.
    #[serde(default)]
    pub device_client_id: String,
    #[serde(default)]
    pub last_push_at: Option<String>,
    #[serde(default)]
    pub last_pull_at: Option<String>,
    /// Content hash (hex) of the payload at the last successful sync.
    /// Drives "local changed again?" and "remote differs from local?"
    /// decisions without a full diff.
    #[serde(default)]
    pub last_hash: Option<String>,
}

impl Account {
    pub fn public_view(&self) -> Value {
        json!({
            "platform": self.platform.as_str(),
            "auth_method": self.auth_method.as_str(),
            "login": self.login,
            "repo": self.repo,
            "has_device_client_id": !self.device_client_id.is_empty(),
            "last_push_at": self.last_push_at,
            "last_pull_at": self.last_pull_at,
        })
    }
}

pub fn account_file_path(internal_dir: &Path) -> std::path::PathBuf {
    internal_dir.join(ACCOUNT_FILE_NAME)
}

/// Read the account file. Missing file → `None` (not logged in). A present
/// but unreadable/corrupt file is an error we surface — silently treating
/// it as logged-out could surprise a user whose token is fine.
pub fn load(internal_dir: &Path) -> Result<Option<Account>, StorageError> {
    let path = account_file_path(internal_dir);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|e| StorageError::io(path.display().to_string(), e))?;
    let account: Account = serde_json::from_slice(&bytes)
        .map_err(|e| StorageError::parse(path.display().to_string(), e))?;
    Ok(Some(account))
}

pub fn save(internal_dir: &Path, account: &Account) -> Result<(), StorageError> {
    let path = account_file_path(internal_dir);
    let bytes = serde_json::to_vec_pretty(account)
        .map_err(|e| StorageError::serialize(path.display().to_string(), e))?;
    atomic_write(&path, &bytes)?;
    // atomic_write renames a sibling temp file into place, which re-derives
    // permissions from the temp file. Re-apply 0600 after the rename so the
    // final file is locked down even when the temp file defaulted to umask.
    set_permissions(&path)?;
    Ok(())
}

pub fn logout(internal_dir: &Path) -> Result<(), StorageError> {
    let path = account_file_path(internal_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StorageError::io(path.display().to_string(), e)),
    }
}

#[cfg(unix)]
fn set_permissions(path: &Path) -> Result<(), StorageError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| StorageError::io(path.display().to_string(), e))
}

#[cfg(not(unix))]
fn set_permissions(_path: &Path) -> Result<(), StorageError> {
    Ok(())
}

/// Validate a repo name against the rules used by GitHub/Gitee. Used by the
/// setup flow before we hit the network with a bad name.
pub fn is_valid_repo_name(name: &str) -> bool {
    let n = name.trim();
    if n.len() < 1 || n.len() > 100 {
        return false;
    }
    n.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
    })
}
