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

use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::chain::ChainError;
use crate::config::ChainConfig;

/// A connected client. Cheap to clone: `reqwest::Client` is a handle.
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

impl RpcClient {
    /// A client for `url`, sending `api_key` as `(header name, value)` on
    /// every request when given. Requests time out after 30 seconds and
    /// connections after 10.
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
                let value = HeaderValue::from_str(value)
                    .map_err(|_| ChainError::Config("RPC_API_KEY is not a valid header value"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::script::ScriptedRpc;
    use serde_json::json;

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
}
