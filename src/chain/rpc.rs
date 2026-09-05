//! The Soroban JSON-RPC client: the eight methods the bot uses, their wire
//! shapes, and the decoding of every base64 XDR field at the boundary.
//!
//! Hand-written rather than the `stellar-rpc-client` crate (see the plan's
//! rulings): the shapes are flat, there are few of them, and keeping them
//! here keeps every field the bot depends on visible and pinned by a test
//! against the scripted server in `chain::script`.
//!
//! Every response carries the ledger it was taken at, and every method
//! returns it, because a snapshot used for a decision must never be older
//! than the tick that triggered it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use stellar_xdr::{
    ContractEventBody, DiagnosticEvent, LedgerEntryData, LedgerKey, LedgerKeyAccount, ScError,
    ScVal, SorobanAuthorizationEntry, SorobanTransactionData, TransactionEnvelope, TransactionMeta,
    TransactionResult,
};

use crate::chain::xdr::encode::{from_base64, to_base64};
use crate::chain::xdr::XdrError;
use crate::chain::{ChainError, TxHash};
use crate::config::ChainConfig;

/// A connected client. Cheap to clone: `reqwest::Client` is a handle.
///
/// The API key, when configured, is stored as a `HeaderValue` marked
/// sensitive: its `Debug` prints `Sensitive` rather than the bytes, so the
/// derived `Debug` on this struct — and any `tracing::debug!(?client)` — can
/// never put the key in a log line.
///
/// `url` renders in full through that same derived `Debug`: the crate
/// treats the RPC URL as configuration, not a secret. A provider whose
/// authentication is keyed through the URL path itself (rather than a
/// header) must not be used that way with this client — pass the key
/// through the header form (`api_key`) instead, or it ends up in a log line
/// the moment something debug-prints the client.
#[derive(Debug, Clone)]
pub struct RpcClient {
    http: reqwest::Client,
    url: String,
    api_key: Option<(HeaderName, HeaderValue)>,
}

#[derive(Serialize)]
struct JsonRpcRequest<'a, P: Serialize> {
    jsonrpc: &'static str,
    id: u32,
    method: &'a str,
    params: P,
}

#[derive(Deserialize)]
struct JsonRpcResponse<R> {
    result: Option<R>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// The RPC renders some integers as JSON strings (close times, fees) and
/// some as numbers; a field decoded with this accepts either.
fn number_or_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(u64),
        Text(String),
    }
    match Raw::deserialize(deserializer)? {
        Raw::Number(number) => Ok(number),
        Raw::Text(text) => text.parse().map_err(serde::de::Error::custom),
    }
}

/// `getHealth`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    /// `"healthy"` when the RPC is serving.
    pub status: String,
    /// The newest ledger the RPC has ingested.
    pub latest_ledger: u32,
    /// Its close time, unix seconds.
    #[serde(deserialize_with = "number_or_string")]
    pub latest_ledger_close_time: u64,
    /// The oldest ledger the RPC still serves events and transactions for.
    pub oldest_ledger: u32,
    /// How many ledgers the RPC retains.
    pub ledger_retention_window: u32,
}

/// `getLatestLedger`, without the header and metadata blobs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestLedger {
    /// The ledger sequence.
    pub sequence: u32,
    /// The protocol version it closed under.
    pub protocol_version: u32,
    /// Its close time, unix seconds.
    #[serde(deserialize_with = "number_or_string")]
    pub close_time: u64,
}

/// The RPC's cap on keys per `getLedgerEntries` call.
const ENTRY_BATCH: usize = 200;
/// The RPC's cap on contract ids per event filter, and on filters per call.
const IDS_PER_FILTER: usize = 5;
const FILTERS_PER_CALL: usize = 5;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntries {
    latest_ledger: u32,
    #[serde(default)]
    entries: Vec<RawEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    key: String,
    xdr: String,
    last_modified_ledger_seq: u32,
    live_until_ledger_seq: Option<u32>,
}

/// One ledger entry as the RPC returned it, decoded.
#[derive(Debug, Clone)]
pub struct LedgerEntry {
    /// The entry.
    pub data: LedgerEntryData,
    /// The ledger that last wrote it.
    pub last_modified_ledger: u32,
    /// For contract data and code: the ledger it is archived after.
    pub live_until_ledger: Option<u32>,
}

/// The entries one `ledger_entries` call returned, keyed by their key's
/// base64 so a lookup never depends on the RPC's ordering. Absent keys are
/// simply not present — the RPC omits them rather than nulling them.
#[derive(Debug, Clone)]
pub struct LedgerEntries {
    /// The ledger every entry describes.
    pub latest_ledger: u32,
    entries: BTreeMap<String, LedgerEntry>,
}

impl LedgerEntries {
    /// The entry for `key`, or `None` when the ledger has no such entry.
    pub fn get(&self, key: &LedgerKey) -> Result<Option<&LedgerEntry>, ChainError> {
        Ok(self.entries.get(&to_base64(key)?))
    }

    /// How many of the requested keys had an entry.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether none of the requested keys had an entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The signing account's state that a transaction needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Account {
    /// The account's current sequence number; a transaction uses the next.
    pub sequence: i64,
    /// The ledger the sequence was read at.
    pub latest_ledger: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFeeStats {
    soroban_inclusion_fee: RawFeeDistribution,
    latest_ledger: u32,
}

#[derive(Deserialize)]
struct RawFeeDistribution {
    p70: String,
    p90: String,
}

/// The Soroban inclusion-fee percentiles the fee policy reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeStats {
    /// p70 of recent Soroban inclusion fees, stroops.
    pub soroban_percentile_70: u32,
    /// p90 of recent Soroban inclusion fees, stroops.
    pub soroban_percentile_90: u32,
    /// The ledger the statistics were computed at.
    pub latest_ledger: u32,
}

