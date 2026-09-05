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

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use stellar_xdr::{LedgerEntryData, LedgerKey, LedgerKeyAccount, ScVal};

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
    /// ledgers: a moved ledger is `LedgerMoved` and the caller retries.
    pub async fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<LedgerEntries, ChainError> {
        if keys.is_empty() {
            return Err(ChainError::Config(
                "getLedgerEntries needs at least one key",
            ));
        }
        let mut latest_ledger = None;
        let mut entries = BTreeMap::new();
        for batch in keys.chunks(ENTRY_BATCH) {
            let encoded: Vec<String> = batch.iter().map(to_base64).collect::<Result<_, _>>()?;
            let raw: RawEntries = self
                .call("getLedgerEntries", json!({ "keys": encoded }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::script::account_entry_b64;
    use crate::chain::script::ScriptedRpc;
    use crate::chain::xdr::{encode, keys};
    use serde_json::json;
    use stellar_xdr::{LedgerEntryData, LedgerKey, ScVal};

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
}
