//! One `chat/completions` call to an OpenAI-compatible provider, and the price lookup.
//! Errors are fixed sentences plus a status code; the key is redacted from anything echoed.

use std::time::Duration;

use serde_json::{Value, json};

use crate::error::ApiError;

pub const RATE_LIMIT: &str = "assistant_rate_limit";
pub const KEY: &str = "assistant_key";
pub const UNREACHABLE: &str = "assistant_unreachable";
pub const REPLY: &str = "assistant_reply";

/// Longest provider error text passed on.
const MAX_ECHO: usize = 200;

pub struct Provider<'a> {
    pub label: &'static str,
    pub base_url: &'a str,
    pub model: &'a str,
    pub api_key: &'a str,
    pub timeout: Duration,
}

impl Provider<'_> {
    /// The assistant message of the first choice (`content` and/or `tool_calls`).
    pub fn complete(&self, messages: &[Value], tools: &[Value]) -> Result<Value, ApiError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        check_transport(&url)?;
        let body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
            "temperature": 0.2,
        });
        let response = minreq::post(&url)
            .with_header("Authorization", format!("Bearer {}", self.api_key))
            .with_header("Content-Type", "application/json")
            .with_header("Accept", "application/json")
            .with_body(body.to_string())
            .with_timeout(self.timeout.as_secs().max(1))
            .with_follow_redirects(false)
            .send()
            .map_err(|e| self.unreachable(&e))?;
        let status = response.status_code;
        let text = response.as_str().unwrap_or_default();
        tracing::debug!(status, "assistant: provider answered");
        match status {
            200..=299 => {}
            401 | 403 => {
                return Err(error(
                    KEY,
                    format!(
                        "the {} API key was refused; check it in Settings → Assistant",
                        self.label
                    ),
                ));
            }
            429 => {
                return Err(error(
                    RATE_LIMIT,
                    format!(
                        "{} is rate limiting this key (free tier); wait a minute and try again",
                        self.label
                    ),
                ));
            }
            500..=599 | 300..=399 => {
                return Err(error(
                    UNREACHABLE,
                    format!("{} is not answering right now (HTTP {status})", self.label),
                ));
            }
            _ => {
                let reason = provider_message(text)
                    .map(|m| format!(": {}", self.redact(&m)))
                    .unwrap_or_default();
                return Err(error(
                    REPLY,
                    format!("{} refused the request (HTTP {status}){reason}", self.label),
                ));
            }
        }
        let reply: Value = serde_json::from_str(text).map_err(|_| self.malformed())?;
        let message = reply
            .pointer("/choices/0/message")
            .filter(|m| m.is_object())
            .cloned()
            .ok_or_else(|| self.malformed())?;
        Ok(message)
    }

    fn unreachable(&self, e: &minreq::Error) -> ApiError {
        let timed_out = matches!(e, minreq::Error::IoError(io)
            if matches!(io.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock));
        tracing::debug!(timed_out, "assistant: provider request failed");
        if timed_out {
            error(
                UNREACHABLE,
                format!(
                    "{} did not answer within {} seconds; try again",
                    self.label,
                    self.timeout.as_secs().max(1)
                ),
            )
        } else {
            error(
                UNREACHABLE,
                format!(
                    "can't reach {}; check your internet connection and the provider URL",
                    self.label
                ),
            )
        }
    }

    fn malformed(&self) -> ApiError {
        error(
            REPLY,
            format!(
                "{} sent a reply btcw doesn't understand; check the model supports tool calling",
                self.label
            ),
        )
    }

    fn redact(&self, text: &str) -> String {
        redact(text, self.api_key)
    }
}

/// `text` with every occurrence of `secret` (and long pieces of it) removed, capped.
pub fn redact(text: &str, secret: &str) -> String {
    let mut out: String = text.chars().take(MAX_ECHO).collect();
    if !secret.is_empty() {
        out = out.replace(secret, "[redacted]");
        // Providers sometimes echo a prefix or suffix of the key.
        if secret.len() >= 8 {
            let pieces = [secret.get(..8), secret.get(secret.len() - 8..)];
            for piece in pieces.into_iter().flatten() {
                out = out.replace(piece, "[redacted]");
            }
        }
    }
    out
}

/// `{"error": {"message": "..."}}` or `[{"error": ...}]` (Gemini).
fn provider_message(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    let error = value.get("error").or_else(|| value.pointer("/0/error"))?;
    error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(|m| m.split('\n').next().unwrap_or_default().trim().to_owned())
}

/// The current BTC price in `currency` from CoinGecko.
pub fn btc_price(base_url: &str, currency: &str, timeout: Duration) -> Result<f64, ApiError> {
    let url = format!("{base_url}/simple/price?ids=bitcoin&vs_currencies={currency}");
    check_transport(&url)?;
    let unreachable = || {
        error(
            UNREACHABLE,
            "can't reach CoinGecko for the price".to_owned(),
        )
    };
    let response = minreq::get(&url)
        .with_header("Accept", "application/json")
        .with_timeout(timeout.as_secs().max(1))
        .with_follow_redirects(false)
        .send()
        .map_err(|_| unreachable())?;
    if response.status_code == 429 {
        return Err(error(
            RATE_LIMIT,
            "CoinGecko is rate limiting price requests".into(),
        ));
    }
    if !(200..=299).contains(&response.status_code) {
        return Err(unreachable());
    }
    let value: Value =
        serde_json::from_str(response.as_str().unwrap_or_default()).map_err(|_| unreachable())?;
    value
        .pointer(&format!("/bitcoin/{currency}"))
        .and_then(Value::as_f64)
        .ok_or_else(|| error(REPLY, format!("CoinGecko has no {currency} price")))
}

/// `https://` only. Tests also talk plain HTTP to a fake server on the loopback address.
fn check_transport(url: &str) -> Result<(), ApiError> {
    if url.starts_with("https://") || (cfg!(test) && url.starts_with("http://127.0.0.1:")) {
        Ok(())
    } else {
        Err(error(
            REPLY,
            "the provider URL must start with https://".to_owned(),
        ))
    }
}

fn error(code: &'static str, message: String) -> ApiError {
    ApiError { code, message }
}
