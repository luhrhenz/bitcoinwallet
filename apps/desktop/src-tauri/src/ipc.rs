//! The `#[tauri::command]`s: one thin wrapper per function in `commands.rs`.
//!
//! Arguments are camelCase on the JS side (Tauri converts them). Each wrapper runs on a blocking
//! thread, since wallet, SQLite, Argon2 and RPC calls all block. Passwords and phrases go
//! straight into `SecretString`. Only command names and error codes are logged.

use std::sync::Arc;

use btcw_core::types::{
    AddressRow, BalanceView, Contact, SyncProgress, SyncReport, TxRow, TxStatus, UtxoRow,
};
use secrecy::SecretString;
use tauri::{AppHandle, Emitter as _, State};

use crate::SYNC_PROGRESS_EVENT;
use crate::assistant::settings::{AssistantSettings, AssistantSettingsUpdate};
use crate::assistant::{self, ChatItem};
use crate::commands::{self, AppInfo, MnemonicReply, PreparedSend, SentTx};
use crate::error::{ApiError, ApiResult};
use crate::settings::Settings;
use crate::state::AppState;

type Shared<'a> = State<'a, Arc<AppState>>;

/// Run `f` on Tauri's blocking pool and log how it went.
async fn blocking<T, F>(name: &'static str, state: &Arc<AppState>, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&AppState) -> ApiResult<T> + Send + 'static,
{
    let state = Arc::clone(state);
    let result = tauri::async_runtime::spawn_blocking(move || f(&state))
        .await
        .unwrap_or_else(|_| {
            // A panic. Generic message: the payload may contain data.
            Err(ApiError::internal(
                "the command stopped unexpectedly; please try again",
            ))
        });
    match &result {
        Ok(_) => tracing::debug!(command = name, "ok"),
        Err(e) => tracing::debug!(command = name, code = e.code, "failed"),
    }
    result
}

#[tauri::command]
pub async fn app_info(state: Shared<'_>) -> ApiResult<AppInfo> {
    blocking("app_info", &state, commands::app_info).await
}

#[tauri::command]
pub async fn create_wallet(
    state: Shared<'_>,
    words: u32,
    password: SecretString,
) -> ApiResult<MnemonicReply> {
    blocking("create_wallet", &state, move |s| {
        commands::create_wallet(s, words, &password)
    })
    .await
}

#[tauri::command]
pub async fn restore_wallet(
    state: Shared<'_>,
    phrase: SecretString,
    password: SecretString,
    birthday: Option<u32>,
) -> ApiResult<()> {
    blocking("restore_wallet", &state, move |s| {
        commands::restore_wallet(s, &phrase, &password, birthday)
    })
    .await
}

#[tauri::command]
pub async fn unlock(state: Shared<'_>, password: SecretString) -> ApiResult<()> {
    blocking("unlock", &state, move |s| commands::unlock(s, &password)).await
}

#[tauri::command]
pub async fn lock(state: Shared<'_>) -> ApiResult<()> {
    blocking("lock", &state, commands::lock).await
}

#[tauri::command]
pub async fn keep_alive(state: Shared<'_>) -> ApiResult<()> {
    blocking("keep_alive", &state, commands::keep_alive).await
}

#[tauri::command]
pub async fn new_address(state: Shared<'_>) -> ApiResult<AddressRow> {
    blocking("new_address", &state, commands::new_address).await
}

#[tauri::command]
pub async fn list_addresses(state: Shared<'_>) -> ApiResult<Vec<AddressRow>> {
    blocking("list_addresses", &state, commands::list_addresses).await
}

#[tauri::command]
pub async fn sync(app: AppHandle, state: Shared<'_>) -> ApiResult<SyncReport> {
    blocking("sync", &state, move |s| {
        commands::sync(s, &mut |progress: SyncProgress| {
            if let Err(e) = app.emit(SYNC_PROGRESS_EVENT, progress) {
                tracing::debug!(error = %e, "could not emit sync progress");
            }
        })
    })
    .await
}

#[tauri::command]
pub async fn balance(state: Shared<'_>) -> ApiResult<BalanceView> {
    blocking("balance", &state, commands::balance).await
}

#[tauri::command]
pub async fn history(state: Shared<'_>) -> ApiResult<Vec<TxRow>> {
    blocking("history", &state, commands::history).await
}

#[tauri::command]
pub async fn utxos(state: Shared<'_>) -> ApiResult<Vec<UtxoRow>> {
    blocking("utxos", &state, commands::utxos).await
}

#[tauri::command]
pub async fn prepare_send(
    state: Shared<'_>,
    to: String,
    amount_sat: u64,
    fee_rate_sat_vb: Option<f64>,
) -> ApiResult<PreparedSend> {
    blocking("prepare_send", &state, move |s| {
        commands::prepare_send(s, &to, amount_sat, fee_rate_sat_vb)
    })
    .await
}

#[tauri::command]
pub async fn confirm_send(state: Shared<'_>, id: String) -> ApiResult<SentTx> {
    blocking("confirm_send", &state, move |s| {
        commands::confirm_send(s, &id)
    })
    .await
}

#[tauri::command]
pub async fn cancel_send(state: Shared<'_>, id: String) -> ApiResult<()> {
    blocking("cancel_send", &state, move |s| {
        commands::cancel_send(s, &id)
    })
    .await
}

#[tauri::command]
pub async fn min_fee_bump_rate(state: Shared<'_>, txid: String) -> ApiResult<f64> {
    blocking("min_fee_bump_rate", &state, move |s| {
        commands::min_fee_bump_rate(s, &txid)
    })
    .await
}

#[tauri::command]
pub async fn prepare_fee_bump(
    state: Shared<'_>,
    txid: String,
    fee_rate_sat_vb: Option<f64>,
) -> ApiResult<PreparedSend> {
    blocking("prepare_fee_bump", &state, move |s| {
        commands::prepare_fee_bump(s, &txid, fee_rate_sat_vb)
    })
    .await
}

#[tauri::command]
pub async fn list_contacts(state: Shared<'_>) -> ApiResult<Vec<Contact>> {
    blocking("list_contacts", &state, commands::list_contacts).await
}

#[tauri::command]
pub async fn add_contact(
    state: Shared<'_>,
    name: String,
    address: String,
    note: Option<String>,
) -> ApiResult<Contact> {
    blocking("add_contact", &state, move |s| {
        commands::add_contact(s, &name, &address, note.as_deref())
    })
    .await
}

#[tauri::command]
pub async fn remove_contact(state: Shared<'_>, name: String) -> ApiResult<Contact> {
    blocking("remove_contact", &state, move |s| {
        commands::remove_contact(s, &name)
    })
    .await
}

#[tauri::command]
pub async fn rename_contact(state: Shared<'_>, old: String, new: String) -> ApiResult<Contact> {
    blocking("rename_contact", &state, move |s| {
        commands::rename_contact(s, &old, &new)
    })
    .await
}

#[tauri::command]
pub async fn set_label(state: Shared<'_>, txid: String, label: String) -> ApiResult<String> {
    blocking("set_label", &state, move |s| {
        commands::set_label(s, &txid, &label)
    })
    .await
}

#[tauri::command]
pub async fn clear_label(state: Shared<'_>, txid: String) -> ApiResult<()> {
    blocking("clear_label", &state, move |s| {
        commands::clear_label(s, &txid)
    })
    .await
}

#[tauri::command]
pub async fn tx_status(state: Shared<'_>, txid: String) -> ApiResult<Option<TxStatus>> {
    blocking("tx_status", &state, move |s| commands::tx_status(s, &txid)).await
}

#[tauri::command]
pub async fn get_settings(state: Shared<'_>) -> ApiResult<Settings> {
    blocking("get_settings", &state, commands::get_settings).await
}

#[tauri::command]
pub async fn set_settings(state: Shared<'_>, settings: Settings) -> ApiResult<()> {
    blocking("set_settings", &state, move |s| {
        commands::set_settings(s, &settings)
    })
    .await
}

#[tauri::command]
pub async fn backup_challenge(state: Shared<'_>, password: SecretString) -> ApiResult<Vec<usize>> {
    blocking("backup_challenge", &state, move |s| {
        commands::backup_challenge(s, &password)
    })
    .await
}

#[tauri::command]
pub async fn verify_backup(
    state: Shared<'_>,
    password: SecretString,
    answers: Vec<(usize, String)>,
) -> ApiResult<()> {
    blocking("verify_backup", &state, move |s| {
        commands::verify_backup(s, &password, answers)
    })
    .await
}

#[tauri::command]
pub async fn reveal_phrase(state: Shared<'_>, password: SecretString) -> ApiResult<MnemonicReply> {
    blocking("reveal_phrase", &state, move |s| {
        commands::reveal_phrase(s, &password)
    })
    .await
}

#[tauri::command]
pub async fn get_assistant_settings(state: Shared<'_>) -> ApiResult<AssistantSettings> {
    blocking("get_assistant_settings", &state, assistant::get_settings).await
}

#[tauri::command]
pub async fn set_assistant_settings(
    state: Shared<'_>,
    settings: AssistantSettingsUpdate,
) -> ApiResult<AssistantSettings> {
    blocking("set_assistant_settings", &state, move |s| {
        assistant::set_settings(s, &settings)
    })
    .await
}

#[tauri::command]
pub async fn assistant_send(state: Shared<'_>, text: String) -> ApiResult<ChatItem> {
    blocking("assistant_send", &state, move |s| assistant::send(s, &text)).await
}

#[tauri::command]
pub async fn assistant_history(state: Shared<'_>) -> ApiResult<Vec<ChatItem>> {
    blocking("assistant_history", &state, assistant::history).await
}

#[tauri::command]
pub async fn assistant_clear(state: Shared<'_>) -> ApiResult<()> {
    blocking("assistant_clear", &state, |s| {
        assistant::clear(s);
        Ok(())
    })
    .await
}
