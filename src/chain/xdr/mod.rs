//! ScVal and ledger-entry codecs for the Blend v2 pool contract.
//!
//! Hand-written rather than generated from the contract spec so that every
//! shape the bot depends on is visible here and pinned by a fixture test.
//! A shape mismatch after a contract upgrade fails a test, not a fill.

use crate::math::MathError;

pub mod decode;
pub mod encode;
pub mod events;
pub mod keys;

pub use encode::{
    address, from_base64, i128_val, invoke_contract_op, map, sc_address, simulation_envelope,
    stellar_asset, symbol, to_base64, vec,
};

/// Failures turning chain data into bot types, or bot types into chain data.
///
/// `PartialEq` (not `Eq`: `stellar_xdr::Error` carries an unequatable I/O
/// variant) so tests can assert a decode result directly against `Ok(...)`
/// or a specific error variant.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum XdrError {
    /// The XDR library rejected the bytes or the base64.
    #[error("xdr: {0}")]
    Xdr(#[from] stellar_xdr::Error),
    /// A string that is not a valid Stellar strkey for the expected kind.
    #[error("invalid address: {0}")]
    Address(String),
    /// A symbol longer than 32 bytes or with characters Soroban rejects.
    #[error("invalid symbol: {0}")]
    Symbol(String),
    /// The value decoded, but is not the shape this contract type has.
    #[error("unexpected value: expected {expected}, got {got}")]
    Shape {
        /// What the decoder was looking for.
        expected: &'static str,
        /// A debug rendering of what it found.
        got: String,
    },
    /// A struct map lacks a field the contract type always has.
    #[error("missing field {0}")]
    MissingField(&'static str),
    /// Decoded numbers could not be combined (e.g. `10^decimals` overflow).
    #[error("math: {0}")]
    Math(#[from] MathError),
}
