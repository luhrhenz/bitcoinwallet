//! The assistant's settings: stored in `desktop.json` with the rest, shown to the UI without the key.

use btcw_core::WalletError;
use serde::{Deserialize, Serialize};

const MAX_URL_LEN: usize = 2048;
const MAX_MODEL_LEN: usize = 200;
const MAX_KEY_LEN: usize = 512;

/// A provider preset: its name, base URL and a tool-capable model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    pub model: &'static str,
}

pub const PRESETS: [Preset; 3] = [
    Preset {
        id: "groq",
        label: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        model: "llama-3.3-70b-versatile",
    },
    Preset {
        id: "gemini",
        label: "Gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        model: "gemini-2.5-flash",
    },
    Preset {
        id: "custom",
        label: "your provider",
        base_url: "",
        model: "",
    },
];

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// What `desktop.json` keeps under `"assistant"`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAssistant {
    #[serde(default)]
    pub enabled: bool,
    /// The user agreed, once, that wallet data goes to the provider.
    #[serde(default)]
    pub consented: bool,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub live_price: bool,
}

impl Default for StoredAssistant {
    fn default() -> Self {
        let groq = &PRESETS[0];
        Self {
            enabled: false,
            consented: false,
            provider: groq.id.into(),
            base_url: groq.base_url.into(),
            model: groq.model.into(),
            api_key: None,
            live_price: false,
        }
    }
}

/// Never shows the key.
impl std::fmt::Debug for StoredAssistant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredAssistant")
            .field("enabled", &self.enabled)
            .field("consented", &self.consented)
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .field("live_price", &self.live_price)
            .finish()
    }
}

impl StoredAssistant {
    /// A damaged entry is dropped (logged) rather than stopping the app.
    pub(crate) fn sanitized(self) -> Option<Self> {
        if preset(&self.provider).is_none() || check_url(&self.base_url).is_err() {
            tracing::warn!("desktop.json: unusable assistant settings; using the defaults");
            return None;
        }
        Some(self)
    }

    pub fn provider_label(&self) -> &'static str {
        preset(&self.provider).map_or("your provider", |p| p.label)
    }

    pub fn view(&self) -> AssistantSettings {
        AssistantSettings {
            enabled: self.enabled,
            consented: self.consented,
            provider: self.provider.clone(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            has_api_key: self.api_key.is_some(),
            live_price: self.live_price,
        }
    }
}

/// `AssistantSettings` in `types.ts`: everything but the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssistantSettings {
    pub enabled: bool,
    pub consented: bool,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub has_api_key: bool,
    pub live_price: bool,
}

/// `AssistantSettingsUpdate` in `types.ts`. `api_key: null` keeps the saved key.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantSettingsUpdate {
    pub enabled: bool,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub clear_api_key: bool,
    pub live_price: bool,
    /// The user accepted the data notice just now.
    #[serde(default)]
    pub consent: bool,
}

impl std::fmt::Debug for AssistantSettingsUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistantSettingsUpdate")
            .field("enabled", &self.enabled)
            .field("provider", &self.provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .finish_non_exhaustive()
    }
}

/// Apply `next` to what is saved. The key is never echoed in an error.
pub fn apply(
    current: &StoredAssistant,
    next: &AssistantSettingsUpdate,
) -> btcw_core::Result<StoredAssistant> {
    if preset(&next.provider).is_none() {
        return Err(config_error("the provider must be groq, gemini or custom"));
    }
    let base_url = next.base_url.trim().trim_end_matches('/').to_owned();
    check_url(&base_url)?;
    let model = next.model.trim().to_owned();
    if model.is_empty() || model.len() > MAX_MODEL_LEN || model.chars().any(char::is_control) {
        return Err(config_error("enter the model name your provider lists"));
    }
    let api_key = if next.clear_api_key {
        None
    } else {
        match next.api_key.as_deref().map(str::trim) {
            Some("") | None => current.api_key.clone(),
            Some(key) => {
                if key.len() > MAX_KEY_LEN
                    || key.chars().any(|c| c.is_whitespace() || c.is_control())
                {
                    return Err(config_error(
                        "that API key doesn't look right; paste it again",
                    ));
                }
                Some(key.to_owned())
            }
        }
    };
    let consented = current.consented || next.consent;
    if next.enabled && !consented {
        return Err(config_error(
            "turning the assistant on needs your agreement to send wallet data to the provider",
        ));
    }
    Ok(StoredAssistant {
        enabled: next.enabled,
        consented,
        provider: next.provider.clone(),
        base_url,
        model,
        api_key,
        live_price: next.live_price,
    })
}

