//! The tools the model may call. Each one reads the wallet or prepares a payment for the user to
//! review; none can sign or broadcast, and none returns keys, the phrase, or a PSBT.

use btcw_core::chain::Node;
use serde_json::{Value, json};

use super::{Card, PrepareRequest, provider};
use crate::commands;
use crate::error::ApiError;
use crate::state::{Activity, AppState};

pub const MAX_LIST: u64 = 50;
const DEFAULT_LIST: u64 = 10;

/// What a tool call gave: the JSON for the model, and maybe a card for the user.
pub struct Outcome {
    pub content: Value,
    pub card: Option<Card>,
}

impl Outcome {
    fn data(content: Value) -> Self {
        Self {
            content,
            card: None,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self::data(json!({ "error": message.into() }))
    }
}

/// The JSON-schema function definitions sent with every request.
pub fn definitions(live_price: bool) -> Vec<Value> {
    let mut tools = vec![
        function(
            "get_balance",
            "The wallet's balance in satoshis.",
            json!({}),
        ),
        function(
            "list_transactions",
            "The wallet's most recent transactions, newest first, with the user's labels.",
            json!({ "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIST } }),
        ),
        function(
            "get_transaction",
            "One of the wallet's transactions by txid.",
            json!({ "txid": { "type": "string" } }),
        ),
        function(
            "new_receive_address",
            "A fresh address of this wallet to receive bitcoin.",
            json!({}),
        ),
        function(
            "list_contacts",
            "The user's saved contacts (name and address).",
            json!({}),
        ),
        function(
            "estimate_fee",
            "Current fee rates in sat/vB from the user's node.",
            json!({}),
        ),
        function(
            "prepare_payment",
            "Prepare a payment for the user to review. Nothing is sent: the user checks it and \
             confirms it themselves in the app. Use only when the user asked to pay someone.",
            json!({
                "to": { "type": "string", "description": "An address or a contact's name" },
                "amount_sat": { "type": "integer", "minimum": 1 },
                "fee_rate": { "type": "number", "description": "sat/vB; omit for the node's estimate" }
            }),
        ),
        function(
            "prepare_fee_bump",
            "Prepare a faster replacement (higher fee) of an unconfirmed payment the wallet sent, \
             for the user to review and confirm themselves.",
            json!({
                "txid": { "type": "string" },
                "fee_rate": { "type": "number", "description": "sat/vB; omit for the node's estimate" }
            }),
        ),
    ];
    if live_price {
        tools.push(function(
            "get_btc_price",
            "The current price of one mainnet bitcoin (testnet coins have no value).",
            json!({ "currency": { "type": "string", "description": "e.g. usd, eur" } }),
        ));
    }
    tools
}

fn function(name: &str, description: &str, properties: Value) -> Value {
    let required: Vec<&str> = match name {
        "get_transaction" | "prepare_fee_bump" => vec!["txid"],
        "prepare_payment" => vec!["to", "amount_sat"],
        "get_btc_price" => vec!["currency"],
        _ => vec![],
    };
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": { "type": "object", "properties": properties, "required": required }
        }
    })
}

pub struct Context<'a> {
    pub state: &'a AppState,
    pub live_price: bool,
    pub price_url: &'a str,
    pub timeout: std::time::Duration,
}

/// Run one tool call. Failures go back to the model as `{"error": ...}`.
pub fn run(ctx: &Context<'_>, name: &str, args: &Value) -> Outcome {
    let state = ctx.state;
    let result = match name {
        "get_balance" => commands::balance(state).map(|b| json!(b)),
        "list_transactions" => {
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_LIST)
                .clamp(1, MAX_LIST);
            commands::history(state).map(|rows| {
                let limit = usize::try_from(limit).unwrap_or(usize::MAX);
                json!(rows.into_iter().take(limit).collect::<Vec<_>>())
            })
        }
        "get_transaction" => {
            let Some(txid) = args.get("txid").and_then(Value::as_str) else {
                return Outcome::error("txid is required");
            };
            let txid = txid.trim().to_ascii_lowercase();
            commands::history(state).map(|rows| match rows.into_iter().find(|r| r.txid == txid) {
                Some(row) => json!(row),
                None => json!({ "error": "no transaction with this txid in the wallet" }),
            })
        }
        "new_receive_address" => {
            commands::new_address(state).map(|a| json!({ "address": a.address, "index": a.index }))
        }
        "list_contacts" => commands::list_contacts(state).map(|list| {
            json!(
                list.into_iter()
                    .map(|c| json!({ "name": c.name, "address": c.address }))
                    .collect::<Vec<_>>()
            )
        }),
        "estimate_fee" => estimate_fee(state),
        "get_btc_price" => return price(ctx, args),
        "prepare_payment" => return prepare_payment(state, args),
        "prepare_fee_bump" => return prepare_fee_bump(state, args),
        _ => {
            return Outcome::error(format!(
                "there is no tool named `{}`; payments are only ever confirmed by the user",
                name.chars().take(40).collect::<String>()
            ));
        }
    };
    match result {
        Ok(content) => Outcome::data(content),
        Err(e) => Outcome::error(e.message),
    }
}

