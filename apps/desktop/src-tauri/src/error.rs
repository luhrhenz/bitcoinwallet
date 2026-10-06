//! The one error shape that crosses into JS: `ApiError { code, message }` (`types.ts`).
//!
//! `code` is [`WalletError::code`] for core errors, plus `"locked"` (the command needs the
//! signer) and `"internal"` (a bug or unexpected failure in the bridge).
//!
//! `message` is the core's `Display` text or a fixed sentence; nothing that could be secret is
//! ever formatted into it.

use btcw_core::WalletError;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub const LOCKED: &'static str = "locked";
    pub const INTERNAL: &'static str = "internal";

    /// The command needs the signer (sending), and there is none.
    pub fn locked() -> Self {
        Self {
            code: Self::LOCKED,
            message: "the wallet is locked; unlock it with your password to send".into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: Self::INTERNAL,
            message: message.into(),
        }
    }
}

impl From<WalletError> for ApiError {
    fn from(e: WalletError) -> Self {
        Self {
            code: e.code(),
            message: e.to_string(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for ApiError {}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_the_ts_shape() {
        let e = ApiError::from(WalletError::WrongPassword);
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "code": "wrong_password", "message": "wrong password or corrupted keystore" })
        );
        assert_eq!(ApiError::locked().code, "locked");
    }
}
