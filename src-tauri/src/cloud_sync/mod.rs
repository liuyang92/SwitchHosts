//! Cloud sync over free Git hosting accounts (GitHub / Gitee).
//!
//! See `docs/cloud-sync.md` for the full design. Layout:
//!
//! - `account` — credentials file under `internal/`
//! - `payload` — build / hash / snapshot / apply the backup JSON
//! - `provider` — GitHub / Gitee HTTP adapters + GitHub device flow
//! - `engine`  — setup, pull, push, debounce auto-push watcher
//!
//! Renderer-facing error codes are stable strings (e.g. `invalid_token`,
//! `network`, `device_pending`) so the settings page can localize them.

mod account;
mod engine;
mod payload;
mod provider;

#[cfg(test)]
mod tests;

pub(crate) use account::{AuthMethod, Platform};
pub(crate) use engine::{
    logout, on_data_mutated, poll_device_login, pull_once, push_once, setup,
    start_auto_push_watcher, start_device_login, start_remote_poller,
    start_startup_pull, status, verify_token,
};