fn estimate_fee(state: &AppState) -> Result<Value, ApiError> {
    let cfg = state.begin(Activity::Background)?;
    let node = Node::connect(&cfg.rpc, cfg.network)?;
    let mut rates = serde_json::Map::new();
    for (name, blocks) in [
        ("fast_2_blocks", 2),
        ("normal_6_blocks", 6),
        ("slow_1_day", 144),
    ] {
        let rate = node.estimate_fee_rate(blocks)?;
        rates.insert(name.into(), json!(rate.to_sat_per_kwu() as f64 / 250.0));
    }
    rates.insert("unit".into(), json!("sat/vB"));
    Ok(Value::Object(rates))
}

fn price(ctx: &Context<'_>, args: &Value) -> Outcome {
    if !ctx.live_price {
        return Outcome::error("live prices are turned off in Settings");
    }
    let currency = args
        .get("currency")
        .and_then(Value::as_str)
        .unwrap_or("usd")
        .trim()
        .to_ascii_lowercase();
    if !(3..=5).contains(&currency.len()) || !currency.chars().all(|c| c.is_ascii_lowercase()) {
        return Outcome::error("currency must be a code like usd or eur");
    }
    match provider::btc_price(ctx.price_url, &currency, ctx.timeout) {
        Ok(value) => Outcome::data(json!({
            "currency": currency,
            "price_per_btc": value,
            "note": "price of mainnet BTC; testnet, signet and regtest coins have no value",
        })),
        Err(e) => Outcome::error(e.message),
    }
}

fn fee_rate(args: &Value) -> Result<Option<f64>, &'static str> {
    match args.get("fee_rate") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or("fee_rate must be a number of sat/vB"),
    }
}

fn prepare_payment(state: &AppState, args: &Value) -> Outcome {
    let Some(to) = args.get("to").and_then(Value::as_str).map(str::trim) else {
        return Outcome::error("to is required");
    };
    let Some(amount_sat) = args
        .get("amount_sat")
        .and_then(Value::as_u64)
        .filter(|a| *a > 0)
    else {
        return Outcome::error("amount_sat must be a whole number of satoshis above zero");
    };
    let fee_rate = match fee_rate(args) {
        Ok(rate) => rate,
        Err(message) => return Outcome::error(message),
    };
    let request = PrepareRequest::Payment {
        to: to.to_owned(),
        amount_sat,
        fee_rate_sat_vb: fee_rate,
    };
    prepared(
        commands::prepare_send(state, to, amount_sat, fee_rate),
        request,
        false,
    )
}

fn prepare_fee_bump(state: &AppState, args: &Value) -> Outcome {
    let Some(txid) = args.get("txid").and_then(Value::as_str).map(str::trim) else {
        return Outcome::error("txid is required");
    };
    let fee_rate = match fee_rate(args) {
        Ok(rate) => rate,
        Err(message) => return Outcome::error(message),
    };
    let request = PrepareRequest::FeeBump {
        txid: txid.to_owned(),
        fee_rate_sat_vb: fee_rate,
    };
    prepared(
        commands::prepare_fee_bump(state, txid, fee_rate),
        request,
        true,
    )
}

/// The model learns what the card shows, never its id.
fn prepared(
    result: Result<commands::PreparedSend, ApiError>,
    request: PrepareRequest,
    bump: bool,
) -> Outcome {
    match result {
        Ok(prepared) => {
            let p = &prepared.preview;
            let content = json!({
                "status": "shown to the user as a card; nothing has been sent",
                "to": p.to,
                "contact": p.contact,
                "amount_sat": p.amount_sat,
                "fee_sat": p.fee_sat,
                "fee_rate_sat_vb": p.fee_rate_sat_vb,
                "total_sat": p.total_sat,
                "replaces": p.replaces,
                "next": "only the user can confirm it, in the app",
            });
            let card = if bump {
                Card::FeeBump {
                    id: prepared.id,
                    preview: prepared.preview,
                }
            } else {
                Card::Payment {
                    id: prepared.id,
                    preview: prepared.preview,
                }
            };
            Outcome {
                content,
                card: Some(card),
            }
        }
        Err(e) if e.code == ApiError::LOCKED => Outcome {
            content: json!({
                "status": "the wallet is locked; the user was shown a button to unlock and review it",
            }),
            card: Some(Card::NeedsUnlock { request }),
        },
        Err(e) => Outcome::error(e.message),
    }
}
