//! `btcw-desktop`: the Rust side of the desktop app (Tauri 2).
//!
//! The React UI talks to Rust only through [`ipc`] (`invoke("balance")` → `ipc::balance` →
//! [`commands::balance`] → btcw-core). Every secret stays in Rust; the webview gets plain views.
//!
//! - [`commands`] holds the logic, as plain functions over [`state::AppState`];
//! - [`state`] keeps the settings, the signing session and the per-network wallet gates;
//! - [`settings`] reads and writes `<datadir>/desktop.json`;
//! - [`error`] is `ApiError { code, message }`;
//! - `ipc` has the `#[tauri::command]` wrappers.
//!
//! The window is locked down: strict CSP, no plugins, only event permissions, and navigation
//! limited to the app's own pages.

pub mod assistant;
pub mod commands;
pub mod error;
mod ipc;
pub mod settings;
pub mod state;

use std::sync::Arc;

use tauri::webview::NewWindowResponse;
use tauri::{Url, WebviewWindowBuilder};

use crate::state::AppState;

/// Event carrying `SyncProgress { height, tip_height }` during `sync` (`api.ts` listens for it).
pub const SYNC_PROGRESS_EVENT: &str = "sync-progress";

/// Label of the one window; `capabilities/main-window.json` grants its permissions to it.
const MAIN_WINDOW: &str = "main";

/// Start the app. Logs go to stderr (`RUST_LOG`, default `warn`) and never contain secrets.
pub fn run() {
    // Settings from the project's `.env` (never committed); real environment variables win.
    let _ = dotenvy::dotenv();
    init_logging();
    let state = Arc::new(AppState::from_process_env());
    tracing::info!(datadir = %state.datadir().display(), "starting btcw");

    let result = tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            ipc::app_info,
            ipc::create_wallet,
            ipc::restore_wallet,
            ipc::unlock,
            ipc::lock,
            ipc::keep_alive,
            ipc::new_address,
            ipc::list_addresses,
            ipc::sync,
            ipc::balance,
            ipc::history,
            ipc::utxos,
            ipc::prepare_send,
            ipc::confirm_send,
            ipc::cancel_send,
            ipc::tx_status,
            ipc::min_fee_bump_rate,
            ipc::prepare_fee_bump,
            ipc::list_contacts,
            ipc::add_contact,
            ipc::remove_contact,
            ipc::rename_contact,
            ipc::set_label,
            ipc::clear_label,
            ipc::get_settings,
            ipc::set_settings,
            ipc::backup_challenge,
            ipc::verify_backup,
            ipc::reveal_phrase,
            ipc::get_assistant_settings,
            ipc::set_assistant_settings,
            ipc::assistant_send,
            ipc::assistant_history,
            ipc::assistant_clear,
        ])
        .setup(|app| {
            create_main_window(app)?;
            Ok(())
        })
        .run(tauri::generate_context!());
    if let Err(e) = result {
        tracing::error!(error = %e, "the app stopped with an error");
        eprintln!("btcw: {e}");
        std::process::exit(1);
    }
}

/// The window from `tauri.conf.json`, built here so it refuses to navigate away from the app or
/// open new windows: a remote page must never get the IPC bridge.
fn create_main_window(app: &tauri::App) -> tauri::Result<()> {
    let Some(config) = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == MAIN_WINDOW)
        .cloned()
    else {
        return Err(tauri::Error::WindowNotFound);
    };
    WebviewWindowBuilder::from_config(app.handle(), &config)?
        .on_navigation(|url| {
            let allowed = is_app_url(url);
            if !allowed {
                tracing::warn!(
                    scheme = url.scheme(),
                    host = url.host_str(),
                    "blocked navigation away from the app"
                );
            }
            allowed
        })
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .build()?;
    Ok(())
}

/// The app's own pages: `tauri://localhost` (Linux, macOS), `http(s)://tauri.localhost`
/// (Windows), and in development builds the Vite dev server.
pub fn is_app_url(url: &Url) -> bool {
    match (url.scheme(), url.host_str()) {
        ("tauri", Some("localhost")) => true,
        ("http" | "https", Some("tauri.localhost")) => true,
        ("http", Some("localhost")) => tauri::is_dev() && url.port() == Some(1420),
        _ => false,
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // `try_init`: a second call (tests) is not an error worth failing over.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_app_itself_may_load_in_the_window() {
        let ok = |s: &str| is_app_url(&s.parse().unwrap());
        assert!(ok("tauri://localhost/"));
        assert!(ok("tauri://localhost/index.html"));
        assert!(ok("http://tauri.localhost/"));
        assert!(!ok("https://example.com/"));
        assert!(!ok("http://localhost:8080/"));
        assert!(!ok("tauri://evil.example/"));
        assert!(!ok("file:///etc/passwd"));
        assert!(!ok("data:text/html,hi"));
        // The dev server only in development builds.
        assert_eq!(ok("http://localhost:1420/"), tauri::is_dev());
    }
}