/// What to ask `getEvents` for. Exactly one of `start_ledger` and `cursor`
/// is given: the RPC rejects both and needs one.
#[derive(Debug, Clone, Copy)]
pub struct EventQuery<'a> {
    /// First ledger to read, for a fresh query.
    pub start_ledger: Option<u32>,
    /// Where the previous page ended, for the next page.
    pub cursor: Option<&'a str>,
    /// The contracts whose events to read; at most 25.
    pub contract_ids: &'a [&'a str],
    /// Page size; the RPC caps it at 10 000.
    pub limit: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEvents {
    latest_ledger: u32,
    cursor: Option<String>,
    #[serde(default)]
    events: Vec<RawEvent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEvent {
    ledger: u32,
    id: String,
    tx_hash: String,
    contract_id: String,
    in_successful_contract_call: bool,
    topic: Vec<String>,
    value: String,
}

/// One contract event, topics and value decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The ledger it was emitted in.
    pub ledger: u32,
    /// The RPC's event id, also usable as a cursor.
    pub id: String,
    /// The transaction that emitted it.
    pub tx_hash: TxHash,
    /// The emitting contract, as a `C…` strkey.
    pub contract_id: String,
    /// Whether the emitting call succeeded (events of failed calls are
    /// diagnostic only).
    pub in_successful_contract_call: bool,
    /// The topics, the first being the event name.
    pub topics: Vec<ScVal>,
    /// The data.
    pub value: ScVal,
}

/// One page of events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Events {
    /// The newest ledger the RPC had when it answered.
    pub latest_ledger: u32,
    /// Where this page ended; pass it back as the next query's cursor.
    pub cursor: Option<String>,
    /// The events, in ledger order.
    pub events: Vec<Event>,
}

impl RpcClient {
    /// A client for `url`, sending `api_key` as `(header name, value)` on
    /// every request when given. Requests time out after 30 seconds and
    /// connections after 10.
    ///
    /// The value is marked as a sensitive header, so it never renders
    /// through `Debug` — see the invariant on [`RpcClient`].
    pub fn new(url: &str, api_key: Option<(&str, &str)>) -> Result<Self, ChainError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("blend-liquidator/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let api_key = api_key
            .map(|(name, value)| {
                let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    ChainError::Config("RPC_API_KEY_HEADER is not a valid header name")
                })?;
                let mut value = HeaderValue::from_str(value)
                    .map_err(|_| ChainError::Config("RPC_API_KEY is not a valid header value"))?;
                // The key is a secret: a sensitive HeaderValue renders as
                // `Sensitive` in Debug, so the derived Debug on RpcClient
                // can never put it in a log line.
                value.set_sensitive(true);
                Ok::<_, ChainError>((name, value))
            })
            .transpose()?;
        Ok(Self {
            http,
            url: url.to_string(),
            api_key,
        })
    }

    /// A client from validated configuration.
    pub fn from_config(config: &ChainConfig) -> Result<Self, ChainError> {
        let api_key = config
            .rpc_api_key
            .as_ref()
            .map(|(name, value)| (name.as_str(), value.expose()));
        Self::new(&config.rpc_url, api_key)
    }

    /// One JSON-RPC call. A non-2xx status is `Http`, a JSON-RPC error
    /// object is `Rpc`, a 200 whose body is neither result nor error — or
    /// a result serde cannot read into `R` — is `Shape`.
    async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, ChainError> {
        let mut request = self.http.post(&self.url).json(&JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method,
            params,
        });
        if let Some((name, value)) = &self.api_key {
            request = request.header(name.clone(), value.clone());
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(ChainError::Http(status.as_u16()));
        }
        let text = response.text().await?;
        let body: JsonRpcResponse<R> = serde_json::from_str(&text)
            .map_err(|error| ChainError::Shape(format!("{method}: {error}")))?;
        match (body.result, body.error) {
            (_, Some(error)) => Err(ChainError::Rpc {
                code: error.code,
                message: error.message,
            }),
            (Some(result), None) => Ok(result),
            (None, None) => Err(ChainError::Shape(format!(
                "{method}: neither result nor error in the response"
            ))),
        }
    }

    /// `getHealth`.
    pub async fn health(&self) -> Result<Health, ChainError> {
        self.call("getHealth", serde_json::json!({})).await
    }

    /// `getLatestLedger`.
    pub async fn latest_ledger(&self) -> Result<LatestLedger, ChainError> {
        self.call("getLatestLedger", serde_json::json!({})).await
    }
}

impl RpcClient {
    /// `getLedgerEntries` for every key, in batches of 200. Every batch must
    /// report the same `latestLedger`, or the result would describe two
    /// ledgers: a moved ledger is `LedgerMoved` and the caller retries. An
    /// entry whose key was not among those requested is dropped rather than
    /// trusted, so `LedgerEntries::len` and lookups stay keyed only by what
    /// the caller asked for, regardless of what an RPC hands back.
    pub async fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<LedgerEntries, ChainError> {
        if keys.is_empty() {
            return Err(ChainError::Config(
                "getLedgerEntries needs at least one key",
            ));
        }
        let encoded_keys: Vec<String> = keys.iter().map(to_base64).collect::<Result<_, _>>()?;
        let requested: BTreeSet<&str> = encoded_keys.iter().map(String::as_str).collect();
        let mut latest_ledger = None;
        let mut entries = BTreeMap::new();
        for batch in encoded_keys.chunks(ENTRY_BATCH) {
            let raw: RawEntries = self
                .call("getLedgerEntries", json!({ "keys": batch }))
                .await?;
            match latest_ledger {
                None => latest_ledger = Some(raw.latest_ledger),
                Some(first) if first != raw.latest_ledger => {
                    return Err(ChainError::LedgerMoved {
                        first,
                        second: raw.latest_ledger,
                    })
                }
                Some(_) => {}
            }
            for entry in raw.entries {
                if !requested.contains(entry.key.as_str()) {
                    tracing::warn!(
                        key = entry.key,
                        "the RPC returned an entry that was not requested"
                    );
                    continue;
                }
                let data = from_base64(&entry.xdr)?;
                entries.insert(
                    entry.key,
                    LedgerEntry {
                        data,
                        last_modified_ledger: entry.last_modified_ledger_seq,
                        live_until_ledger: entry.live_until_ledger_seq,
                    },
                );
            }
        }
        Ok(LedgerEntries {
            // Reached only after at least one batch (the empty-keys case
            // returns above), so this default is never observed.
            latest_ledger: latest_ledger.unwrap_or_default(),
            entries,
        })
    }

