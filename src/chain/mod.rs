//! Everything that touches Soroban: XDR codecs, the JSON-RPC client, pool
//! reads, the signing key and the transaction path.
//!
//! Every fallible step reports through [`ChainError`], one enum for the
//! module so a pipeline propagates with `?` and a caller matches only on the
//! variants that change its behaviour: `BadSequence` means re-plan,
//! `Simulation { contract_error }` means the contract refused, everything
//! else means retry or give up.

use crate::chain::xdr::XdrError;
use crate::math::MathError;

pub mod rpc;
#[cfg(test)]
pub(crate) mod script;
pub mod signer;
pub mod xdr;

pub use rpc::RpcClient;
pub use signer::{Network, Signer};

/// A failure anywhere between the bot and the chain.
///
/// Not `PartialEq`: `reqwest::Error` is not. Tests match on variants.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    /// The HTTP request never produced a response (DNS, TLS, timeout).
    #[error("rpc transport: {0}")]
    Transport(#[from] reqwest::Error),
    /// The RPC answered with a non-2xx status.
    #[error("rpc http status {0}")]
    Http(u16),
    /// The RPC answered with a JSON-RPC error object.
    #[error("rpc error {code}: {message}")]
    Rpc {
        /// The JSON-RPC error code.
        code: i64,
        /// The RPC's message.
        message: String,
    },
    /// The RPC answered 200 with a body this client does not understand.
    #[error("rpc response shape: {0}")]
    Shape(String),
    /// Two reads that must describe one ledger described two.
    #[error("the ledger moved between reads ({first} then {second})")]
    LedgerMoved {
        /// The ledger the first read reported.
        first: u32,
        /// The ledger a later read reported.
        second: u32,
    },
    /// The signing account does not exist on this network.
    #[error("no account entry for {0}")]
    NoAccount(String),
    /// A base64 or XDR value did not decode, or a bot type did not encode.
    #[error("xdr: {0}")]
    Xdr(#[from] XdrError),
    /// Checked arithmetic on chain values failed.
    #[error("math: {0}")]
    Math(#[from] MathError),
    /// A configuration value the chain layer cannot use.
    #[error("configuration: {0}")]
    Config(&'static str),
    /// The secret key did not parse. Never carries the text.
    #[error("the secret key is not a valid S… strkey")]
    SecretKey,
    /// The RPC refused to simulate the operation.
    #[error("simulation failed: {message}")]
    Simulation {
        /// The RPC's error text, diagnostic log included.
        message: String,
        /// The pool's error code, when the failure was a contract error.
        contract_error: Option<u32>,
    },
    /// The restore-footprint transaction did not succeed.
    #[error("restoring archived entries failed: {0}")]
    Restore(String),
    /// `sendTransaction` refused the transaction outright.
    #[error("transaction rejected at send: {0}")]
    Rejected(String),
    /// Another signer of the same account got in first: the plan this
    /// transaction was built from is stale and must be rebuilt, never resent.
    #[error("the account's sequence number moved under this transaction")]
    BadSequence,
}

/// A transaction hash, rendered as 64 lowercase hex digits on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxHash(pub [u8; 32]);

impl TxHash {
    /// The wire form: 64 lowercase hex digits.
    #[must_use]
    pub fn to_hex(&self) -> String {
        use std::fmt::Write as _;
        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// Parses the wire form; any other length or character is a shape error.
    pub fn from_hex(text: &str) -> Result<Self, ChainError> {
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return Err(ChainError::Shape(format!(
                "transaction hash has {} characters, expected 64",
                bytes.len()
            )));
        }
        let mut out = [0_u8; 32];
        for (index, pair) in bytes.chunks(2).enumerate() {
            let digits = std::str::from_utf8(pair)
                .map_err(|_| ChainError::Shape("transaction hash is not ascii".to_string()))?;
            out[index] = u8::from_str_radix(digits, 16)
                .map_err(|_| ChainError::Shape(format!("transaction hash digit {digits:?}")))?;
        }
        Ok(Self(out))
    }
}

impl std::fmt::Display for TxHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_round_trips_through_hex() {
        let hash = TxHash([0xab; 32]);
        let hex = hash.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(&hex[..4], "abab");
        assert_eq!(TxHash::from_hex(&hex).expect("parses"), hash);
        assert_eq!(hash.to_string(), hex);
    }

    #[test]
    fn a_hash_of_the_wrong_length_or_alphabet_is_a_shape_error() {
        assert!(matches!(TxHash::from_hex("abc"), Err(ChainError::Shape(_))));
        let bad = "zz".repeat(32);
        assert!(matches!(TxHash::from_hex(&bad), Err(ChainError::Shape(_))));
    }
}
