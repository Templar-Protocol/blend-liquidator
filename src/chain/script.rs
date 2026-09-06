//! A scripted JSON-RPC server for the chain tests.
//!
//! Every method has a queue of canned answers, consumed in order, and every
//! request is recorded, so a test asserts both what the client sent and how
//! it handled what came back. An unscripted call answers HTTP 500 with the
//! method name in the body: a test that forgot to script a method fails
//! loudly instead of hanging. The canned-XDR builders below produce the
//! base64 the RPC would put in its responses.
//!
//! A canned `Result` may contain the literal string `"$ENVELOPE_HASH"`
//! anywhere in its JSON tree; the responder replaces every occurrence with
//! the lowercase hex hash of the `TransactionEnvelope` in the request's own
//! `params.transaction`, hashed for `Network::testnet` — the network every
//! `tx.rs` test signs for. This lets a script assert against the hash of
//! whatever envelope the client actually built and sent (which `send` now
//! checks its `sendTransaction` response against) instead of an arbitrary
//! literal that would never match. A request with no `transaction` param
//! leaves the placeholder text as it is.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use stellar_xdr::{
    AccountEntry, AccountEntryExt, AccountId, ContractEvent, ContractEventBody, ContractEventType,
    ContractEventV0, DiagnosticEvent, ExtensionPoint, LedgerEntryChanges, LedgerEntryData,
    LedgerFootprint, ScError, ScVal, SorobanResources, SorobanTransactionData,
    SorobanTransactionDataExt, SorobanTransactionMeta, SorobanTransactionMetaExt,
    SorobanTransactionMetaV2, String32, StringM, Thresholds, TransactionEnvelope, TransactionMeta,
    TransactionMetaV3, TransactionMetaV4, TransactionResult, TransactionResultExt,
    TransactionResultResult, VecM,
};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::chain::signer::Network;
use crate::chain::xdr::encode::{from_base64, symbol, to_base64};
use crate::chain::TxHash;

/// The placeholder a canned `Result` substitutes for the hash of the
/// request's own envelope. See the module doc.
const ENVELOPE_HASH_PLACEHOLDER: &str = "$ENVELOPE_HASH";

/// The hex hash of the `TransactionEnvelope` in `request`'s
/// `params.transaction`, hashed for testnet, when there is one to decode.
fn request_envelope_hash(request: &Value) -> Option<String> {
    let encoded = request["params"]["transaction"].as_str()?;
    let envelope: TransactionEnvelope = from_base64(encoded).ok()?;
    let hash = envelope.hash(Network::testnet().id).ok()?;
    Some(TxHash(hash).to_hex())
}

/// Replaces every `ENVELOPE_HASH_PLACEHOLDER` string found anywhere in
/// `value` with `hash`, walking arrays and objects. Any other JSON value is
/// left untouched.
fn substitute_envelope_hash(value: &mut Value, hash: &str) {
    match value {
        Value::String(text) if text.as_str() == ENVELOPE_HASH_PLACEHOLDER => {
            hash.clone_into(text);
        }
        Value::Array(items) => {
            for item in items {
                substitute_envelope_hash(item, hash);
            }
        }
        Value::Object(fields) => {
            for field in fields.values_mut() {
                substitute_envelope_hash(field, hash);
            }
        }
        _ => {}
    }
}

/// One canned answer.
pub(crate) enum Canned {
    /// A JSON-RPC `result`.
    Result(Value),
    /// A JSON-RPC `error` object.
    Error { code: i64, message: String },
    /// A bare HTTP status with an empty body.
    Http(u16),
}

#[derive(Default)]
struct State {
    script: HashMap<String, VecDeque<Canned>>,
    calls: Vec<(String, Value)>,
}

/// The server and its script. Dropping it stops the server.
pub(crate) struct ScriptedRpc {
    server: MockServer,
    state: Arc<Mutex<State>>,
}

struct Responder {
    state: Arc<Mutex<State>>,
}

impl Respond for Responder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(error) => {
                return ResponseTemplate::new(400)
                    .set_body_string(format!("request body is not JSON: {error}"))
            }
        };
        let method = body["method"].as_str().unwrap_or_default().to_string();
        let id = body["id"].clone();
        let mut state = self.state.lock().expect("script mutex");
        state.calls.push((method.clone(), body["params"].clone()));
        match state.script.get_mut(&method).and_then(VecDeque::pop_front) {
            Some(Canned::Result(mut result)) => {
                if let Some(hash) = request_envelope_hash(&body) {
                    substitute_envelope_hash(&mut result, &hash);
                }
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            }
            Some(Canned::Error { code, message }) => ResponseTemplate::new(200).set_body_json(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
            ),
            Some(Canned::Http(status)) => ResponseTemplate::new(status),
            None => {
                ResponseTemplate::new(500).set_body_string(format!("unscripted method {method}"))
            }
        }
    }
}

impl ScriptedRpc {
    pub(crate) async fn start() -> Self {
        let server = MockServer::start().await;
        let state = Arc::new(Mutex::new(State::default()));
        Mock::given(method("POST"))
            .respond_with(Responder {
                state: Arc::clone(&state),
            })
            .mount(&server)
            .await;
        Self { server, state }
    }

    pub(crate) fn url(&self) -> String {
        self.server.uri()
    }

    fn push(&self, method: &str, canned: Canned) -> &Self {
        self.state
            .lock()
            .expect("script mutex")
            .script
            .entry(method.to_string())
            .or_default()
            .push_back(canned);
        self
    }

    /// Queues a JSON-RPC `result` for `method`.
    pub(crate) fn expect(&self, method: &str, result: Value) -> &Self {
        self.push(method, Canned::Result(result))
    }