    /// The account entry's sequence number. A missing entry is `NoAccount`:
    /// the account has never been funded on this network.
    pub async fn account(&self, account: &str) -> Result<Account, ChainError> {
        let account_id = account
            .parse()
            .map_err(|_| XdrError::Address(account.to_string()))?;
        let key = LedgerKey::Account(LedgerKeyAccount { account_id });
        let entries = self.ledger_entries(std::slice::from_ref(&key)).await?;
        match entries.get(&key)? {
            Some(LedgerEntry {
                data: LedgerEntryData::Account(entry),
                ..
            }) => Ok(Account {
                sequence: entry.seq_num.0,
                latest_ledger: entries.latest_ledger,
            }),
            Some(other) => Err(ChainError::Shape(format!(
                "account key returned a non-account entry: {:?}",
                other.data
            ))),
            None => Err(ChainError::NoAccount(account.to_string())),
        }
    }

    /// `getFeeStats`, reduced to the two Soroban percentiles the fee policy
    /// uses. The RPC renders percentiles as strings of stroops.
    pub async fn fee_stats(&self) -> Result<FeeStats, ChainError> {
        let raw: RawFeeStats = self.call("getFeeStats", json!({})).await?;
        let parse = |name: &str, text: &str| {
            text.parse::<u32>()
                .map_err(|_| ChainError::Shape(format!("fee percentile {name} = {text:?}")))
        };
        Ok(FeeStats {
            soroban_percentile_70: parse("p70", &raw.soroban_inclusion_fee.p70)?,
            soroban_percentile_90: parse("p90", &raw.soroban_inclusion_fee.p90)?,
            latest_ledger: raw.latest_ledger,
        })
    }

    /// `getEvents` for the given contracts: one page, topics and values
    /// decoded. Contract ids are grouped five per filter, the RPC's cap.
    pub async fn events(&self, query: &EventQuery<'_>) -> Result<Events, ChainError> {
        let filters: Vec<serde_json::Value> = query
            .contract_ids
            .chunks(IDS_PER_FILTER)
            .map(|ids| json!({ "type": "contract", "contractIds": ids }))
            .collect();
        if filters.is_empty() || filters.len() > FILTERS_PER_CALL {
            return Err(ChainError::Config("getEvents takes 1 to 25 contract ids"));
        }
        let mut params = json!({ "filters": filters, "pagination": { "limit": query.limit } });
        match (query.start_ledger, query.cursor) {
            (Some(start), None) => params["startLedger"] = json!(start),
            (None, Some(cursor)) => params["pagination"]["cursor"] = json!(cursor),
            _ => {
                return Err(ChainError::Config(
                    "getEvents needs a start ledger or a cursor, not both",
                ))
            }
        }
        let raw: RawEvents = self.call("getEvents", params).await?;
        let mut events = Vec::with_capacity(raw.events.len());
        for event in raw.events {
            let topics = event
                .topic
                .iter()
                .map(|topic| from_base64::<ScVal>(topic.as_str()))
                .collect::<Result<Vec<_>, _>>()?;
            events.push(Event {
                ledger: event.ledger,
                id: event.id,
                tx_hash: TxHash::from_hex(&event.tx_hash)?,
                contract_id: event.contract_id,
                in_successful_contract_call: event.in_successful_contract_call,
                topics,
                value: from_base64(&event.value)?,
            });
        }
        Ok(Events {
            latest_ledger: raw.latest_ledger,
            cursor: raw.cursor,
            events,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSimulation {
    latest_ledger: u32,
    error: Option<String>,
    min_resource_fee: Option<String>,
    transaction_data: Option<String>,
    #[serde(default)]
    results: Vec<RawSimulationResult>,
    restore_preamble: Option<RawPreamble>,
    #[serde(default)]
    events: Vec<String>,
}

#[derive(Deserialize)]
struct RawSimulationResult {
    #[serde(default)]
    auth: Vec<String>,
    xdr: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPreamble {
    min_resource_fee: String,
    transaction_data: String,
}

/// What a `RestoreFootprint` transaction needs to bring archived entries
/// back before the simulated call can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePreamble {
    /// The restore transaction's Soroban data (its footprint and resources).
    pub transaction_data: SorobanTransactionData,
    /// Its resource fee, stroops.
    pub min_resource_fee: i64,
}

/// A simulation that ran to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedCall {
    /// The host function's return value.
    pub return_value: ScVal,
    /// Authorisation entries the call needs; empty when the source account
    /// covers it.
    pub auth: Vec<SorobanAuthorizationEntry>,
    /// The footprint and resources to attach to the transaction.
    pub transaction_data: SorobanTransactionData,
    /// The resource fee to add to the inclusion fee, stroops.
    pub min_resource_fee: i64,
    /// Present when archived entries must be restored first; the data and
    /// fee above are then not to be trusted until a second simulation.
    pub restore: Option<RestorePreamble>,
}

/// Success or the host's refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimulationOutcome {
    /// The call ran. Boxed so the enum stays small next to `Failure`.
    Success(Box<SimulatedCall>),
    /// The host refused: a contract error, a trap, or a malformed envelope.
    Failure {
        /// The RPC's text, diagnostic log included.
        message: String,
        /// The pool's error code when the failure was a contract error.
        contract_error: Option<u32>,
    },
}

/// `simulateTransaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Simulation {
    /// The ledger the simulation ran against.
    pub latest_ledger: u32,
    /// The diagnostic events the host emitted, decoded.
    pub events: Vec<DiagnosticEvent>,
    /// What happened.
    pub outcome: SimulationOutcome,
}

fn parse_i64(name: &str, text: &str) -> Result<i64, ChainError> {
    text.parse::<i64>()
        .map_err(|_| ChainError::Shape(format!("{name} = {text:?} is not an integer")))
}

/// The contract error code in a diagnostic event log: the host emits an
/// event with topics `[error, Error(Contract, code)]` when a contract
/// fails with an error. The first such event wins.
#[must_use]
pub fn contract_error_in_events(events: &[DiagnosticEvent]) -> Option<u32> {
    events.iter().find_map(|event| {
        let ContractEventBody::V0(body) = &event.event.body;
        let topics = body.topics.as_slice();
        match (topics.first(), topics.get(1)) {
            (Some(ScVal::Symbol(name)), Some(ScVal::Error(ScError::Contract(code))))
                if name.to_utf8_string_lossy() == "error" =>
            {
                Some(*code)
            }
            _ => None,
        }
    })
}

/// The contract error code in a simulation error message, which the host
/// renders as `Error(Contract, #1200)`. Only that form counts; a WasmVm or
/// budget error carries no pool code.
#[must_use]
pub fn contract_error_in_message(message: &str) -> Option<u32> {
    const MARKER: &str = "Error(Contract, #";
    let start = message.find(MARKER)? + MARKER.len();
    let rest = &message[start..];
    let end = rest.find(')')?;
    rest[..end].parse().ok()
}

fn decode_events(events: &[String]) -> Result<Vec<DiagnosticEvent>, ChainError> {
    events
        .iter()
        .map(|event| from_base64(event).map_err(ChainError::Xdr))
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSend {
    status: String,
    hash: String,
    latest_ledger: u32,
    error_result_xdr: Option<String>,
    #[serde(default)]
    diagnostic_events_xdr: Vec<String>,
}

/// What `sendTransaction` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// Accepted into the queue; poll for the result.
    Pending,
    /// Already in the queue or applied; poll for the result.
    Duplicate,
    /// The queue is full; send again after a pause.
    TryAgainLater,
    /// Rejected before the queue: bad sequence, bad auth, insufficient fee.
    Error {
        /// The decoded result, when the RPC gave one.
        result: Option<TransactionResult>,
        /// The pool's error code, when the rejection was a contract error.
        contract_error: Option<u32>,
    },
}

/// `sendTransaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendStatus {
    /// The hash the transaction will be found under.
    pub hash: TxHash,
    /// The ledger the RPC had when it answered.
    pub latest_ledger: u32,
    /// What happened.
    pub outcome: SendOutcome,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTransaction {
    status: String,
    latest_ledger: u32,
    ledger: Option<u32>,
    result_xdr: Option<String>,
    result_meta_xdr: Option<String>,
    #[serde(default)]
    diagnostic_events_xdr: Vec<String>,
}

/// `getTransaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionStatus {
    /// The RPC has not seen the transaction in any ledger it holds.
    NotFound {
        /// The newest ledger the RPC had; against the transaction's ledger
        /// bound this decides between "still possible" and "never".
        latest_ledger: u32,
    },
    /// Applied and succeeded.
    Success {
        /// The ledger it was applied in.
        ledger: u32,
        /// The newest ledger the RPC had.
        latest_ledger: u32,
        /// The host function's return value, when the meta carries one.
        return_value: Option<ScVal>,
    },
    /// Applied and failed; the fee was charged.
    Failed {
        /// The ledger it was applied in.
        ledger: u32,
        /// The newest ledger the RPC had.
        latest_ledger: u32,
        /// The decoded result.
        result: TransactionResult,
        /// The pool's error code, when the failure was a contract error.
        contract_error: Option<u32>,
    },
}