/// `https://` only, no spaces, a sane length.
pub fn check_url(url: &str) -> btcw_core::Result<()> {
    let ok = url.len() <= MAX_URL_LEN
        && url.to_ascii_lowercase().starts_with("https://")
        && url.len() > "https://".len()
        && !url.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok {
        Ok(())
    } else {
        Err(config_error(
            "the provider URL must be a full https:// URL, like https://api.groq.com/openai/v1",
        ))
    }
}

fn config_error(message: &str) -> WalletError {
    WalletError::Config(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update() -> AssistantSettingsUpdate {
        AssistantSettingsUpdate {
            enabled: false,
            provider: "groq".into(),
            base_url: PRESETS[0].base_url.into(),
            model: PRESETS[0].model.into(),
            api_key: None,
            clear_api_key: false,
            live_price: false,
            consent: false,
        }
    }

    #[test]
    fn enabling_needs_consent_once() {
        let base = StoredAssistant::default();
        let on = AssistantSettingsUpdate {
            enabled: true,
            ..update()
        };
        assert_eq!(apply(&base, &on).unwrap_err().code(), "config");
        let agreed = apply(
            &base,
            &AssistantSettingsUpdate {
                consent: true,
                ..on.clone()
            },
        )
        .unwrap();
        assert!(agreed.enabled && agreed.consented);
        // Given once, it stays given.
        let off = apply(&agreed, &update()).unwrap();
        assert!(!off.enabled && off.consented);
        assert!(apply(&off, &on).unwrap().enabled);
    }

    #[test]
    fn the_key_is_kept_replaced_or_cleared_and_never_shown() {
        let base = StoredAssistant::default();
        let with_key = apply(
            &base,
            &AssistantSettingsUpdate {
                api_key: Some(" gsk_secret ".into()),
                ..update()
            },
        )
        .unwrap();
        assert_eq!(with_key.api_key.as_deref(), Some("gsk_secret"));
        assert!(with_key.view().has_api_key);
        assert!(!format!("{with_key:?}").contains("gsk_secret"));
        assert!(
            !serde_json::to_string(&with_key.view())
                .unwrap()
                .contains("gsk_secret")
        );
        let kept = apply(&with_key, &update()).unwrap();
        assert_eq!(kept.api_key.as_deref(), Some("gsk_secret"));
        let cleared = apply(
            &with_key,
            &AssistantSettingsUpdate {
                clear_api_key: true,
                ..update()
            },
        )
        .unwrap();
        assert_eq!(cleared.api_key, None);
        let bad = apply(
            &base,
            &AssistantSettingsUpdate {
                api_key: Some("gsk secret".into()),
                ..update()
            },
        )
        .unwrap_err();
        assert!(!bad.to_string().contains("secret"));
    }

    #[test]
    fn only_https_urls_and_known_providers() {
        let base = StoredAssistant::default();
        for url in [
            "http://api.groq.com/openai/v1",
            "api.groq.com",
            "https://",
            "https://a b",
        ] {
            let next = AssistantSettingsUpdate {
                base_url: url.into(),
                ..update()
            };
            assert_eq!(apply(&base, &next).unwrap_err().code(), "config", "{url}");
        }
        let next = AssistantSettingsUpdate {
            provider: "openai".into(),
            ..update()
        };
        assert!(apply(&base, &next).is_err());
        let next = AssistantSettingsUpdate {
            model: " ".into(),
            ..update()
        };
        assert!(apply(&base, &next).is_err());
        let custom = AssistantSettingsUpdate {
            provider: "custom".into(),
            base_url: "https://llm.example.com/v1/".into(),
            model: "m".into(),
            ..update()
        };
        assert_eq!(
            apply(&base, &custom).unwrap().base_url,
            "https://llm.example.com/v1"
        );
    }
}