    /// Queues a JSON-RPC `error` for `method`.
    pub(crate) fn expect_error(&self, method: &str, code: i64, message: &str) -> &Self {
        self.push(
            method,
            Canned::Error {
                code,
                message: message.to_string(),
            },
        )
    }

    /// Queues a bare HTTP status for `method`.
    pub(crate) fn expect_http(&self, method: &str, status: u16) -> &Self {
        self.push(method, Canned::Http(status))
    }

    /// The `params` of every call to `method`, in order.
    pub(crate) fn calls(&self, method: &str) -> Vec<Value> {
        self.state
            .lock()
            .expect("script mutex")
            .calls
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    /// Canned answers not yet consumed. A test that scripts exactly what it
    /// needs asserts this is zero at the end.
    pub(crate) fn remaining(&self) -> usize {
        self.state
            .lock()
            .expect("script mutex")
            .script
            .values()
            .map(VecDeque::len)
            .sum()
    }

    /// Every HTTP request the server saw, headers included.
    pub(crate) async fn received(&self) -> Vec<Request> {
        self.server.received_requests().await.unwrap_or_default()
    }
}

/// A `LedgerEntryData::Account` for `account` at `sequence`, base64.
pub(crate) fn account_entry_b64(account: &str, sequence: i64) -> String {
    let account_id: AccountId = account.parse().expect("account strkey");
    let entry = LedgerEntryData::Account(AccountEntry {
        account_id,
        balance: 1_000_000_000,
        seq_num: stellar_xdr::SequenceNumber(sequence),
        num_sub_entries: 0,
        inflation_dest: None,
        flags: 0,
        home_domain: String32(StringM::default()),
        thresholds: Thresholds([1, 0, 0, 0]),
        signers: VecM::default(),
        ext: AccountEntryExt::V0,
    });
    to_base64(&entry).expect("encodes")
}

/// Any `ScVal`, base64.
pub(crate) fn scval_b64(value: &ScVal) -> String {
    to_base64(value).expect("encodes")
}

/// Empty resources with the given resource fee, base64, as `transactionData`.
pub(crate) fn transaction_data_b64(resource_fee: i64) -> String {
    let data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: VecM::default(),
                read_write: VecM::default(),
            },
            instructions: 1,
            disk_read_bytes: 0,
            write_bytes: 0,
        },
        resource_fee,
    };
    to_base64(&data).expect("encodes")
}

/// The diagnostic event the host emits for a contract error:
/// topics `[error, Error(Contract, code)]`.
pub(crate) fn diagnostic_error(code: u32) -> DiagnosticEvent {
    DiagnosticEvent {
        in_successful_contract_call: false,
        event: ContractEvent {
            ext: ExtensionPoint::V0,
            contract_id: None,
            type_: ContractEventType::Diagnostic,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: VecM::try_from(vec![
                    symbol("error").expect("symbol"),
                    ScVal::Error(ScError::Contract(code)),
                ])
                .expect("two topics"),
                data: ScVal::Void,
            }),
        },
    }
}

/// `diagnostic_error`, base64.
pub(crate) fn diagnostic_error_b64(code: u32) -> String {
    to_base64(&diagnostic_error(code)).expect("encodes")
}

/// A `TransactionResult` with the given result, base64.
pub(crate) fn result_b64(result: TransactionResultResult) -> String {
    to_base64(&TransactionResult {
        fee_charged: 100,
        result,
        ext: TransactionResultExt::V0,
    })
    .expect("encodes")
}

/// A V4 transaction meta carrying a Soroban return value and diagnostics,
/// base64, as `resultMetaXdr`.
pub(crate) fn meta_v4_b64(
    return_value: Option<ScVal>,
    diagnostics: Vec<DiagnosticEvent>,
) -> String {
    let meta = TransactionMeta::V4(TransactionMetaV4 {
        ext: ExtensionPoint::V0,
        tx_changes_before: LedgerEntryChanges(VecM::default()),
        operations: VecM::default(),
        tx_changes_after: LedgerEntryChanges(VecM::default()),
        soroban_meta: Some(SorobanTransactionMetaV2 {
            ext: SorobanTransactionMetaExt::V0,
            return_value,
        }),
        events: VecM::default(),
        diagnostic_events: VecM::try_from(diagnostics).expect("diagnostics fit"),
    });
    to_base64(&meta).expect("encodes")
}

/// A V3 transaction meta carrying a Soroban return value and diagnostics,
/// base64, as `resultMetaXdr` — the pre-Protocol-23 shape, still one an RPC
/// can hand back for an older transaction.
pub(crate) fn meta_v3_b64(return_value: ScVal, diagnostics: Vec<DiagnosticEvent>) -> String {
    let meta = TransactionMeta::V3(TransactionMetaV3 {
        ext: ExtensionPoint::V0,
        tx_changes_before: LedgerEntryChanges(VecM::default()),
        operations: VecM::default(),
        tx_changes_after: LedgerEntryChanges(VecM::default()),
        soroban_meta: Some(SorobanTransactionMeta {
            ext: SorobanTransactionMetaExt::V0,
            events: VecM::default(),
            return_value,
            diagnostic_events: VecM::try_from(diagnostics).expect("fit"),
        }),
    });
    to_base64(&meta).expect("encodes")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body that is not valid JSON at all cannot carry a `method`, so it
    /// must not be answered as if it were merely an unscripted one: the
    /// scripted server rejects it loudly at HTTP 400 with the parse error,
    /// and records nothing, instead of decaying to "unscripted method".
    #[tokio::test]
    async fn a_non_json_body_is_answered_with_http_400() {
        let rpc = ScriptedRpc::start().await;
        let response = reqwest::Client::new()
            .post(rpc.url())
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
}