/// The Soroban return value a transaction meta carries, if any.
fn return_value(meta: &TransactionMeta) -> Option<ScVal> {
    match meta {
        TransactionMeta::V4(meta) => meta
            .soroban_meta
            .as_ref()
            .and_then(|soroban| soroban.return_value.clone()),
        TransactionMeta::V3(meta) => meta
            .soroban_meta
            .as_ref()
            .map(|soroban| soroban.return_value.clone()),
        _ => None,
    }
}

/// The diagnostic events a transaction meta carries, if any.
fn meta_diagnostics(meta: &TransactionMeta) -> Vec<DiagnosticEvent> {
    match meta {
        TransactionMeta::V4(meta) => meta.diagnostic_events.iter().cloned().collect(),
        TransactionMeta::V3(meta) => meta
            .soroban_meta
            .as_ref()
            .map(|soroban| soroban.diagnostic_events.iter().cloned().collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

impl RpcClient {
    /// `simulateTransaction`. A failure is an `Ok(Simulation)` whose outcome
    /// is `Failure`, because the caller usually wants the contract code; a
    /// response that is neither a result nor an error is `Shape`.
    pub async fn simulate(&self, envelope: &TransactionEnvelope) -> Result<Simulation, ChainError> {
        let raw: RawSimulation = self
            .call(
                "simulateTransaction",
                json!({ "transaction": to_base64(envelope)? }),
            )
            .await?;
        let events = decode_events(&raw.events)?;
        if let Some(message) = raw.error {
            let contract_error =
                contract_error_in_events(&events).or_else(|| contract_error_in_message(&message));
            return Ok(Simulation {
                latest_ledger: raw.latest_ledger,
                events,
                outcome: SimulationOutcome::Failure {
                    message,
                    contract_error,
                },
            });
        }
        let missing =
            |field: &'static str| ChainError::Shape(format!("simulation without {field}"));
        let result = raw.results.first().ok_or_else(|| missing("results"))?;
        let auth = result
            .auth
            .iter()
            .map(|entry| from_base64(entry).map_err(ChainError::Xdr))
            .collect::<Result<Vec<SorobanAuthorizationEntry>, _>>()?;
        let restore = raw
            .restore_preamble
            .map(|preamble| {
                Ok::<_, ChainError>(RestorePreamble {
                    transaction_data: from_base64(&preamble.transaction_data)?,
                    min_resource_fee: parse_i64(
                        "restorePreamble.minResourceFee",
                        &preamble.min_resource_fee,
                    )?,
                })
            })
            .transpose()?;
        let call = SimulatedCall {
            return_value: from_base64(&result.xdr)?,
            auth,
            transaction_data: from_base64(
                raw.transaction_data
                    .as_deref()
                    .ok_or_else(|| missing("transactionData"))?,
            )?,
            min_resource_fee: parse_i64(
                "minResourceFee",
                raw.min_resource_fee
                    .as_deref()
                    .ok_or_else(|| missing("minResourceFee"))?,
            )?,
            restore,
        };
        Ok(Simulation {
            latest_ledger: raw.latest_ledger,
            events,
            outcome: SimulationOutcome::Success(Box::new(call)),
        })
    }

    /// `sendTransaction`.
    pub async fn send(&self, envelope: &TransactionEnvelope) -> Result<SendStatus, ChainError> {
        let raw: RawSend = self
            .call(
                "sendTransaction",
                json!({ "transaction": to_base64(envelope)? }),
            )
            .await?;
        let outcome = match raw.status.as_str() {
            "PENDING" => SendOutcome::Pending,
            "DUPLICATE" => SendOutcome::Duplicate,
            "TRY_AGAIN_LATER" => SendOutcome::TryAgainLater,
            "ERROR" => {
                let result = raw
                    .error_result_xdr
                    .as_deref()
                    .map(from_base64::<TransactionResult>)
                    .transpose()?;
                let events = decode_events(&raw.diagnostic_events_xdr)?;
                SendOutcome::Error {
                    result,
                    contract_error: contract_error_in_events(&events),
                }
            }
            other => {
                return Err(ChainError::Shape(format!(
                    "sendTransaction status {other:?}"
                )))
            }
        };
        Ok(SendStatus {
            hash: TxHash::from_hex(&raw.hash)?,
            latest_ledger: raw.latest_ledger,
            outcome,
        })
    }

    /// `getTransaction`.
    pub async fn transaction(&self, hash: &TxHash) -> Result<TransactionStatus, ChainError> {
        let raw: RawTransaction = self
            .call("getTransaction", json!({ "hash": hash.to_hex() }))
            .await?;
        let latest_ledger = raw.latest_ledger;
        if raw.status == "NOT_FOUND" {
            return Ok(TransactionStatus::NotFound { latest_ledger });
        }
        let missing =
            |field: &'static str| ChainError::Shape(format!("{} without {field}", raw.status));
        let ledger = raw
            .ledger
            .filter(|ledger| *ledger > 0)
            .ok_or_else(|| missing("ledger"))?;
        let meta: TransactionMeta = from_base64(
            raw.result_meta_xdr
                .as_deref()
                .ok_or_else(|| missing("resultMetaXdr"))?,
        )?;
        match raw.status.as_str() {
            "SUCCESS" => Ok(TransactionStatus::Success {
                ledger,
                latest_ledger,
                return_value: return_value(&meta),
            }),
            "FAILED" => {
                let result: TransactionResult = from_base64(
                    raw.result_xdr
                        .as_deref()
                        .ok_or_else(|| missing("resultXdr"))?,
                )?;
                let mut events = decode_events(&raw.diagnostic_events_xdr)?;
                events.extend(meta_diagnostics(&meta));
                Ok(TransactionStatus::Failed {
                    ledger,
                    latest_ledger,
                    result,
                    contract_error: contract_error_in_events(&events),
                })
            }
            other => Err(ChainError::Shape(format!(
                "getTransaction status {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::script::account_entry_b64;
    use crate::chain::script::ScriptedRpc;
    use crate::chain::script::{
        diagnostic_error_b64, meta_v4_b64, result_b64, scval_b64, transaction_data_b64,
    };
    use crate::chain::xdr::encode::{address, invoke_contract_op, simulation_envelope};
    use crate::chain::xdr::{encode, keys};
    use serde_json::json;
    use stellar_xdr::{
        InvokeHostFunctionResult, LedgerEntryData, LedgerKey, OperationResult, OperationResultTr,
        ScVal, TransactionResultResult, VecM,
    };

    fn health_json() -> serde_json::Value {
        json!({
            "status": "healthy", "latestLedger": 64_289_467,
            "latestLedgerCloseTime": "1788635204", "oldestLedger": 64_168_508,
            "oldestLedgerCloseTime": "1787949229", "ledgerRetentionWindow": 120_960
        })
    }

    #[tokio::test]
    async fn health_decodes_and_close_times_may_be_strings_or_numbers() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health_json());
        let mut numeric = health_json();
        numeric["latestLedgerCloseTime"] = json!(1_788_635_204_u64);
        rpc.expect("getHealth", numeric);
        let client = RpcClient::new(&rpc.url(), None).unwrap();

        for _ in 0..2 {
            let health = client.health().await.unwrap();
            assert_eq!(health.status, "healthy");
            assert_eq!(health.latest_ledger, 64_289_467);
            assert_eq!(health.latest_ledger_close_time, 1_788_635_204);
            assert_eq!(health.oldest_ledger, 64_168_508);
            assert_eq!(health.ledger_retention_window, 120_960);
        }
        assert_eq!(rpc.calls("getHealth").len(), 2);
        assert_eq!(rpc.remaining(), 0);
    }

    #[tokio::test]
    async fn latest_ledger_decodes_and_ignores_the_xdr_blobs() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "getLatestLedger",
            json!({"id": "70f3", "protocolVersion": 27, "sequence": 64_289_467,
                   "closeTime": "1788635204", "headerXdr": "AAAA", "metadataXdr": "AAAA"}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let ledger = client.latest_ledger().await.unwrap();
        assert_eq!(ledger.sequence, 64_289_467);
        assert_eq!(ledger.protocol_version, 27);
        assert_eq!(ledger.close_time, 1_788_635_204);
    }

    #[tokio::test]
    async fn the_request_is_json_rpc_2_with_the_method_and_params() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health_json());
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        client.health().await.unwrap();
        let requests = rpc.received().await;
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["method"], "getHealth");
        assert_eq!(
            requests[0].headers.get("content-type").unwrap(),
            "application/json"
        );
        assert!(requests[0].headers.get("x-api-key").is_none());
    }

    #[tokio::test]
    async fn the_api_key_header_is_sent_when_configured() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health_json());
        let client = RpcClient::new(&rpc.url(), Some(("X-Api-Key", "secret-123"))).unwrap();
        client.health().await.unwrap();
        let requests = rpc.received().await;
        assert_eq!(requests[0].headers.get("x-api-key").unwrap(), "secret-123");
    }

    #[test]
    fn the_api_key_never_appears_in_the_clients_debug_rendering() {
        let client = RpcClient::new("http://localhost", Some(("X-Api-Key", "secret-123"))).unwrap();
        let rendered = format!("{client:?}");
        assert!(rendered.contains("x-api-key"), "{rendered}");
        assert!(rendered.contains("Sensitive"), "{rendered}");
        assert!(!rendered.contains("secret-123"), "{rendered}");
    }

    #[tokio::test]
    async fn a_json_rpc_error_object_is_an_rpc_error() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect_error("getHealth", -32_602, "invalid params");
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let error = client.health().await.unwrap_err();
        assert!(
            matches!(&error, ChainError::Rpc { code: -32_602, message } if message == "invalid params"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_non_2xx_status_is_an_http_error_and_so_is_an_unscripted_method() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect_http("getHealth", 503);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        assert!(matches!(
            client.health().await.unwrap_err(),
            ChainError::Http(503)
        ));
        assert!(matches!(
            client.latest_ledger().await.unwrap_err(),
            ChainError::Http(500)
        ));
    }

    #[tokio::test]
    async fn a_result_of_the_wrong_shape_is_a_shape_error_not_a_panic() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", json!({"status": "healthy"}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        assert!(matches!(
            client.health().await.unwrap_err(),
            ChainError::Shape(_)
        ));
    }

    #[test]
    fn a_bad_header_name_is_a_config_error() {
        assert!(matches!(
            RpcClient::new("http://localhost", Some(("bad header", "v"))),
            Err(ChainError::Config(_))
        ));
    }

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    const ACCOUNT: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";

    #[tokio::test]
    async fn ledger_entries_are_keyed_and_an_absent_key_is_none() {
        let instance = keys::instance(POOL).unwrap();
        let positions = keys::positions(POOL, ACCOUNT).unwrap();
        let fixture = crate::fixture::mainnet_fixed_v2();
        let instance_xdr = crate::fixture::text(&fixture, &["instance_entry_xdr"]);
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": 64_271_347, "entries": [
                {"key": encode::to_base64(&instance).unwrap(), "xdr": instance_xdr,
                 "lastModifiedLedgerSeq": 61_962_028, "liveUntilLedgerSeq": 64_814_477, "extXdr": "AAAAAA=="}
            ]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let entries = client
            .ledger_entries(&[instance.clone(), positions.clone()])
            .await
            .unwrap();
        assert_eq!(entries.latest_ledger, 64_271_347);
        assert_eq!(entries.len(), 1);
        let entry = entries.get(&instance).unwrap().unwrap();
        assert!(matches!(entry.data, LedgerEntryData::ContractData(_)));
        assert_eq!(entry.last_modified_ledger, 61_962_028);
        assert_eq!(entry.live_until_ledger, Some(64_814_477));
        assert!(entries.get(&positions).unwrap().is_none());
        // The request carried both keys, base64.
        let params = rpc.calls("getLedgerEntries");
        assert_eq!(params[0]["keys"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn ledger_entries_ignore_an_entry_that_was_not_requested() {
        let instance = keys::instance(POOL).unwrap();
        let positions = keys::positions(POOL, ACCOUNT).unwrap();
        let fixture = crate::fixture::mainnet_fixed_v2();
        let instance_xdr = crate::fixture::text(&fixture, &["instance_entry_xdr"]);
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": 64_271_347, "entries": [
                {"key": encode::to_base64(&instance).unwrap(), "xdr": instance_xdr,
                 "lastModifiedLedgerSeq": 61_962_028, "liveUntilLedgerSeq": 64_814_477},
                {"key": encode::to_base64(&positions).unwrap(), "xdr": instance_xdr,
                 "lastModifiedLedgerSeq": 61_962_028}
            ]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        // Only the instance key was requested; the RPC hands back an entry
        // for the positions key too, which must not count or be reachable.
        let entries = client
            .ledger_entries(std::slice::from_ref(&instance))
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries.get(&positions).unwrap().is_none());
        assert!(entries.get(&instance).unwrap().is_some());
    }

    #[tokio::test]
    async fn ledger_entries_batch_by_200_and_refuse_a_moved_ledger() {
        let keys: Vec<LedgerKey> = (0..201).map(|_| keys::instance(POOL).unwrap()).collect();
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": 10, "entries": []}),
        );
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": 11, "entries": []}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let error = client.ledger_entries(&keys).await.unwrap_err();
        assert!(
            matches!(
                error,
                ChainError::LedgerMoved {
                    first: 10,
                    second: 11
                }
            ),
            "{error:?}"
        );
        let params = rpc.calls("getLedgerEntries");
        assert_eq!(params[0]["keys"].as_array().unwrap().len(), 200);
        assert_eq!(params[1]["keys"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn no_keys_is_a_config_error_without_a_request() {
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        assert!(matches!(
            client.ledger_entries(&[]).await.unwrap_err(),
            ChainError::Config(_)
        ));
        assert!(rpc.calls("getLedgerEntries").is_empty());
    }

    #[tokio::test]
    async fn the_account_sequence_comes_from_the_account_entry() {
        let key = LedgerKey::Account(stellar_xdr::LedgerKeyAccount {
            account_id: ACCOUNT.parse().unwrap(),
        });
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": 7, "entries": [
                {"key": encode::to_base64(&key).unwrap(), "xdr": account_entry_b64(ACCOUNT, 41),
                 "lastModifiedLedgerSeq": 5}
            ]}),
        );
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": 8, "entries": []}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let account = client.account(ACCOUNT).await.unwrap();
        assert_eq!(
            account,
            Account {
                sequence: 41,
                latest_ledger: 7
            }
        );
        let missing = client.account(ACCOUNT).await.unwrap_err();
        assert!(matches!(missing, ChainError::NoAccount(a) if a == ACCOUNT));
        assert!(matches!(
            client.account("not-a-key").await.unwrap_err(),
            ChainError::Xdr(_)
        ));
    }

    #[tokio::test]
    async fn fee_stats_read_the_soroban_percentiles() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "getFeeStats",
            json!({"sorobanInclusionFee": {"max": "300", "min": "100", "mode": "200", "p10": "100",
                    "p70": "250", "p90": "300", "p99": "300", "transactionCount": "6851", "ledgerCount": 50},
                   "inclusionFee": {"p70": "100", "p90": "100"}, "latestLedger": 64_289_468}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let fees = client.fee_stats().await.unwrap();
        assert_eq!(
            fees,
            FeeStats {
                soroban_percentile_70: 250,
                soroban_percentile_90: 300,
                latest_ledger: 64_289_468
            }
        );
    }

    #[tokio::test]
    async fn events_decode_topics_and_values_and_carry_the_cursor() {
        let fixture = crate::fixture::mainnet_fixed_v2();
        let event = &fixture["events"][0];
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 64_271_347, "latestLedgerCloseTime": "1788534414",
                   "oldestLedger": 64_150_000, "cursor": "0275527941655015424-0000000006",
                   "events": [event]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let events = client
            .events(&EventQuery {
                start_ledger: Some(64_150_000),
                cursor: None,
                contract_ids: &[POOL],
                limit: 100,
            })
            .await
            .unwrap();
        assert_eq!(events.latest_ledger, 64_271_347);
        assert_eq!(
            events.cursor.as_deref(),
            Some("0275527941655015424-0000000006")
        );
        assert_eq!(events.events.len(), 1);
        let decoded = &events.events[0];
        let expected_ledger: u32 = event["ledger"].as_u64().unwrap().try_into().unwrap();
        assert_eq!(decoded.ledger, expected_ledger);
        assert_eq!(decoded.contract_id, POOL);
        assert!(decoded.in_successful_contract_call);
        assert_eq!(decoded.tx_hash.to_hex(), event["txHash"].as_str().unwrap());
        assert!(matches!(decoded.topics[0], ScVal::Symbol(_)));
        let params = &rpc.calls("getEvents")[0];
        assert_eq!(params["startLedger"], 64_150_000);
        assert_eq!(params["filters"][0]["type"], "contract");
        assert_eq!(params["filters"][0]["contractIds"][0], POOL);
        assert_eq!(params["pagination"]["limit"], 100);
        assert!(params["pagination"].get("cursor").is_none());
    }

    #[tokio::test]
    async fn events_paginate_by_cursor_without_a_start_ledger_and_refuse_neither_or_both() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getEvents", json!({"latestLedger": 1, "events": []}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let page = client
            .events(&EventQuery {
                start_ledger: None,
                cursor: Some("c-1"),
                contract_ids: &[POOL],
                limit: 10,
            })
            .await
            .unwrap();
        assert!(page.events.is_empty());
        assert_eq!(page.cursor, None);
        let params = &rpc.calls("getEvents")[0];
        assert!(params.get("startLedger").is_none());
        assert_eq!(params["pagination"]["cursor"], "c-1");

        for query in [
            EventQuery {
                start_ledger: None,
                cursor: None,
                contract_ids: &[POOL],
                limit: 10,
            },
            EventQuery {
                start_ledger: Some(1),
                cursor: Some("c"),
                contract_ids: &[POOL],
                limit: 10,
            },
        ] {
            assert!(matches!(
                client.events(&query).await.unwrap_err(),
                ChainError::Config(_)
            ));
        }
    }

    fn envelope() -> stellar_xdr::TransactionEnvelope {
        let op =
            invoke_contract_op(POOL, "get_positions", vec![address(ACCOUNT).unwrap()]).unwrap();
        simulation_envelope(op).unwrap()
    }

    #[tokio::test]
    async fn a_successful_simulation_decodes_its_return_value_data_and_fee() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "simulateTransaction",
            json!({"transactionData": transaction_data_b64(446_953), "events": [],
                   "minResourceFee": "446953",
                   "results": [{"auth": [], "xdr": scval_b64(&ScVal::U32(7))}],
                   "latestLedger": 64_289_527}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let simulation = client.simulate(&envelope()).await.unwrap();
        assert_eq!(simulation.latest_ledger, 64_289_527);
        let SimulationOutcome::Success(call) = simulation.outcome else {
            panic!("expected success");
        };
        assert_eq!(call.return_value, ScVal::U32(7));
        assert!(call.auth.is_empty());
        assert_eq!(call.transaction_data.resource_fee, 446_953);
        assert_eq!(call.min_resource_fee, 446_953);
        assert!(call.restore.is_none());
        let params = &rpc.calls("simulateTransaction")[0];
        assert_eq!(
            params["transaction"],
            encode::to_base64(&envelope()).unwrap()
        );
    }

    #[tokio::test]
    async fn a_restore_preamble_is_carried_with_the_success() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "simulateTransaction",
            json!({"transactionData": transaction_data_b64(10), "minResourceFee": "10",
                   "results": [{"auth": [], "xdr": scval_b64(&ScVal::Void)}],
                   "restorePreamble": {"minResourceFee": "77", "transactionData": transaction_data_b64(77)},
                   "latestLedger": 5}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let simulation = client.simulate(&envelope()).await.unwrap();
        let SimulationOutcome::Success(call) = simulation.outcome else {
            panic!("expected success");
        };
        let restore = call.restore.unwrap();
        assert_eq!(restore.min_resource_fee, 77);
        assert_eq!(restore.transaction_data.resource_fee, 77);
    }

    #[tokio::test]
    async fn a_failed_simulation_reports_the_contract_error_from_events_or_message() {
        let message = "HostError: Error(Contract, #1200)\n\nEvent log (newest first):\n 0: [Diagnostic Event] …";
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "simulateTransaction",
            json!({"error": message, "events": [diagnostic_error_b64(1200)], "latestLedger": 9}),
        );
        rpc.expect(
            "simulateTransaction",
            json!({"error": message, "events": [], "latestLedger": 9}),
        );
        rpc.expect(
            "simulateTransaction",
            json!({"error": "HostError: Error(WasmVm, UnexpectedSize)", "latestLedger": 9}),
        );
        rpc.expect(
            "simulateTransaction",
            json!({"error": "Could not unmarshal transaction", "latestLedger": 0}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        for expected in [Some(1200), Some(1200), None, None] {
            let simulation = client.simulate(&envelope()).await.unwrap();
            let SimulationOutcome::Failure { contract_error, .. } = simulation.outcome else {
                panic!("expected failure");
            };
            assert_eq!(contract_error, expected);
        }
        assert_eq!(rpc.remaining(), 0);
    }

    #[test]
    fn contract_error_parsing_reads_only_the_contract_code() {
        assert_eq!(
            contract_error_in_message("HostError: Error(Contract, #1205)\n\nEvent log"),
            Some(1205)
        );
        assert_eq!(contract_error_in_message("Error(Contract, #7)"), Some(7));
        assert_eq!(
            contract_error_in_message("HostError: Error(WasmVm, UnexpectedSize)"),
            None
        );
        assert_eq!(
            contract_error_in_message("Error(Contract, #notanumber)"),
            None
        );
        assert_eq!(
            contract_error_in_events(&[crate::chain::script::diagnostic_error(1212)]),
            Some(1212)
        );
        assert_eq!(contract_error_in_events(&[]), None);
    }

    #[tokio::test]
    async fn send_decodes_every_status_and_the_error_result() {
        let hash = "ab".repeat(32);
        let rpc = ScriptedRpc::start().await;
        for status in ["PENDING", "DUPLICATE", "TRY_AGAIN_LATER"] {
            rpc.expect("sendTransaction", json!({"status": status, "hash": hash, "latestLedger": 3, "latestLedgerCloseTime": "1"}));
        }
        rpc.expect(
            "sendTransaction",
            json!({"status": "ERROR", "hash": hash, "latestLedger": 3, "latestLedgerCloseTime": "1",
                   "errorResultXdr": result_b64(TransactionResultResult::TxBadSeq),
                   "diagnosticEventsXdr": [diagnostic_error_b64(1201)]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let mut outcomes = Vec::new();
        for _ in 0..4 {
            let status = client.send(&envelope()).await.unwrap();
            assert_eq!(status.hash.to_hex(), hash);
            assert_eq!(status.latest_ledger, 3);
            outcomes.push(status.outcome);
        }
        assert!(matches!(outcomes[0], SendOutcome::Pending));
        assert!(matches!(outcomes[1], SendOutcome::Duplicate));
        assert!(matches!(outcomes[2], SendOutcome::TryAgainLater));
        let SendOutcome::Error {
            result,
            contract_error,
        } = &outcomes[3]
        else {
            panic!("expected error");
        };
        assert!(matches!(
            result.as_ref().unwrap().result,
            TransactionResultResult::TxBadSeq
        ));
        assert_eq!(*contract_error, Some(1201));
    }

    #[tokio::test]
    async fn an_unknown_send_status_is_a_shape_error() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "sendTransaction",
            json!({"status": "WEIRD", "hash": "ab".repeat(32), "latestLedger": 3}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        assert!(matches!(
            client.send(&envelope()).await.unwrap_err(),
            ChainError::Shape(_)
        ));
    }

    #[tokio::test]
    async fn transaction_status_decodes_not_found_success_and_failed() {
        let hash = TxHash([0xcd; 32]);
        let failed = TransactionResultResult::TxFailed(
            VecM::try_from(vec![OperationResult::OpInner(
                OperationResultTr::InvokeHostFunction(InvokeHostFunctionResult::Trapped),
            )])
            .unwrap(),
        );
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "getTransaction",
            json!({"status": "NOT_FOUND", "latestLedger": 100, "latestLedgerCloseTime": "1", "oldestLedger": 1,
                   "txHash": hash.to_hex(), "applicationOrder": 0, "feeBump": false, "events": {}, "ledger": 0, "createdAt": "0"}),
        );
        rpc.expect(
            "getTransaction",
            json!({"status": "SUCCESS", "latestLedger": 101, "oldestLedger": 1, "ledger": 99, "createdAt": "1788618724",
                   "txHash": hash.to_hex(), "envelopeXdr": "AAAA",
                   "resultXdr": result_b64(TransactionResultResult::TxSuccess(VecM::default())),
                   "resultMetaXdr": meta_v4_b64(Some(ScVal::U32(7)), vec![]), "diagnosticEventsXdr": []}),
        );
        rpc.expect(
            "getTransaction",
            json!({"status": "FAILED", "latestLedger": 102, "oldestLedger": 1, "ledger": 100, "createdAt": 1_788_618_800,
                   "txHash": hash.to_hex(), "resultXdr": result_b64(failed),
                   "resultMetaXdr": meta_v4_b64(None, vec![]), "diagnosticEventsXdr": [diagnostic_error_b64(1205)]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        assert!(matches!(
            client.transaction(&hash).await.unwrap(),
            TransactionStatus::NotFound { latest_ledger: 100 }
        ));
        let TransactionStatus::Success {
            ledger,
            latest_ledger,
            return_value,
        } = client.transaction(&hash).await.unwrap()
        else {
            panic!("expected success");
        };
        assert_eq!(
            (ledger, latest_ledger, return_value),
            (99, 101, Some(ScVal::U32(7)))
        );
        let TransactionStatus::Failed {
            ledger,
            contract_error,
            result,
            ..
        } = client.transaction(&hash).await.unwrap()
        else {
            panic!("expected failed");
        };
        assert_eq!((ledger, contract_error), (100, Some(1205)));
        assert!(matches!(
            result.result,
            TransactionResultResult::TxFailed(_)
        ));
        assert_eq!(rpc.calls("getTransaction")[0]["hash"], hash.to_hex());
    }
}
