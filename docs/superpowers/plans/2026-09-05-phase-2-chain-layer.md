# Phase 2: Chain Layer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the bot a Soroban RPC client, pool reads that turn one ledger's entries and view calls into `math` types, a signing key, and one transaction path (build, simulate, restore, assemble, fee, sign, send, poll, classify), every step of which is driven through a scripted local JSON-RPC server in tests.

**Architecture:** Four new files under `src/chain/`. `rpc.rs` is the JSON-RPC transport and the typed wire shapes of the eight methods the bot uses; it decodes base64 XDR at the boundary and returns `stellar_xdr` types. `signer.rs` holds the network id and the Ed25519 key and signs a `Transaction` into an envelope. `tx.rs` is the write path from section 3 of the spec, returning a classified outcome. `pool.rs` is the read path: a batched, single-ledger `PoolSnapshot` a caller can value with `math`, plus the auction and balance reads and the three operation builders. A `cfg(test)` `script.rs` runs a scripted RPC server on localhost so the real client is exercised end to end without a network. Configuration gains the network and RPC knobs, and an example prints a live pool snapshot as the phase's dry-run demonstration.

**Tech Stack:** Rust 1.97 (pinned three ways), `reqwest` 0.12 (`json`, `rustls-tls-native-roots`; no default features), `serde` + `serde_json`, `ed25519-dalek` 2.2, `stellar-strkey` 0.0.13, `sha2` 0.10, `stellar-xdr` 28.0.0 as before, `tokio`; dev: `wiremock` 0.6.

**Spec:** `docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md`, sections 2 (module map), 3 (chain access: reads, writes), 6 (configuration), 8 (error handling: queues), 9 (testing: `chain/tx.rs` scripted server), 12 (delivery, phase 2). This plan argues from the spec; where it departs, the departure is a ruling recorded below.

## Global Constraints

- The three-way Rust pin stays at 1.97.0 (`Cargo.toml` `rust-version`, `rust-toolchain.toml`, Dockerfile `FROM rust:1.97.0-bookworm`); do not touch any of the three.
- `clippy::pedantic` is warn-level and CI runs `cargo clippy --all-targets -- -D warnings`, so every pedantic finding is an error in every target including tests and examples. `unwrap_used` is denied outside tests; `expect_used` warns, which is also an error in CI outside tests (`clippy.toml` exempts tests only — `#[cfg(test)]` modules and files count as tests, `examples/` does not).
- Numeric literals use 3-digit `_` grouping.
- Money is `i128` in each asset's own decimals; fees are `u32` stroops as the transaction carries them; resource fees are `i64` as `SorobanTransactionData` carries them. No `f64`.
- No `as` numeric casts: use `u32::try_from`, `i64::from`, `u64::from`.
- Arithmetic on chain-sourced values is checked or proven safe in a comment; never a silent saturation.
- Doc comments state constraints and invariants, not narration.
- `tracing` in the crate, `println!` only in `examples/`.
- Secrets never touch argv: `FILLER_SECRET_KEY` and `RPC_API_KEY` are read from the environment by name, never declared as clap arguments, and never appear in a `Debug` rendering or a log line.
- `cargo deny check` must pass; the dependency set and feature flags in Task 1 are the ones verified to pass it and are not to be widened.
- `make check` (fmt, clippy, tests, docs with `-D warnings`, invariants script, shellcheck) must be green at the end of every task before committing.
- Commit messages follow the repository's conventional style (`feat:`, `test:`, `docs:`, `chore:`) and end with the trailer `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.
- Work on branch `phase-2/chain-layer`; open one pull request for the whole phase at the end.
- **The dependency tree is now heavy.** The first build after Task 1 compiles `reqwest`, `rustls`, `hyper` and `wiremock`. In the dev container cargo can be OOM-killed (`signal: 9`, `collect2: ... ld terminated with signal 9`); that is not a code failure. Rerun with `CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 make check`.

## Rulings

Decisions this plan makes where the spec is silent or where following its letter is not possible, each with its reason. An implementer follows them; a reviewer checks them against the spec.

1. **The JSON-RPC client is hand-written over `reqwest`, not the `stellar-rpc-client` crate the spec names.** Checked on 2026-09-05: `stellar-rpc-client` 27.0.0 requires `stellar-xdr` ^27 (this crate is on 28.0.0, the version the mainnet RPC speaks), and 28.0.0-rc.1 is a release candidate whose `jsonrpsee` transport fails `cargo deny` under this repository's licence allow-list. The bot uses eight methods with flat JSON shapes; writing them is less code than the crate's adapters would be, and every wire shape stays visible in one file, the same argument the spec makes for hand-written XDR codecs.
2. **`reqwest` 0.12 with `default-features = false, features = ["json", "rustls-tls-native-roots"]`.** This is the feature set that resolved to `rustls` + `ring` + `rustls-native-certs` and passed `cargo deny` (advisories, bans, licences, sources all ok). `rustls-tls` would pull `webpki-roots` (CDLA-Permissive-2.0, not in `deny.toml`); `reqwest` 0.13's `rustls` feature routes through `aws-lc-rs` (an OpenSSL-licensed component, also not allowed). The runtime image already installs `ca-certificates`, which is what native roots read.
3. **`stellar-strkey` is pinned to `0.0.13`**, the version `stellar-xdr` 28 already depends on, so the tree gains no second copy. It is used for one thing: decoding an `S…` secret. Public keys parse through `stellar_xdr`'s own `FromStr` impls.
4. **A fifth file, `src/chain/signer.rs`.** The spec's module map folds signing into `chain/tx.rs`; a key-holding type with a redacted `Debug` deserves its own small file, and `tx.rs` is already the longest pipeline in the phase. CLAUDE.md's module map is updated accordingly.
5. **`src/chain/script.rs` is the spec's "scripted localhost JSON-RPC server"**, built on `wiremock` as a dev-dependency and compiled only under `cfg(test)`. It answers each method from a queue of canned results and records every request, so a test can assert both what the client sent and how it handled what came back.
6. **`RPC_API_KEY` is read from the environment by name**, never as a clap argument: the spec calls it a secret, and CLAUDE.md's rule is that secrets never reach argv. The header *name* is an ordinary knob.
7. **Network and RPC knobs are optional at the clap level and required by `Args::chain()`.** The binary is still a skeleton whose only behaviour is to parse and exit; making `RPC_URL` required at parse time would break that and the existing tests. Phase 3's service calls `chain()` and fails at startup exactly as the spec's "required" demands.
8. **The ledger bound is the poll window.** Every transaction carries `ledger_bounds.max_ledger = latest_ledger + TX_POLL_LEDGERS + 1` (exclusive per CAP-21: the transaction cannot be applied in `max_ledger` or later) and a time bound of now + 300 s. `wait` therefore reaches a decision inside the window: a transaction the RPC has not seen once `latestLedger >= max_ledger` is provably never going to land, and is classified `Expired` — the "proves it can never be included" outcome section 8 wants — rather than `Unknown`. `Unknown` is reserved for the case where the RPC could not answer for the whole window.
9. **One error type for the module, `chain::ChainError`.** Transport, HTTP, JSON-RPC, shape, XDR, math, key, simulation, send and sequence failures are variants of one enum so the pipeline propagates with `?` and callers match on the variants that change their behaviour (`BadSequence`, `Simulation { contract_error }`). It is not `PartialEq` (`reqwest::Error` is not); tests use `matches!`.
10. **Pool reads require every batch and every simulation to report the same `latestLedger`**, else `ChainError::LedgerMoved`. The spec's invariant is that a snapshot describes one ledger; a caller retries a moved ledger, it never averages two.

---

## File Structure

| Path | Responsibility |
|---|---|
| `Cargo.toml` (modify) | add `reqwest`, `serde`, `serde_json` (promoted from dev), `ed25519-dalek`, `stellar-strkey`, `sha2`; dev-dependency `wiremock` |
| `src/liquidator.rs` (modify) | `LiquidatorError::Chain(#[from] chain::ChainError)` |
| `src/config.rs` (modify) | network and RPC knobs on `Args`; `NetworkName`; `ChainConfig` and `Args::chain()` / `Args::chain_with_secret()` |
| `src/chain/mod.rs` (modify) | `ChainError`, `TxHash`, module declarations, re-exports |
| `src/chain/script.rs` (create, `cfg(test)`) | scripted JSON-RPC server on localhost and canned-XDR builders shared by the chain tests |
| `src/chain/rpc.rs` (create) | `RpcClient`, the eight methods, their wire shapes and decoded result types, contract-error extraction from diagnostic events |
| `src/chain/signer.rs` (create) | `Network` (passphrase, id), `Signer` (Ed25519 key, account, redacted `Debug`, `sign`) |
| `src/chain/tx.rs` (create) | `TxConfig`, `Priority`, `Prepared`, `TxOutcome`, `Submitter` (prepare with restore, send with one retry, wait and classify, submit) |
| `src/chain/xdr/encode.rs` (modify) | `RequestType`, `Request`, `request()` |
| `src/chain/xdr/mod.rs` (modify) | re-export `RequestType`, `Request` |
| `src/chain/pool.rs` (create) | `PoolReader` (snapshot, auction, balance), `PoolSnapshot::position_data`, `submit_op`, `new_auction_op`, `bad_debt_op` |
| `examples/pool_snapshot.rs` (create) | prints a live pool's reserves and users' health factors; the phase's dry-run demonstration |
| `.env.example` (modify) | the new knobs |
| `CLAUDE.md`, `CHANGELOG.md` (modify) | module map, gotchas, unreleased entries |

## Wire facts the code is written against

Captured from `https://mainnet.sorobanrpc.com` (RPC 27.1.1, protocol 27) on 2026-09-05; the scripted server in tests reproduces exactly these shapes.

- `getHealth` → `{"status":"healthy","latestLedger":64289467,"latestLedgerCloseTime":"1788635204","oldestLedger":64168508,"oldestLedgerCloseTime":"1787949229","ledgerRetentionWindow":120960}`. Close times are **strings**.
- `getLatestLedger` → `{"id":"70f3…","protocolVersion":27,"sequence":64289467,"closeTime":"1788635204", "headerXdr":…, "metadataXdr":…}` (the two XDR blobs are large and ignored).
- `getLedgerEntries {"keys":[base64 LedgerKey…]}` → `{"latestLedger":64289527,"entries":[{"key":…,"xdr":…,"lastModifiedLedgerSeq":61962028,"liveUntilLedgerSeq":64814477,"extXdr":"AAAAAA=="}]}`. **Absent keys are omitted**, not nulled: asking for two keys of which one exists returns one entry. An unparseable key is a JSON-RPC error `-32602`. The RPC accepts at most 200 keys per call.
- `simulateTransaction {"transaction": base64 envelope}` success → `{"transactionData":…,"events":[base64 DiagnosticEvent…],"minResourceFee":"446953","results":[{"auth":[base64 SorobanAuthorizationEntry…],"xdr":base64 ScVal}],"latestLedger":64289527}`; with archived entries the same plus `"restorePreamble":{"minResourceFee":"…","transactionData":…}`. Failure → `{"error":"HostError: Error(Contract, #1200)\n\nEvent log (newest first):\n …","events":[…],"latestLedger":64289527}` — the contract code is in the message **and** in a diagnostic event whose topics are `[Symbol("error"), Error(Contract, 1200)]`. An unparseable envelope → `{"error":"Could not unmarshal transaction","latestLedger":0}`.
- `sendTransaction {"transaction": …}` → `{"status":"PENDING"|"DUPLICATE"|"TRY_AGAIN_LATER"|"ERROR","hash":hex,"latestLedger":…,"latestLedgerCloseTime":"…","errorResultXdr":base64 TransactionResult (ERROR only),"diagnosticEventsXdr":[…] (ERROR only)}`. An unparseable envelope is a JSON-RPC error `-32602 invalid_xdr`.
- `getTransaction {"hash": hex}` → `{"status":"NOT_FOUND"|"SUCCESS"|"FAILED","latestLedger":…,"latestLedgerCloseTime":"…","oldestLedger":…,"oldestLedgerCloseTime":"…","txHash":hex,"applicationOrder":…,"feeBump":bool,"envelopeXdr":…,"resultXdr":base64 TransactionResult,"resultMetaXdr":base64 TransactionMeta,"diagnosticEventsXdr":[…],"events":{…},"ledger":64286516,"createdAt":"1788618724"}`; NOT_FOUND carries `"ledger":0,"createdAt":"0"` and no XDR fields.
- `getFeeStats` → `{"sorobanInclusionFee":{"max":"200","min":"100","mode":"200","p10":"200",…,"p70":"200",…,"p90":"200",…,"transactionCount":"6851","ledgerCount":50},"inclusionFee":{…},"latestLedger":64289468}`. Percentiles are strings of stroops.
- `getEvents {"startLedger":…,"filters":[{"type":"contract","contractIds":[…],"topics":[[…]]}],"pagination":{"limit":…}}` (or `"pagination":{"cursor":…,"limit":…}` without `startLedger`) → `{"latestLedger":…,"latestLedgerCloseTime":"…","oldestLedger":…,"cursor":"0276108303406649344-0000000000","events":[{"type":"contract","ledger":64286474,"ledgerClosedAt":"2026-09-05T14:28:07Z","contractId":"C…","id":"0276108303406649344-0000000000","operationIndex":0,"transactionIndex":365,"txHash":hex,"inSuccessfulContractCall":true,"topic":[base64 ScVal…],"value":base64 ScVal}]}`. A `startLedger` outside the retained window is JSON-RPC error `-32600 "startLedger must be within the ledger range: A - B"`.
- Account sequence: `getLedgerEntries` with `LedgerKey::Account`; the entry's `xdr` decodes to `LedgerEntryData::Account(AccountEntry { seq_num, .. })`.

XDR facts (stellar-xdr 28.0.0): `Transaction { source_account: MuxedAccount, fee: u32, seq_num: SequenceNumber(i64), cond: Preconditions, memo: Memo, operations: VecM<Operation, 100>, ext: TransactionExt }`; `Preconditions::V2(PreconditionsV2 { time_bounds: Option<TimeBounds>, ledger_bounds: Option<LedgerBounds { min_ledger, max_ledger }>, min_seq_num: Option<SequenceNumber>, min_seq_age: Duration(u64), min_seq_ledger_gap: u32, extra_signers: VecM<SignerKey, 2> })`; `TimeBounds { min_time: TimePoint(u64), max_time: TimePoint }`; `TransactionExt::V1(SorobanTransactionData { ext: SorobanTransactionDataExt, resources: SorobanResources, resource_fee: i64 })`; `Transaction::hash(network_id: [u8; 32]) -> Result<[u8; 32], stellar_xdr::Error>` computes the signature payload hash; `TransactionEnvelope::Tx(TransactionV1Envelope { tx, signatures: VecM<DecoratedSignature, 20> })`; `DecoratedSignature { hint: SignatureHint([u8; 4]), signature: Signature(BytesM<64>) }`; `OperationBody::RestoreFootprint(RestoreFootprintOp { ext: ExtensionPoint::V0 })`; `InvokeHostFunctionOp { host_function, auth: VecM<SorobanAuthorizationEntry> }`; `TransactionResult { fee_charged: i64, result: TransactionResultResult, ext: TransactionResultExt::V0 }` with `TransactionResultResult::TxBadSeq` among the variants; `TransactionMeta::V4(TransactionMetaV4 { ext, tx_changes_before, operations, tx_changes_after, soroban_meta: Option<SorobanTransactionMetaV2 { ext, return_value: Option<ScVal> }>, events, diagnostic_events: VecM<DiagnosticEvent> })` and `V3(TransactionMetaV3 { …, soroban_meta: Option<SorobanTransactionMeta { events, return_value: ScVal, diagnostic_events, .. }> })`; `DiagnosticEvent { in_successful_contract_call: bool, event: ContractEvent { ext, contract_id: Option<ContractId>, type_: ContractEventType, body: ContractEventBody::V0(ContractEventV0 { topics: VecM<ScVal>, data: ScVal }) } }`; `ScVal::Error(ScError::Contract(u32))`; `AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([u8; 32])))` implements `FromStr` and `Display` for `G…` strkeys; `MuxedAccount::Ed25519(Uint256)`; `LedgerKey::Account(LedgerKeyAccount { account_id })`.

---

### Task 1: Dependencies, error type, configuration knobs

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/liquidator.rs`
- Modify: `src/chain/mod.rs`
- Modify: `src/config.rs`
- Modify: `.env.example`

**Interfaces:**
- Consumes: `chain::xdr::XdrError`, `math::MathError` (Phase 1).
- Produces:
  - `pub enum chain::ChainError` (variants below) and `pub struct chain::TxHash([u8; 32])` with `to_hex()`, `from_hex()`, `Display`.
  - `pub enum config::NetworkName { Mainnet, Testnet }` with `passphrase()`.
  - `pub struct config::Secret(String)` (redacting `Debug`, `Secret::new`, `expose(&self) -> &str`) and `pub struct config::ChainConfig { pub network_passphrase: String, pub rpc_url: String, pub rpc_api_key: Option<(String, Secret)>, pub base_fee: u32, pub high_fee: u32, pub tx_poll_ledgers: u32 }`.
  - `Args::chain(&self) -> Result<ChainConfig, LiquidatorError>` (reads `RPC_API_KEY` from the environment) and `Args::chain_with_secret(&self, rpc_api_key: Option<String>) -> Result<ChainConfig, LiquidatorError>`.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml`, replace the `[dependencies]` and `[dev-dependencies]` tables with:

```toml
[dependencies]
clap = { version = "4.6.0", features = ["derive", "env"] }
# Ed25519 signing of transaction hashes.
ed25519-dalek = "2.2"
# 256-bit integers for the fixed-point widening path. Already in the tree
# through stellar-xdr, so this adds no new code to audit.
ethnum = "1.5"
# JSON-RPC over HTTPS to the Soroban RPC. rustls with the system root store:
# the runtime image ships ca-certificates, and this is the feature set whose
# licence tree cargo-deny accepts — `rustls-tls` would pull webpki-roots
# (CDLA-Permissive-2.0) and 0.13's `rustls` routes through aws-lc-rs
# (OpenSSL licence), neither of which deny.toml allows.
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls-native-roots"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
# Network id is sha256(passphrase). Already in the tree through stellar-xdr.
sha2 = "0.10"
# Decodes an S… secret. The version stellar-xdr already depends on, so no
# second copy; public keys parse through stellar-xdr's own FromStr impls.
stellar-strkey = "0.0.13"
# XDR types plus base64 for RPC payloads. No `soroban-sdk`: the handful of
# pool types the bot touches are encoded and decoded by hand in
# `chain::xdr`, which keeps every ScVal shape visible in one place.
stellar-xdr = { version = "28.0.0", features = ["base64"] }
thiserror = "2.0.18"
tokio = { version = "1.51.0", features = ["full"] }
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter", "json"] }

[dev-dependencies]
# The scripted JSON-RPC server the chain tests drive the real client through.
wiremock = "0.6"
```

Run: `cargo generate-lockfile` is not needed; `cargo fetch` then `cargo deny check`. Expected: `advisories ok, bans ok, licenses ok, sources ok`. If licences fail, the feature set was changed; restore it.

- [ ] **Step 2: Add `ChainError` and `TxHash` to `src/chain/mod.rs`**

Replace the file with:

```rust
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

pub mod xdr;

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
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
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
        assert_eq!(TxHash::from_hex(&hex), Ok(hash));
        assert_eq!(hash.to_string(), hex);
    }

    #[test]
    fn a_hash_of_the_wrong_length_or_alphabet_is_a_shape_error() {
        assert!(matches!(TxHash::from_hex("abc"), Err(ChainError::Shape(_))));
        let bad = "zz".repeat(32);
        assert!(matches!(TxHash::from_hex(&bad), Err(ChainError::Shape(_))));
    }
}
```

`assert_eq!(TxHash::from_hex(&hex), Ok(hash))` needs `PartialEq` on the `Result`, which `ChainError` lacks; write that line as `assert_eq!(TxHash::from_hex(&hex).expect("parses"), hash);` instead.

- [ ] **Step 3: Add the `Chain` phase to `LiquidatorError`**

In `src/liquidator.rs`, extend the enum:

```rust
#[derive(Debug, thiserror::Error)]
pub enum LiquidatorError {
    /// Configuration was rejected at startup, before anything could act on it.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// The chain layer failed: transport, RPC, decoding, signing or submission.
    #[error("chain: {0}")]
    Chain(#[from] chain::ChainError),
}
```

and update the enum's doc comment: it now has two variants, one per phase that exists; the sentence about "one variant today" goes.

- [ ] **Step 4: Write the failing configuration tests**

Append to `src/config.rs`'s test module:

```rust
    fn parse(argv: &[&str]) -> Args {
        Args::try_parse_from(argv).unwrap()
    }

    #[test]
    fn a_network_name_resolves_to_its_passphrase() {
        let args = parse(&["liquidator", "--network", "testnet", "--rpc-url", "http://rpc"]);
        let chain = args.chain_with_secret(None).unwrap();
        assert_eq!(chain.network_passphrase, "Test SDF Network ; September 2015");
        assert_eq!(chain.rpc_url, "http://rpc");
        assert_eq!(chain.rpc_api_key, None);
        assert_eq!((chain.base_fee, chain.high_fee, chain.tx_poll_ledgers), (5_000, 10_000, 3));
    }

    #[test]
    fn an_explicit_passphrase_wins_over_nothing_and_conflicts_with_a_name() {
        let args = parse(&["liquidator", "--network-passphrase", "Custom ; 2026", "--rpc-url", "http://rpc"]);
        assert_eq!(args.chain_with_secret(None).unwrap().network_passphrase, "Custom ; 2026");
        assert!(Args::try_parse_from([
            "liquidator", "--network", "mainnet", "--network-passphrase", "x", "--rpc-url", "http://rpc",
        ])
        .is_err());
    }

    #[test]
    fn the_network_and_rpc_url_are_required_by_chain_not_by_parsing() {
        let args = parse(&["liquidator"]);
        assert!(matches!(args.chain_with_secret(None), Err(LiquidatorError::Config(_))));
        let args = parse(&["liquidator", "--network", "mainnet"]);
        assert!(matches!(args.chain_with_secret(None), Err(LiquidatorError::Config(_))));
    }

    #[test]
    fn the_api_key_header_and_secret_come_together_or_not_at_all() {
        let base = ["liquidator", "--network", "mainnet", "--rpc-url", "http://rpc"];
        let with_header = parse(&[&base[..], &["--rpc-api-key-header", "X-Api-Key"]].concat());
        assert!(matches!(with_header.chain_with_secret(None), Err(LiquidatorError::Config(_))));
        assert_eq!(
            with_header.chain_with_secret(Some("k".to_string())).unwrap().rpc_api_key,
            Some(("X-Api-Key".to_string(), Secret::new("k")))
        );
        let without = parse(&base);
        assert!(matches!(without.chain_with_secret(Some("k".to_string())), Err(LiquidatorError::Config(_))));
    }

    /// The key is a secret: it must never be a clap argument, or it would
    /// be readable from `/proc/<pid>/cmdline`.
    #[test]
    fn the_api_key_is_not_a_command_line_argument() {
        assert!(Args::try_parse_from(["liquidator", "--rpc-api-key", "k"]).is_err());
    }

    /// Nor may it reach a log line through `Debug`.
    #[test]
    fn the_api_key_never_appears_in_a_debug_rendering() {
        let args = parse(&["liquidator", "--network", "mainnet", "--rpc-url", "http://rpc", "--rpc-api-key-header", "X-Api-Key"]);
        let config = args.chain_with_secret(Some("secret-123".to_string())).unwrap();
        let rendered = format!("{config:?}");
        assert!(rendered.contains("X-Api-Key"));
        assert!(rendered.contains("Secret(<redacted>)"));
        assert!(!rendered.contains("secret-123"));
        assert_eq!(Secret::new("secret-123").expose(), "secret-123");
    }
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test --lib config`
Expected: compile errors — `chain_with_secret`, `--network`, `--rpc-url` do not exist.

- [ ] **Step 6: Implement the knobs and `ChainConfig`**

In `src/config.rs` add, after `LogFormat`:

```rust
/// A named network, standing in for its passphrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum NetworkName {
    /// Public Global Stellar Network.
    Mainnet,
    /// Test SDF Network.
    Testnet,
}

impl NetworkName {
    /// The passphrase the network signs with.
    #[must_use]
    pub fn passphrase(self) -> &'static str {
        match self {
            Self::Mainnet => "Public Global Stellar Network ; September 2015",
            Self::Testnet => "Test SDF Network ; September 2015",
        }
    }
}

/// A secret configuration value. Renders as `Secret(<redacted>)` so it can
/// never reach a log line through `Debug`; the text is available only
/// through `expose`, which every caller must name deliberately.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps the secret text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The secret text, for the one place that puts it on the wire.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Everything the chain layer needs, validated. Built by [`Args::chain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainConfig {
    /// The network passphrase transactions are hashed with.
    pub network_passphrase: String,
    /// The Soroban RPC endpoint.
    pub rpc_url: String,
    /// Header name and secret value for a keyed RPC provider, both or
    /// neither. The value is a `Secret`: it never renders.
    pub rpc_api_key: Option<(String, Secret)>,
    /// Inclusion-fee floor for a normal-priority transaction, in stroops.
    pub base_fee: u32,
    /// Inclusion-fee floor for a high-priority transaction, in stroops.
    pub high_fee: u32,
    /// How many ledgers a submitted transaction stays valid and is polled for.
    pub tx_poll_ledgers: u32,
}
```

Add these fields to `Args`, after `log_format`:

```rust
    /// Network passphrase. Give this or `--network`, not both.
    #[arg(long, env = "NETWORK_PASSPHRASE", conflicts_with = "network")]
    pub network_passphrase: Option<String>,

    /// Named network, an alias for its passphrase.
    #[arg(long, env = "NETWORK", value_enum)]
    pub network: Option<NetworkName>,

    /// Soroban RPC URL.
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: Option<String>,

    /// Header that carries the RPC API key. The key itself is `RPC_API_KEY`
    /// in the environment only — never an argument.
    #[arg(long, env = "RPC_API_KEY_HEADER")]
    pub rpc_api_key_header: Option<String>,

    /// Inclusion-fee floor for normal-priority transactions, in stroops.
    #[arg(long, env = "BASE_FEE", default_value_t = 5_000)]
    pub base_fee: u32,

    /// Inclusion-fee floor for high-priority transactions, in stroops.
    #[arg(long, env = "HIGH_FEE", default_value_t = 10_000)]
    pub high_fee: u32,

    /// Ledgers a submitted transaction stays valid and is polled for.
    #[arg(long, env = "TX_POLL_LEDGERS", default_value_t = 3)]
    pub tx_poll_ledgers: u32,
```

and the methods:

```rust
impl Args {
    /// The chain configuration, reading `RPC_API_KEY` from the environment.
    pub fn chain(&self) -> Result<ChainConfig, LiquidatorError> {
        self.chain_with_secret(std::env::var("RPC_API_KEY").ok())
    }

    /// The chain configuration with the API key supplied by the caller —
    /// what `chain` does after reading the environment, separated so tests
    /// never touch process-global state.
    pub fn chain_with_secret(
        &self,
        rpc_api_key: Option<String>,
    ) -> Result<ChainConfig, LiquidatorError> {
        let network_passphrase = match (&self.network_passphrase, self.network) {
            (Some(passphrase), _) => passphrase.clone(),
            (None, Some(name)) => name.passphrase().to_string(),
            (None, None) => {
                return Err(LiquidatorError::Config(
                    "NETWORK_PASSPHRASE or NETWORK is required".to_string(),
                ))
            }
        };
        let rpc_url = self
            .rpc_url
            .clone()
            .ok_or_else(|| LiquidatorError::Config("RPC_URL is required".to_string()))?;
        let rpc_api_key = match (&self.rpc_api_key_header, rpc_api_key) {
            (Some(header), Some(key)) => Some((header.clone(), Secret::new(key))),
            (None, None) => None,
            (Some(_), None) => {
                return Err(LiquidatorError::Config(
                    "RPC_API_KEY_HEADER is set but RPC_API_KEY is not".to_string(),
                ))
            }
            (None, Some(_)) => {
                return Err(LiquidatorError::Config(
                    "RPC_API_KEY is set but RPC_API_KEY_HEADER is not".to_string(),
                ))
            }
        };
        Ok(ChainConfig {
            network_passphrase,
            rpc_url,
            rpc_api_key,
            base_fee: self.base_fee,
            high_fee: self.high_fee,
            tx_poll_ledgers: self.tx_poll_ledgers,
        })
    }
}
```

with `use crate::LiquidatorError;` at the top. The existing `dry_run` tests keep passing because nothing became required at parse time.

- [ ] **Step 7: Document the knobs in `.env.example`**

Append:

```bash
# ============================================
# CHAIN
# ============================================

# Which network to sign for: `mainnet` or `testnet`, or give the passphrase
# itself with NETWORK_PASSPHRASE for any other network. One or the other.
NETWORK=testnet
# NETWORK_PASSPHRASE="Test SDF Network ; September 2015"

# Soroban RPC endpoint.
RPC_URL=https://soroban-testnet.stellar.org

# For a keyed RPC provider: the header name is an ordinary setting, the key
# is a secret and is read from the environment only — it is never a
# command-line argument, because argv is world-readable.
# RPC_API_KEY_HEADER=X-Api-Key
# RPC_API_KEY=

# Inclusion-fee floors in stroops. The fee-stats p70 (normal) or p90 (high
# priority) percentile is used when it is higher.
BASE_FEE=5000
HIGH_FEE=10000

# Ledgers a submitted transaction stays valid and is polled for. The
# transaction's ledger bound is derived from this, so after that many
# ledgers a transaction the RPC has not seen can never land.
TX_POLL_LEDGERS=3
```

- [ ] **Step 8: Run the checks**

Run: `make check` (with the memory cap from Global Constraints if the first build is killed).
Expected: all green, config tests included.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock src/liquidator.rs src/chain/mod.rs src/config.rs .env.example
git commit -m "feat(chain): dependencies, ChainError and the network and RPC knobs" -m "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: The scripted RPC server and the client core (`getHealth`, `getLatestLedger`)

**Files:**
- Create: `src/chain/script.rs` (`cfg(test)`)
- Create: `src/chain/rpc.rs`
- Modify: `src/chain/mod.rs` (declare `pub mod rpc;` and `#[cfg(test)] pub(crate) mod script;`)

**Interfaces:**
- Consumes: `ChainError`, `TxHash` (Task 1); `chain::xdr::{to_base64, from_base64, XdrError}` (Phase 1).
- Produces:
  - test-only `ScriptedRpc::start().await`, `url()`, `expect(method, result: Value)`, `expect_error(method, code, message)`, `expect_http(method, status)`, `calls(method) -> Vec<Value>` (the `params` of each call, in order), `remaining() -> usize`, `received().await -> Vec<wiremock::Request>`.
  - test-only canned-XDR builders: `account_entry_b64(account: &str, sequence: i64)`, `scval_b64(&ScVal)`, `transaction_data_b64(resource_fee: i64)`, `diagnostic_error(code: u32) -> DiagnosticEvent`, `diagnostic_error_b64(code)`, `result_b64(TransactionResultResult)`, `meta_v4_b64(return_value: Option<ScVal>, diagnostics: Vec<DiagnosticEvent>)`.
  - `pub struct RpcClient` with `RpcClient::new(url: &str, api_key: Option<(&str, &str)>) -> Result<Self, ChainError>`, `RpcClient::from_config(&ChainConfig) -> Result<Self, ChainError>`.
  - `pub struct Health { pub status: String, pub latest_ledger: u32, pub latest_ledger_close_time: u64, pub oldest_ledger: u32, pub ledger_retention_window: u32 }` and `pub async fn health(&self) -> Result<Health, ChainError>`.
  - `pub struct LatestLedger { pub sequence: u32, pub protocol_version: u32, pub close_time: u64 }` and `pub async fn latest_ledger(&self) -> Result<LatestLedger, ChainError>`.
  - private `async fn call<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: P) -> Result<R, ChainError>` that every later method uses.

- [ ] **Step 1: Write the scripted server**

Create `src/chain/script.rs`:

```rust
//! A scripted JSON-RPC server for the chain tests.
//!
//! Every method has a queue of canned answers, consumed in order, and every
//! request is recorded, so a test asserts both what the client sent and how
//! it handled what came back. An unscripted call answers HTTP 500 with the
//! method name in the body: a test that forgot to script a method fails
//! loudly instead of hanging. The canned-XDR builders below produce the
//! base64 the RPC would put in its responses.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use stellar_xdr::{
    AccountEntry, AccountEntryExt, AccountId, ContractEvent, ContractEventBody,
    ContractEventType, ContractEventV0, DiagnosticEvent, ExtensionPoint, LedgerEntryChanges,
    LedgerEntryData, LedgerFootprint, ScError, ScVal, SorobanResources, SorobanTransactionData,
    SorobanTransactionDataExt, SorobanTransactionMetaExt, SorobanTransactionMetaV2, String32,
    StringM, Thresholds, TransactionMeta, TransactionMetaV4, TransactionResult,
    TransactionResultExt, TransactionResultResult, VecM,
};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::chain::xdr::encode::{symbol, to_base64};

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
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let method = body["method"].as_str().unwrap_or_default().to_string();
        let id = body["id"].clone();
        let mut state = self.state.lock().expect("script mutex");
        state.calls.push((method.clone(), body["params"].clone()));
        match state.script.get_mut(&method).and_then(VecDeque::pop_front) {
            Some(Canned::Result(result)) => ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": result})),
            Some(Canned::Error { code, message }) => ResponseTemplate::new(200).set_body_json(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
            ),
            Some(Canned::Http(status)) => ResponseTemplate::new(status),
            None => ResponseTemplate::new(500).set_body_string(format!("unscripted method {method}")),
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
pub(crate) fn meta_v4_b64(return_value: Option<ScVal>, diagnostics: Vec<DiagnosticEvent>) -> String {
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
```

If `TransactionMetaV4`'s field list differs from the XDR facts above, follow the compiler: the shape is what `stellar-xdr` 28.0.0 defines.

- [ ] **Step 2: Write the failing client tests**

Create `src/chain/rpc.rs` with the module doc, the items below as `todo!()` stubs where a body is needed, and this test module (stubs are acceptable for one step only; Step 4 replaces them):

```rust
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
        assert_eq!(requests[0].headers.get("content-type").unwrap(), "application/json");
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
        assert!(matches!(client.health().await.unwrap_err(), ChainError::Http(503)));
        assert!(matches!(client.latest_ledger().await.unwrap_err(), ChainError::Http(500)));
    }

    #[tokio::test]
    async fn a_result_of_the_wrong_shape_is_a_shape_error_not_a_panic() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", json!({"status": "healthy"}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        assert!(matches!(client.health().await.unwrap_err(), ChainError::Shape(_)));
    }

    #[test]
    fn a_bad_header_name_is_a_config_error() {
        assert!(matches!(
            RpcClient::new("http://localhost", Some(("bad header", "v"))),
            Err(ChainError::Config(_))
        ));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib chain::rpc`
Expected: FAIL (the stubs `todo!()` panic, or the module does not compile until the stubs exist).

- [ ] **Step 4: Implement the client core**

`src/chain/rpc.rs`, above the tests:

```rust
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
                let name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| ChainError::Config("RPC_API_KEY_HEADER is not a valid header name"))?;
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
```

In `src/chain/mod.rs` add `pub mod rpc;` and `#[cfg(test)] pub(crate) mod script;`, and re-export `pub use rpc::RpcClient;`.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib chain::rpc`
Expected: 8 passed.

- [ ] **Step 6: `make check`, then commit**

```bash
git add src/chain/mod.rs src/chain/rpc.rs src/chain/script.rs
git commit -m "feat(chain): JSON-RPC client core with a scripted server for tests" -m "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: Ledger entries, account sequence, fee stats, events

**Files:**
- Modify: `src/chain/rpc.rs`

**Interfaces:**
- Consumes: `RpcClient::call` (Task 2); `chain::xdr::{to_base64, from_base64}`; `TxHash`.
- Produces:
  - `pub struct LedgerEntry { pub data: LedgerEntryData, pub last_modified_ledger: u32, pub live_until_ledger: Option<u32> }`.
  - `pub struct LedgerEntries { pub latest_ledger: u32, .. }` with `get(&self, key: &LedgerKey) -> Result<Option<&LedgerEntry>, ChainError>` and `len()`.
  - `pub async fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<LedgerEntries, ChainError>` — batches of 200, all batches must agree on `latestLedger`.
  - `pub struct Account { pub sequence: i64, pub latest_ledger: u32 }` and `pub async fn account(&self, account: &str) -> Result<Account, ChainError>` (`NoAccount` when absent).
  - `pub struct FeeStats { pub soroban_percentile_70: u32, pub soroban_percentile_90: u32, pub latest_ledger: u32 }` and `pub async fn fee_stats(&self) -> Result<FeeStats, ChainError>`.
  - `pub struct EventQuery<'a> { pub start_ledger: Option<u32>, pub cursor: Option<&'a str>, pub contract_ids: &'a [&'a str], pub limit: u32 }`, `pub struct Event { pub ledger: u32, pub id: String, pub tx_hash: TxHash, pub contract_id: String, pub in_successful_contract_call: bool, pub topics: Vec<ScVal>, pub value: ScVal }`, `pub struct Events { pub latest_ledger: u32, pub cursor: Option<String>, pub events: Vec<Event> }`, and `pub async fn events(&self, query: &EventQuery<'_>) -> Result<Events, ChainError>`.

- [ ] **Step 1: Write the failing tests**

Append to the test module in `src/chain/rpc.rs`:

```rust
    use crate::chain::script::account_entry_b64;
    use crate::chain::xdr::{encode, keys};
    use stellar_xdr::{LedgerEntryData, LedgerKey, ScVal};

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
        let entries = client.ledger_entries(&[instance.clone(), positions.clone()]).await.unwrap();
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
        rpc.expect("getLedgerEntries", json!({"latestLedger": 10, "entries": []}));
        rpc.expect("getLedgerEntries", json!({"latestLedger": 11, "entries": []}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let error = client.ledger_entries(&keys).await.unwrap_err();
        assert!(matches!(error, ChainError::LedgerMoved { first: 10, second: 11 }), "{error:?}");
        let params = rpc.calls("getLedgerEntries");
        assert_eq!(params[0]["keys"].as_array().unwrap().len(), 200);
        assert_eq!(params[1]["keys"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn no_keys_is_a_config_error_without_a_request() {
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        assert!(matches!(client.ledger_entries(&[]).await.unwrap_err(), ChainError::Config(_)));
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
        rpc.expect("getLedgerEntries", json!({"latestLedger": 8, "entries": []}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let account = client.account(ACCOUNT).await.unwrap();
        assert_eq!(account, Account { sequence: 41, latest_ledger: 7 });
        let missing = client.account(ACCOUNT).await.unwrap_err();
        assert!(matches!(missing, ChainError::NoAccount(a) if a == ACCOUNT));
        assert!(matches!(client.account("not-a-key").await.unwrap_err(), ChainError::Xdr(_)));
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
            FeeStats { soroban_percentile_70: 250, soroban_percentile_90: 300, latest_ledger: 64_289_468 }
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
            .events(&EventQuery { start_ledger: Some(64_150_000), cursor: None, contract_ids: &[POOL], limit: 100 })
            .await
            .unwrap();
        assert_eq!(events.latest_ledger, 64_271_347);
        assert_eq!(events.cursor.as_deref(), Some("0275527941655015424-0000000006"));
        assert_eq!(events.events.len(), 1);
        let decoded = &events.events[0];
        assert_eq!(decoded.ledger, event["ledger"].as_u64().unwrap().try_into().unwrap());
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
            .events(&EventQuery { start_ledger: None, cursor: Some("c-1"), contract_ids: &[POOL], limit: 10 })
            .await
            .unwrap();
        assert!(page.events.is_empty());
        assert_eq!(page.cursor, None);
        let params = &rpc.calls("getEvents")[0];
        assert!(params.get("startLedger").is_none());
        assert_eq!(params["pagination"]["cursor"], "c-1");

        for query in [
            EventQuery { start_ledger: None, cursor: None, contract_ids: &[POOL], limit: 10 },
            EventQuery { start_ledger: Some(1), cursor: Some("c"), contract_ids: &[POOL], limit: 10 },
        ] {
            assert!(matches!(client.events(&query).await.unwrap_err(), ChainError::Config(_)));
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib chain::rpc`
Expected: compile errors for the missing items.

- [ ] **Step 3: Implement**

Add to `src/chain/rpc.rs`:

```rust
use std::collections::BTreeMap;

use serde_json::json;
use stellar_xdr::{LedgerEntryData, LedgerKey, LedgerKeyAccount, ScVal};

use crate::chain::xdr::encode::{from_base64, to_base64};
use crate::chain::xdr::XdrError;
use crate::chain::TxHash;

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
    /// `getLedgerEntries` for every key, in batches of 200. Every batch must
    /// report the same `latestLedger`, or the result would describe two
    /// ledgers: a moved ledger is `LedgerMoved` and the caller retries.
    pub async fn ledger_entries(&self, keys: &[LedgerKey]) -> Result<LedgerEntries, ChainError> {
        if keys.is_empty() {
            return Err(ChainError::Config("getLedgerEntries needs at least one key"));
        }
        let mut latest_ledger = None;
        let mut entries = BTreeMap::new();
        for batch in keys.chunks(ENTRY_BATCH) {
            let encoded: Vec<String> = batch.iter().map(to_base64).collect::<Result<_, _>>()?;
            let raw: RawEntries = self.call("getLedgerEntries", json!({ "keys": encoded })).await?;
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
            _ => return Err(ChainError::Config("getEvents needs a start ledger or a cursor, not both")),
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
```

`Event` derives `Eq`; if `ScVal` in this crate version is not `Eq`, drop `Eq` from `Event` and `Events` and keep `PartialEq`. `latest_ledger.unwrap_or_default()` is reached only after at least one batch, so the default is never observed; say so in a one-line comment above it.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib chain::rpc`
Expected: 15 passed.

- [ ] **Step 5: `make check`, then commit**

```bash
git add src/chain/rpc.rs
git commit -m "feat(chain): ledger entries, account sequence, fee stats and events" -m "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 4: Simulate, send, and transaction status

**Files:**
- Modify: `src/chain/rpc.rs`

**Interfaces:**
- Consumes: `RpcClient::call`; canned builders from `chain::script`.
- Produces:
  - `pub struct RestorePreamble { pub transaction_data: SorobanTransactionData, pub min_resource_fee: i64 }`.
  - `pub struct SimulatedCall { pub return_value: ScVal, pub auth: Vec<SorobanAuthorizationEntry>, pub transaction_data: SorobanTransactionData, pub min_resource_fee: i64, pub restore: Option<RestorePreamble> }`.
  - `pub enum SimulationOutcome { Success(SimulatedCall), Failure { message: String, contract_error: Option<u32> } }`.
  - `pub struct Simulation { pub latest_ledger: u32, pub events: Vec<DiagnosticEvent>, pub outcome: SimulationOutcome }` and `pub async fn simulate(&self, envelope: &TransactionEnvelope) -> Result<Simulation, ChainError>`.
  - `pub fn contract_error_in_events(events: &[DiagnosticEvent]) -> Option<u32>` and `pub fn contract_error_in_message(message: &str) -> Option<u32>`.
  - `pub enum SendOutcome { Pending, Duplicate, TryAgainLater, Error { result: Option<TransactionResult>, contract_error: Option<u32> } }`, `pub struct SendStatus { pub hash: TxHash, pub latest_ledger: u32, pub outcome: SendOutcome }`, `pub async fn send(&self, envelope: &TransactionEnvelope) -> Result<SendStatus, ChainError>`.
  - `pub enum TransactionStatus { NotFound { latest_ledger: u32 }, Success { ledger: u32, latest_ledger: u32, return_value: Option<ScVal> }, Failed { ledger: u32, latest_ledger: u32, result: TransactionResult, contract_error: Option<u32> } }` and `pub async fn transaction(&self, hash: &TxHash) -> Result<TransactionStatus, ChainError>`.

- [ ] **Step 1: Write the failing tests**

Append to the test module in `src/chain/rpc.rs`:

```rust
    use crate::chain::script::{diagnostic_error_b64, meta_v4_b64, result_b64, scval_b64, transaction_data_b64};
    use crate::chain::xdr::encode::{address, invoke_contract_op, simulation_envelope};
    use stellar_xdr::{
        InvokeHostFunctionResult, OperationResult, OperationResultTr, TransactionResultResult, VecM,
    };

    fn envelope() -> stellar_xdr::TransactionEnvelope {
        let op = invoke_contract_op(POOL, "get_positions", vec![address(ACCOUNT).unwrap()]).unwrap();
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
        assert_eq!(params["transaction"], encode::to_base64(&envelope()).unwrap());
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
        rpc.expect("simulateTransaction", json!({"error": message, "events": [diagnostic_error_b64(1200)], "latestLedger": 9}));
        rpc.expect("simulateTransaction", json!({"error": message, "events": [], "latestLedger": 9}));
        rpc.expect("simulateTransaction", json!({"error": "HostError: Error(WasmVm, UnexpectedSize)", "latestLedger": 9}));
        rpc.expect("simulateTransaction", json!({"error": "Could not unmarshal transaction", "latestLedger": 0}));
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
        assert_eq!(contract_error_in_message("HostError: Error(Contract, #1205)\n\nEvent log"), Some(1205));
        assert_eq!(contract_error_in_message("Error(Contract, #7)"), Some(7));
        assert_eq!(contract_error_in_message("HostError: Error(WasmVm, UnexpectedSize)"), None);
        assert_eq!(contract_error_in_message("Error(Contract, #notanumber)"), None);
        assert_eq!(contract_error_in_events(&[crate::chain::script::diagnostic_error(1212)]), Some(1212));
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
        let SendOutcome::Error { result, contract_error } = &outcomes[3] else {
            panic!("expected error");
        };
        assert!(matches!(result.as_ref().unwrap().result, TransactionResultResult::TxBadSeq));
        assert_eq!(*contract_error, Some(1201));
    }

    #[tokio::test]
    async fn an_unknown_send_status_is_a_shape_error() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect("sendTransaction", json!({"status": "WEIRD", "hash": "ab".repeat(32), "latestLedger": 3}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        assert!(matches!(client.send(&envelope()).await.unwrap_err(), ChainError::Shape(_)));
    }

    #[tokio::test]
    async fn transaction_status_decodes_not_found_success_and_failed() {
        let hash = TxHash([0xcd; 32]);
        let failed = TransactionResultResult::TxFailed(
            VecM::try_from(vec![OperationResult::OpInner(OperationResultTr::InvokeHostFunction(
                InvokeHostFunctionResult::Trapped,
            ))])
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
        assert!(matches!(client.transaction(&hash).await.unwrap(), TransactionStatus::NotFound { latest_ledger: 100 }));
        let TransactionStatus::Success { ledger, latest_ledger, return_value } = client.transaction(&hash).await.unwrap() else {
            panic!("expected success");
        };
        assert_eq!((ledger, latest_ledger, return_value), (99, 101, Some(ScVal::U32(7))));
        let TransactionStatus::Failed { ledger, contract_error, result, .. } = client.transaction(&hash).await.unwrap() else {
            panic!("expected failed");
        };
        assert_eq!((ledger, contract_error), (100, Some(1205)));
        assert!(matches!(result.result, TransactionResultResult::TxFailed(_)));
        assert_eq!(rpc.calls("getTransaction")[0]["hash"], hash.to_hex());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib chain::rpc`
Expected: compile errors for the missing items.

- [ ] **Step 3: Implement**

Add to `src/chain/rpc.rs`:

```rust
use stellar_xdr::{
    ContractEventBody, DiagnosticEvent, ScError, SorobanAuthorizationEntry, SorobanTransactionData,
    TransactionEnvelope, TransactionMeta, TransactionResult,
};

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
    /// The call ran.
    Success(SimulatedCall),
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
            .call("simulateTransaction", json!({ "transaction": to_base64(envelope)? }))
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
        let missing = |field: &'static str| ChainError::Shape(format!("simulation without {field}"));
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
                    min_resource_fee: parse_i64("restorePreamble.minResourceFee", &preamble.min_resource_fee)?,
                })
            })
            .transpose()?;
        let call = SimulatedCall {
            return_value: from_base64(&result.xdr)?,
            auth,
            transaction_data: from_base64(raw.transaction_data.as_deref().ok_or_else(|| missing("transactionData"))?)?,
            min_resource_fee: parse_i64(
                "minResourceFee",
                raw.min_resource_fee.as_deref().ok_or_else(|| missing("minResourceFee"))?,
            )?,
            restore,
        };
        Ok(Simulation {
            latest_ledger: raw.latest_ledger,
            events,
            outcome: SimulationOutcome::Success(call),
        })
    }

    /// `sendTransaction`.
    pub async fn send(&self, envelope: &TransactionEnvelope) -> Result<SendStatus, ChainError> {
        let raw: RawSend = self
            .call("sendTransaction", json!({ "transaction": to_base64(envelope)? }))
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
            other => return Err(ChainError::Shape(format!("sendTransaction status {other:?}"))),
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
        let missing = |field: &'static str| ChainError::Shape(format!("{} without {field}", raw.status));
        let ledger = raw.ledger.filter(|ledger| *ledger > 0).ok_or_else(|| missing("ledger"))?;
        let meta: TransactionMeta = from_base64(raw.result_meta_xdr.as_deref().ok_or_else(|| missing("resultMetaXdr"))?)?;
        match raw.status.as_str() {
            "SUCCESS" => Ok(TransactionStatus::Success {
                ledger,
                latest_ledger,
                return_value: return_value(&meta),
            }),
            "FAILED" => {
                let result: TransactionResult =
                    from_base64(raw.result_xdr.as_deref().ok_or_else(|| missing("resultXdr"))?)?;
                let mut events = decode_events(&raw.diagnostic_events_xdr)?;
                events.extend(meta_diagnostics(&meta));
                Ok(TransactionStatus::Failed {
                    ledger,
                    latest_ledger,
                    result,
                    contract_error: contract_error_in_events(&events),
                })
            }
            other => Err(ChainError::Shape(format!("getTransaction status {other:?}"))),
        }
    }
}
```

The closure `missing` in `transaction` borrows `raw.status`, which `match raw.status.as_str()` also borrows; both are shared borrows, so it compiles. If `too_many_lines` fires on `simulate`, move the success branch into a `fn simulated_call(raw: RawSimulation) -> Result<SimulatedCall, ChainError>` helper.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib chain::rpc`
Expected: 22 passed.

- [ ] **Step 5: `make check`, then commit**

```bash
git add src/chain/rpc.rs
git commit -m "feat(chain): simulate, send and transaction status with contract-error extraction" -m "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: The network and the signer

**Files:**
- Create: `src/chain/signer.rs`
- Modify: `src/chain/mod.rs` (declare `pub mod signer;`, re-export `Network`, `Signer`)

**Interfaces:**
- Consumes: `ChainError`; `config::NetworkName`.
- Produces:
  - `pub struct Network { pub passphrase: String, pub id: [u8; 32] }` with `Network::from_passphrase(&str)`, `Network::mainnet()`, `Network::testnet()`, `Network::from_config(&ChainConfig)`.
  - `pub struct Signer` with `Signer::from_secret(secret: &str) -> Result<Self, ChainError>`, `address(&self) -> &str` (the `G…` strkey), `account_id(&self) -> AccountId`, `muxed(&self) -> MuxedAccount`, `sign(&self, tx: &Transaction, network: &Network) -> Result<TransactionEnvelope, ChainError>`; `Debug` shows the address only.

- [ ] **Step 1: Write the failing tests**

Create `src/chain/signer.rs` with the module doc, stubs, and:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::xdr::encode::{address, invoke_contract_op};
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use stellar_xdr::{Memo, Preconditions, SequenceNumber, TransactionExt, VecM};

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The well-known network ids: sha256 of the passphrase.
    #[test]
    fn the_network_id_is_the_sha256_of_the_passphrase() {
        assert_eq!(hex(&Network::mainnet().id), "7ac33997544e3175d266bd022439b22cdb16508c01163f26e5cb2a3e1045a979");
        assert_eq!(hex(&Network::testnet().id), "cee0302d59844d32bdca915c8203dd44b33fbb7edc19051ea37abedf28ecd472");
        assert_eq!(Network::from_passphrase("Test SDF Network ; September 2015"), Network::testnet());
    }

    fn secret() -> (String, SigningKey) {
        let key = SigningKey::from_bytes(&[7_u8; 32]);
        (stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string(), key)
    }

    #[test]
    fn a_signer_derives_its_account_from_the_secret() {
        let (secret, key) = secret();
        let signer = Signer::from_secret(&secret).unwrap();
        let expected = stellar_strkey::ed25519::PublicKey(key.verifying_key().to_bytes()).to_string();
        assert_eq!(signer.address(), expected);
        assert_eq!(signer.account_id().to_string(), expected);
        assert!(matches!(signer.muxed(), MuxedAccount::Ed25519(_)));
    }

    #[test]
    fn a_bad_secret_is_an_error_that_never_echoes_it() {
        let error = Signer::from_secret("SNOTAKEY").unwrap_err();
        assert!(matches!(error, ChainError::SecretKey));
        assert!(!error.to_string().contains("SNOTAKEY"));
        let (secret, _) = secret();
        // A public key is not a secret key.
        let public = Signer::from_secret(&secret).unwrap().address().to_string();
        assert!(matches!(Signer::from_secret(&public).unwrap_err(), ChainError::SecretKey));
    }

    #[test]
    fn debug_shows_the_address_and_nothing_of_the_key() {
        let (secret, key) = secret();
        let signer = Signer::from_secret(&secret).unwrap();
        let rendered = format!("{signer:?}");
        assert!(rendered.contains(signer.address()));
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains(&hex(&key.to_bytes())));
        assert!(!rendered.contains("[7, 7, 7"));
    }

    #[test]
    fn a_signature_verifies_against_the_transaction_hash_and_carries_the_hint() {
        let (secret, key) = secret();
        let signer = Signer::from_secret(&secret).unwrap();
        let tx = Transaction {
            source_account: signer.muxed(),
            fee: 100,
            seq_num: SequenceNumber(42),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: VecM::try_from(vec![invoke_contract_op(POOL, "bad_debt", vec![address(signer.address()).unwrap()]).unwrap()]).unwrap(),
            ext: TransactionExt::V0,
        };
        let network = Network::testnet();
        let envelope = signer.sign(&tx, &network).unwrap();
        let TransactionEnvelope::Tx(v1) = &envelope else {
            panic!("expected a v1 envelope");
        };
        assert_eq!(v1.tx, tx);
        assert_eq!(v1.signatures.len(), 1);
        let public = key.verifying_key().to_bytes();
        assert_eq!(v1.signatures[0].hint.0, public[28..32]);
        let hash = tx.hash(network.id).unwrap();
        let bytes: [u8; 64] = v1.signatures[0].signature.0.as_slice().try_into().unwrap();
        let signature = ed25519_dalek::Signature::from_bytes(&bytes);
        VerifyingKey::from_bytes(&public).unwrap().verify_strict(&hash, &signature).unwrap();
        // The envelope hash is the transaction hash: what getTransaction is polled by.
        assert_eq!(envelope.hash(network.id).unwrap(), hash);
        // A different network gives a different hash, so a testnet signature never lands on mainnet.
        assert_ne!(tx.hash(Network::mainnet().id).unwrap(), hash);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib chain::signer`
Expected: compile errors.

- [ ] **Step 3: Implement**

```rust
//! The network a transaction is hashed for, and the key that signs it.
//!
//! A `Signer` is the one place in the crate that holds key material. It
//! renders as its public address and nothing else, and the secret is never
//! stored as text: `from_secret` decodes it and keeps only the 32-byte seed
//! inside `ed25519_dalek::SigningKey`.

use ed25519_dalek::{Signer as _, SigningKey};
use sha2::{Digest, Sha256};
use stellar_xdr::{
    AccountId, DecoratedSignature, MuxedAccount, PublicKey, Signature, SignatureHint, Transaction,
    TransactionEnvelope, TransactionV1Envelope, Uint256, VecM,
};

use crate::chain::xdr::XdrError;
use crate::chain::ChainError;
use crate::config::{ChainConfig, NetworkName};

/// A Stellar network: its passphrase and the id every signature is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    /// The passphrase.
    pub passphrase: String,
    /// `sha256(passphrase)`, mixed into every transaction hash so a
    /// signature for one network is invalid on every other.
    pub id: [u8; 32],
}

impl Network {
    /// The network with this passphrase.
    #[must_use]
    pub fn from_passphrase(passphrase: &str) -> Self {
        Self {
            passphrase: passphrase.to_string(),
            id: Sha256::digest(passphrase.as_bytes()).into(),
        }
    }

    /// Public Global Stellar Network.
    #[must_use]
    pub fn mainnet() -> Self {
        Self::from_passphrase(NetworkName::Mainnet.passphrase())
    }

    /// Test SDF Network.
    #[must_use]
    pub fn testnet() -> Self {
        Self::from_passphrase(NetworkName::Testnet.passphrase())
    }

    /// The network the configuration names.
    #[must_use]
    pub fn from_config(config: &ChainConfig) -> Self {
        Self::from_passphrase(&config.network_passphrase)
    }
}

/// An Ed25519 signing key and the account it controls.
pub struct Signer {
    key: SigningKey,
    public: [u8; 32],
    address: String,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// Decodes an `S…` secret. Any other input, a `G…` public key included,
    /// is `SecretKey`, which carries no text.
    pub fn from_secret(secret: &str) -> Result<Self, ChainError> {
        let seed = stellar_strkey::ed25519::PrivateKey::from_string(secret)
            .map_err(|_| ChainError::SecretKey)?;
        let key = SigningKey::from_bytes(&seed.0);
        let public = key.verifying_key().to_bytes();
        let address = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(public))).to_string();
        Ok(Self {
            key,
            public,
            address,
        })
    }

    /// The `G…` strkey of the account this key controls.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The account id, for ledger keys.
    #[must_use]
    pub fn account_id(&self) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(self.public)))
    }

    /// The account as a transaction source.
    #[must_use]
    pub fn muxed(&self) -> MuxedAccount {
        MuxedAccount::Ed25519(Uint256(self.public))
    }

    /// Signs `tx` for `network` into a v1 envelope with one decorated
    /// signature. The hint is the last four bytes of the public key, which
    /// is how validators find the matching signer without trying each.
    pub fn sign(&self, tx: &Transaction, network: &Network) -> Result<TransactionEnvelope, ChainError> {
        let hash = tx.hash(network.id).map_err(XdrError::Xdr)?;
        let signature = self.key.sign(&hash);
        let mut hint = [0_u8; 4];
        hint.copy_from_slice(&self.public[28..32]);
        let decorated = DecoratedSignature {
            hint: SignatureHint(hint),
            signature: Signature(
                signature
                    .to_bytes()
                    .to_vec()
                    .try_into()
                    .map_err(XdrError::Xdr)?,
            ),
        };
        Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: tx.clone(),
            signatures: VecM::try_from(vec![decorated]).map_err(XdrError::Xdr)?,
        }))
    }
}
```

`stellar_strkey::ed25519::PrivateKey::from_string` in 0.0.13 accepts only the `S…` alphabet and version byte, so a `G…` key is rejected there. In `src/chain/mod.rs` add `pub mod signer;` and `pub use signer::{Network, Signer};`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib chain::signer`
Expected: 5 passed.

- [ ] **Step 5: `make check`, then commit**

```bash
git add src/chain/mod.rs src/chain/signer.rs
git commit -m "feat(chain): network id and Ed25519 signer" -m "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: The transaction path — prepare (build, simulate, restore, assemble, fee, sign)

**Files:**
- Create: `src/chain/tx.rs`
- Modify: `src/chain/mod.rs` (declare `pub mod tx;`, re-export `Submitter`, `TxConfig`, `TxOutcome`, `Priority`)

**Interfaces:**
- Consumes: `RpcClient::{account, fee_stats, simulate, send, transaction}` and their types (Tasks 3–4); `Signer`, `Network` (Task 5); `ChainConfig`.
- Produces:
  - `pub const TIME_BOUND_SECS: u64 = 300`.
  - `pub struct TxConfig { pub base_fee: u32, pub high_fee: u32, pub poll_ledgers: u32, pub poll_interval: Duration, pub send_retry_pause: Duration, pub wait_cap: Duration }` with `TxConfig::new(base_fee, high_fee, poll_ledgers)` (1 s, 1 s, 10 s × (poll_ledgers + 1)) and `TxConfig::from_config(&ChainConfig)`.
  - `pub enum Priority { Normal, High }`.
  - `pub struct Prepared { pub envelope: TransactionEnvelope, pub hash: TxHash, pub sequence: i64, pub max_ledger: u32, pub fee: u32, pub resource_fee: i64 }`.
  - `pub enum TxOutcome { Succeeded { hash: TxHash, ledger: u32, return_value: Option<ScVal> }, Failed { hash: TxHash, ledger: u32, contract_error: Option<u32>, result: TransactionResult }, Expired { hash: TxHash, max_ledger: u32, latest_ledger: u32 }, Unknown { hash: TxHash, sequence: i64, max_ledger: u32 } }` (Task 7 fills in its use).
  - `pub struct Submitter<'a>` with `Submitter::new(rpc: &'a RpcClient, network: &'a Network, signer: &'a Signer, config: TxConfig)`, `pub fn inclusion_fee(&self, fees: &FeeStats, priority: Priority) -> u32`, `pub async fn prepare(&self, operation: Operation, priority: Priority) -> Result<Prepared, ChainError>`.
  - Task 7 adds `send`, `wait`, `classify`, `submit` on the same type; this task's `restore` already needs a minimal send-and-wait, so `send` and `wait` are implemented here and tested in Task 7.

- [ ] **Step 1: Write the failing tests**

Create `src/chain/tx.rs` with the module doc, stubs, and this test module (Task 7 extends it):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::{
        account_entry_b64, diagnostic_error_b64, meta_v4_b64, result_b64, scval_b64, transaction_data_b64,
        ScriptedRpc,
    };
    use crate::chain::xdr::encode::{address, from_base64, invoke_contract_op, to_base64};
    use serde_json::json;
    use stellar_xdr::{
        LedgerKey, LedgerKeyAccount, OperationBody, Preconditions, TransactionEnvelope, TransactionExt,
        TransactionResultResult, VecM,
    };

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";

    fn signer() -> Signer {
        let key = ed25519_dalek::SigningKey::from_bytes(&[9_u8; 32]);
        Signer::from_secret(&stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string()).unwrap()
    }

    fn config() -> TxConfig {
        TxConfig {
            poll_interval: Duration::from_millis(5),
            send_retry_pause: Duration::from_millis(5),
            wait_cap: Duration::from_millis(200),
            ..TxConfig::new(5_000, 10_000, 3)
        }
    }

    fn operation() -> Operation {
        invoke_contract_op(POOL, "bad_debt", vec![address(signer().address()).unwrap()]).unwrap()
    }

    fn script_account(rpc: &ScriptedRpc, sequence: i64, ledger: u32) {
        let key = LedgerKey::Account(LedgerKeyAccount { account_id: signer().account_id() });
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                {"key": to_base64(&key).unwrap(), "xdr": account_entry_b64(signer().address(), sequence), "lastModifiedLedgerSeq": 1}
            ]}),
        );
    }

    fn script_fees(rpc: &ScriptedRpc, percentile_70: u32, percentile_90: u32) {
        rpc.expect(
            "getFeeStats",
            json!({"sorobanInclusionFee": {"p70": percentile_70.to_string(), "p90": percentile_90.to_string()},
                   "inclusionFee": {"p70": "100", "p90": "100"}, "latestLedger": 100}),
        );
    }

    fn script_simulation(rpc: &ScriptedRpc, resource_fee: i64, ledger: u32, restore_fee: Option<i64>) {
        let mut body = json!({"transactionData": transaction_data_b64(resource_fee), "events": [],
                              "minResourceFee": resource_fee.to_string(),
                              "results": [{"auth": [], "xdr": scval_b64(&ScVal::Void)}],
                              "latestLedger": ledger});
        if let Some(fee) = restore_fee {
            body["restorePreamble"] = json!({"minResourceFee": fee.to_string(), "transactionData": transaction_data_b64(fee)});
        }
        rpc.expect("simulateTransaction", body);
    }

    fn sent_transaction(rpc: &ScriptedRpc, index: usize) -> Transaction {
        let params = rpc.calls("sendTransaction");
        let envelope: TransactionEnvelope = from_base64(params[index]["transaction"].as_str().unwrap()).unwrap();
        let TransactionEnvelope::Tx(v1) = envelope else { panic!("expected a v1 envelope") };
        v1.tx
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }

    #[tokio::test]
    async fn prepare_builds_a_bounded_signed_transaction_with_the_fee_policy() {
        let rpc = ScriptedRpc::start().await;
        script_account(&rpc, 41, 100);
        script_fees(&rpc, 200, 9_000);
        script_simulation(&rpc, 446_953, 100, None);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = Submitter::new(&client, &network, &signer, config());

        let prepared = submitter.prepare(operation(), Priority::Normal).await.unwrap();
        assert_eq!(prepared.sequence, 42);
        assert_eq!(prepared.max_ledger, 104, "latest 100 + poll 3 + 1, exclusive");
        assert_eq!(prepared.fee, 5_000 + 446_953, "p70 200 is below the 5000 floor");
        assert_eq!(prepared.resource_fee, 446_953);
        let TransactionEnvelope::Tx(v1) = &prepared.envelope else { panic!("v1") };
        assert_eq!(v1.signatures.len(), 1);
        assert_eq!(v1.tx.seq_num.0, 42);
        assert_eq!(v1.tx.source_account, signer.muxed());
        assert_eq!(v1.tx.fee, prepared.fee);
        let Preconditions::V2(cond) = &v1.tx.cond else { panic!("v2 preconditions") };
        let time = cond.time_bounds.as_ref().unwrap();
        assert_eq!(time.min_time.0, 0);
        assert!((unix_now() + TIME_BOUND_SECS - 5..=unix_now() + TIME_BOUND_SECS).contains(&time.max_time.0));
        let ledgers = cond.ledger_bounds.as_ref().unwrap();
        assert_eq!((ledgers.min_ledger, ledgers.max_ledger), (0, 104));
        let TransactionExt::V1(data) = &v1.tx.ext else { panic!("soroban data attached") };
        assert_eq!(data.resource_fee, 446_953);
        assert_eq!(prepared.hash.0, prepared.envelope.hash(network.id).unwrap());
        // The simulation saw the real source account and sequence, unsigned.
        let simulated: TransactionEnvelope =
            from_base64(rpc.calls("simulateTransaction")[0]["transaction"].as_str().unwrap()).unwrap();
        let TransactionEnvelope::Tx(sim) = simulated else { panic!("v1") };
        assert_eq!((sim.tx.seq_num.0, sim.signatures.len()), (42, 0));
        assert_eq!(sim.tx.source_account, signer.muxed());
        assert_eq!(rpc.remaining(), 0);
    }

    #[tokio::test]
    async fn the_inclusion_fee_is_the_percentile_floored_by_priority() {
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = Submitter::new(&client, &network, &signer, config());
        let fees = FeeStats { soroban_percentile_70: 7_000, soroban_percentile_90: 9_000, latest_ledger: 1 };
        assert_eq!(submitter.inclusion_fee(&fees, Priority::Normal), 7_000);
        assert_eq!(submitter.inclusion_fee(&fees, Priority::High), 10_000);
        let quiet = FeeStats { soroban_percentile_70: 100, soroban_percentile_90: 100, latest_ledger: 1 };
        assert_eq!(submitter.inclusion_fee(&quiet, Priority::Normal), 5_000);
        assert_eq!(submitter.inclusion_fee(&quiet, Priority::High), 10_000);
        let busy = FeeStats { soroban_percentile_70: 20_000, soroban_percentile_90: 30_000, latest_ledger: 1 };
        assert_eq!(submitter.inclusion_fee(&busy, Priority::High), 30_000);
    }

    #[tokio::test]
    async fn prepare_restores_archived_entries_then_simulates_again() {
        let rpc = ScriptedRpc::start().await;
        script_account(&rpc, 41, 100);
        script_fees(&rpc, 200, 200);
        script_simulation(&rpc, 10, 100, Some(77));
        rpc.expect("sendTransaction", json!({"status": "PENDING", "hash": "11".repeat(32), "latestLedger": 100}));
        rpc.expect(
            "getTransaction",
            json!({"status": "SUCCESS", "latestLedger": 101, "oldestLedger": 1, "ledger": 101,
                   "resultXdr": result_b64(TransactionResultResult::TxSuccess(VecM::default())),
                   "resultMetaXdr": meta_v4_b64(None, vec![])}),
        );
        script_simulation(&rpc, 500, 101, None);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = Submitter::new(&client, &network, &signer, config());

        let prepared = submitter.prepare(operation(), Priority::Normal).await.unwrap();
        assert_eq!(prepared.sequence, 43, "the restore consumed sequence 42");
        assert_eq!(prepared.fee, 5_000 + 500);
        assert_eq!(prepared.max_ledger, 105, "bounded from the second simulation's ledger");
        let restore = sent_transaction(&rpc, 0);
        assert_eq!(restore.seq_num.0, 42);
        assert!(matches!(restore.operations[0].body, OperationBody::RestoreFootprint(_)));
        assert_eq!(restore.fee, 5_000 + 77);
        let TransactionExt::V1(data) = &restore.ext else { panic!("restore data attached") };
        assert_eq!(data.resource_fee, 77);
        assert_eq!(rpc.calls("simulateTransaction").len(), 2);
        assert_eq!(rpc.remaining(), 0);
    }

    #[tokio::test]
    async fn a_failed_restore_is_a_restore_error() {
        let rpc = ScriptedRpc::start().await;
        script_account(&rpc, 41, 100);
        script_fees(&rpc, 200, 200);
        script_simulation(&rpc, 10, 100, Some(77));
        rpc.expect("sendTransaction", json!({"status": "PENDING", "hash": "11".repeat(32), "latestLedger": 100}));
        rpc.expect(
            "getTransaction",
            json!({"status": "FAILED", "latestLedger": 101, "oldestLedger": 1, "ledger": 101,
                   "resultXdr": result_b64(TransactionResultResult::TxInsufficientBalance),
                   "resultMetaXdr": meta_v4_b64(None, vec![])}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = Submitter::new(&client, &network, &signer, config());
        assert!(matches!(submitter.prepare(operation(), Priority::Normal).await.unwrap_err(), ChainError::Restore(_)));
    }

    #[tokio::test]
    async fn a_simulation_failure_surfaces_the_contract_error() {
        let rpc = ScriptedRpc::start().await;
        script_account(&rpc, 41, 100);
        script_fees(&rpc, 200, 200);
        rpc.expect(
            "simulateTransaction",
            json!({"error": "HostError: Error(Contract, #1212)", "events": [diagnostic_error_b64(1212)], "latestLedger": 100}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = Submitter::new(&client, &network, &signer, config());
        let error = submitter.prepare(operation(), Priority::Normal).await.unwrap_err();
        assert!(matches!(error, ChainError::Simulation { contract_error: Some(1212), .. }), "{error:?}");
        assert!(rpc.calls("sendTransaction").is_empty(), "nothing is sent after a failed simulation");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib chain::tx`
Expected: compile errors.

- [ ] **Step 3: Implement**

`src/chain/tx.rs`:

```rust
//! The one write path: build, simulate, restore, assemble, fee, sign, send,
//! poll, classify — section 3 of the design spec.
//!
//! Every transaction carries two bounds. The time bound (now + 5 minutes)
//! is the network's convention; the ledger bound, `latest_ledger +
//! poll_ledgers + 1` and exclusive, is what makes the outcome decidable:
//! once the RPC's `latestLedger` reaches it, a transaction the RPC has not
//! seen can never be applied, and `wait` reports `Expired` — provably not
//! included — instead of leaving the caller to guess. `Unknown` is kept for
//! the case where the RPC could not answer for the whole window.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use stellar_xdr::{
    Duration as XdrDuration, ExtensionPoint, LedgerBounds, Memo, Operation, OperationBody,
    Preconditions, PreconditionsV2, RestoreFootprintOp, ScVal, SequenceNumber, TimeBounds, TimePoint,
    Transaction, TransactionEnvelope, TransactionExt, TransactionResult, TransactionResultResult,
    TransactionV1Envelope, VecM,
};

use crate::chain::rpc::{
    FeeStats, RestorePreamble, RpcClient, SendOutcome, SimulatedCall, SimulationOutcome,
    TransactionStatus,
};
use crate::chain::signer::{Network, Signer};
use crate::chain::xdr::XdrError;
use crate::chain::{ChainError, TxHash};
use crate::config::ChainConfig;

/// How long after building a transaction the network may still apply it.
pub const TIME_BOUND_SECS: u64 = 300;

/// The fee and polling policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxConfig {
    /// Inclusion-fee floor for a normal-priority transaction, stroops.
    pub base_fee: u32,
    /// Inclusion-fee floor for a high-priority transaction, stroops.
    pub high_fee: u32,
    /// Ledgers a transaction stays valid; also the polling horizon.
    pub poll_ledgers: u32,
    /// Pause between `getTransaction` polls.
    pub poll_interval: Duration,
    /// Pause before the one retry of a `TRY_AGAIN_LATER`.
    pub send_retry_pause: Duration,
    /// Wall-clock cap on `wait`; after it an unseen transaction is `Unknown`.
    pub wait_cap: Duration,
}

impl TxConfig {
    /// The production timings: poll every second, retry a full queue after a
    /// second, and give the chain ten seconds per ledger of the window.
    #[must_use]
    pub fn new(base_fee: u32, high_fee: u32, poll_ledgers: u32) -> Self {
        Self {
            base_fee,
            high_fee,
            poll_ledgers,
            poll_interval: Duration::from_secs(1),
            send_retry_pause: Duration::from_secs(1),
            wait_cap: Duration::from_secs(10) * poll_ledgers.saturating_add(1),
        }
    }

    /// From validated configuration.
    #[must_use]
    pub fn from_config(config: &ChainConfig) -> Self {
        Self::new(config.base_fee, config.high_fee, config.tx_poll_ledgers)
    }
}

/// Which inclusion-fee percentile and floor a transaction gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// p70, floored at `base_fee`.
    Normal,
    /// p90, floored at `high_fee`: a fill worth paying to land first.
    High,
}

/// A signed transaction and what the caller needs to track it.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The signed envelope, ready for `sendTransaction`.
    pub envelope: TransactionEnvelope,
    /// Its hash, what `getTransaction` is polled by.
    pub hash: TxHash,
    /// The sequence number it consumes.
    pub sequence: i64,
    /// Exclusive: the transaction cannot be applied in this ledger or later.
    pub max_ledger: u32,
    /// The total fee: inclusion plus resource.
    pub fee: u32,
    /// The resource fee the simulation asked for.
    pub resource_fee: i64,
}

/// How a submitted transaction ended.
#[derive(Debug, Clone)]
pub enum TxOutcome {
    /// Applied and succeeded.
    Succeeded {
        /// The transaction.
        hash: TxHash,
        /// The ledger it landed in.
        ledger: u32,
        /// The host function's return value, when the meta carries one.
        return_value: Option<ScVal>,
    },
    /// Applied and failed; the fee was charged.
    Failed {
        /// The transaction.
        hash: TxHash,
        /// The ledger it failed in.
        ledger: u32,
        /// The pool's error code, when the failure was a contract error.
        contract_error: Option<u32>,
        /// The decoded result.
        result: TransactionResult,
    },
    /// The chain passed the ledger bound without applying it: it never will.
    /// A retry with a fresh sequence number is safe.
    Expired {
        /// The transaction.
        hash: TxHash,
        /// The bound it missed.
        max_ledger: u32,
        /// The ledger the RPC had when that became certain.
        latest_ledger: u32,
    },
    /// The RPC could not say within the window. The transaction may still
    /// land; the caller keeps polling `getTransaction` for `hash` until it
    /// does or the chain passes `max_ledger`.
    Unknown {
        /// The transaction.
        hash: TxHash,
        /// The sequence it would consume if it lands.
        sequence: i64,
        /// The bound after which it cannot.
        max_ledger: u32,
    },
}

/// Builds, signs and submits transactions for one signer on one network.
#[derive(Debug, Clone, Copy)]
pub struct Submitter<'a> {
    rpc: &'a RpcClient,
    network: &'a Network,
    signer: &'a Signer,
    config: TxConfig,
}

fn unix_now() -> Result<u64, ChainError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| ChainError::Config("the system clock is before 1970"))
}

/// Inclusion plus resource fee, which the transaction carries as one `u32`.
fn total_fee(inclusion: u32, resource_fee: i64) -> Result<u32, ChainError> {
    u32::try_from(resource_fee)
        .ok()
        .and_then(|resource| inclusion.checked_add(resource))
        .ok_or_else(|| {
            ChainError::Shape(format!(
                "fee {inclusion} + {resource_fee} does not fit the transaction's u32"
            ))
        })
}

impl<'a> Submitter<'a> {
    /// A submitter for `signer` on `network` through `rpc`.
    #[must_use]
    pub fn new(rpc: &'a RpcClient, network: &'a Network, signer: &'a Signer, config: TxConfig) -> Self {
        Self {
            rpc,
            network,
            signer,
            config,
        }
    }

    /// The fee policy: p70 floored at `base_fee`, or p90 floored at
    /// `high_fee` for a high-priority transaction.
    #[must_use]
    pub fn inclusion_fee(&self, fees: &FeeStats, priority: Priority) -> u32 {
        match priority {
            Priority::Normal => fees.soroban_percentile_70.max(self.config.base_fee),
            Priority::High => fees.soroban_percentile_90.max(self.config.high_fee),
        }
    }

    /// An unsigned transaction with both bounds. Returns the exclusive
    /// ledger bound alongside it.
    fn unsigned(
        &self,
        operation: Operation,
        sequence: i64,
        latest_ledger: u32,
        fee: u32,
    ) -> Result<(Transaction, u32), ChainError> {
        let now = unix_now()?;
        let max_ledger = latest_ledger
            .checked_add(self.config.poll_ledgers)
            .and_then(|ledger| ledger.checked_add(1))
            .ok_or(ChainError::Config("the ledger bound overflows u32"))?;
        let tx = Transaction {
            source_account: self.signer.muxed(),
            fee,
            seq_num: SequenceNumber(sequence),
            cond: Preconditions::V2(PreconditionsV2 {
                time_bounds: Some(TimeBounds {
                    min_time: TimePoint(0),
                    max_time: TimePoint(now.saturating_add(TIME_BOUND_SECS)),
                }),
                ledger_bounds: Some(LedgerBounds {
                    min_ledger: 0,
                    max_ledger,
                }),
                min_seq_num: None,
                min_seq_age: XdrDuration(0),
                min_seq_ledger_gap: 0,
                extra_signers: VecM::default(),
            }),
            memo: Memo::None,
            operations: VecM::try_from(vec![operation]).map_err(XdrError::Xdr)?,
            ext: TransactionExt::V0,
        };
        Ok((tx, max_ledger))
    }

    /// Simulates `operation` as the signer at `sequence`. A refusal is
    /// `Simulation`, with the contract code when there is one.
    async fn simulate_call(
        &self,
        operation: &Operation,
        sequence: i64,
        latest_ledger: u32,
    ) -> Result<(SimulatedCall, u32), ChainError> {
        let (tx, _) = self.unsigned(operation.clone(), sequence, latest_ledger, 100)?;
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });
        let simulation = self.rpc.simulate(&envelope).await?;
        match simulation.outcome {
            SimulationOutcome::Success(call) => Ok((call, simulation.latest_ledger)),
            SimulationOutcome::Failure {
                message,
                contract_error,
            } => Err(ChainError::Simulation {
                message,
                contract_error,
            }),
        }
    }

    /// Attaches what the simulation produced: the Soroban data, the
    /// authorisation entries (only when the operation has none of its own)
    /// and the total fee.
    fn assemble(mut tx: Transaction, call: &SimulatedCall, inclusion: u32) -> Result<Transaction, ChainError> {
        tx.fee = total_fee(inclusion, call.min_resource_fee)?;
        tx.ext = TransactionExt::V1(call.transaction_data.clone());
        let mut operations = tx.operations.to_vec();
        if let Some(Operation {
            body: OperationBody::InvokeHostFunction(invoke),
            ..
        }) = operations.first_mut()
        {
            if invoke.auth.is_empty() && !call.auth.is_empty() {
                invoke.auth = VecM::try_from(call.auth.clone()).map_err(XdrError::Xdr)?;
            }
        }
        tx.operations = VecM::try_from(operations).map_err(XdrError::Xdr)?;
        Ok(tx)
    }

    /// Restores the archived entries a simulation reported, consuming one
    /// sequence number, and waits for the restore to land.
    async fn restore(
        &self,
        preamble: RestorePreamble,
        sequence: i64,
        latest_ledger: u32,
        inclusion: u32,
    ) -> Result<(), ChainError> {
        let operation = Operation {
            source_account: None,
            body: OperationBody::RestoreFootprint(RestoreFootprintOp {
                ext: ExtensionPoint::V0,
            }),
        };
        let fee = total_fee(inclusion, preamble.min_resource_fee)?;
        let (mut tx, max_ledger) = self.unsigned(operation, sequence, latest_ledger, fee)?;
        tx.ext = TransactionExt::V1(preamble.transaction_data);
        let envelope = self.signer.sign(&tx, self.network)?;
        let hash = TxHash(envelope.hash(self.network.id).map_err(XdrError::Xdr)?);
        let prepared = Prepared {
            envelope,
            hash,
            sequence,
            max_ledger,
            fee,
            resource_fee: preamble.min_resource_fee,
        };
        self.send(&prepared).await?;
        match self.wait(&prepared).await? {
            TxOutcome::Succeeded { .. } => Ok(()),
            other => Err(ChainError::Restore(format!("{other:?}"))),
        }
    }

    /// Steps 1 to 6 of the write path: sequence, build, simulate (restoring
    /// archived entries first when the simulation says so, then simulating
    /// again), assemble, fee, sign.
    pub async fn prepare(&self, operation: Operation, priority: Priority) -> Result<Prepared, ChainError> {
        let account = self.rpc.account(self.signer.address()).await?;
        let fees = self.rpc.fee_stats().await?;
        let inclusion = self.inclusion_fee(&fees, priority);
        let mut sequence = account
            .sequence
            .checked_add(1)
            .ok_or_else(|| ChainError::Shape("the account sequence overflows i64".to_string()))?;
        let (mut call, mut latest_ledger) =
            self.simulate_call(&operation, sequence, account.latest_ledger).await?;
        if let Some(preamble) = call.restore.take() {
            self.restore(preamble, sequence, latest_ledger, inclusion).await?;
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| ChainError::Shape("the account sequence overflows i64".to_string()))?;
            (call, latest_ledger) = self.simulate_call(&operation, sequence, latest_ledger).await?;
            if call.restore.is_some() {
                return Err(ChainError::Restore(
                    "archived entries remain after a restore".to_string(),
                ));
            }
        }
        let (tx, max_ledger) = self.unsigned(operation, sequence, latest_ledger, 0)?;
        let tx = Self::assemble(tx, &call, inclusion)?;
        let envelope = self.signer.sign(&tx, self.network)?;
        let hash = TxHash(envelope.hash(self.network.id).map_err(XdrError::Xdr)?);
        Ok(Prepared {
            envelope,
            hash,
            sequence,
            max_ledger,
            fee: tx.fee,
            resource_fee: call.min_resource_fee,
        })
    }
}
```

`send` and `wait` are part of Task 7's code block; write them now from that block so `restore` compiles, and Task 7 adds their tests. In `src/chain/mod.rs` add `pub mod tx;` and `pub use tx::{Prepared, Priority, Submitter, TxConfig, TxOutcome};`.

`tx.operations.to_vec()` needs `VecM::to_vec`; if that method is absent in this crate version, use `tx.operations.iter().cloned().collect::<Vec<_>>()`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib chain::tx`
Expected: 5 passed.

- [ ] **Step 5: `make check`, then commit**

```bash
git add src/chain/mod.rs src/chain/tx.rs
git commit -m "feat(chain): transaction preparation with restore, fee policy and bounds" -m "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 7: The transaction path — send, wait, classify, submit

**Files:**
- Modify: `src/chain/tx.rs`

**Interfaces:**
- Produces on `Submitter`: `pub async fn send(&self, prepared: &Prepared) -> Result<(), ChainError>` (one retry of `TRY_AGAIN_LATER`; `TxBadSeq` is `BadSequence`; any other rejection is `Rejected`), `pub async fn wait(&self, prepared: &Prepared) -> Result<TxOutcome, ChainError>`, `pub async fn submit(&self, operation: Operation, priority: Priority) -> Result<TxOutcome, ChainError>`; and the pure `pub fn classify(status: TransactionStatus, prepared: &Prepared) -> Option<TxOutcome>` (`None` = keep polling).

- [ ] **Step 1: Write the failing tests**

Append to the test module in `src/chain/tx.rs`:

```rust
    fn prepared(max_ledger: u32) -> Prepared {
        let envelope = crate::chain::xdr::encode::simulation_envelope(operation()).unwrap();
        Prepared { envelope, hash: TxHash([0x42; 32]), sequence: 42, max_ledger, fee: 100, resource_fee: 0 }
    }

    fn submitter_for(client: &RpcClient, network: &Network, signer: &Signer) -> Submitter<'_> {
        // The test config: millisecond pauses, a 200 ms wait cap.
        Submitter::new(client, network, signer, config())
    }

    #[tokio::test]
    async fn send_retries_try_again_later_exactly_once() {
        let hash = "42".repeat(32);
        let rpc = ScriptedRpc::start().await;
        rpc.expect("sendTransaction", json!({"status": "TRY_AGAIN_LATER", "hash": hash, "latestLedger": 1}));
        rpc.expect("sendTransaction", json!({"status": "PENDING", "hash": hash, "latestLedger": 1}));
        rpc.expect("sendTransaction", json!({"status": "TRY_AGAIN_LATER", "hash": hash, "latestLedger": 1}));
        rpc.expect("sendTransaction", json!({"status": "TRY_AGAIN_LATER", "hash": hash, "latestLedger": 1}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = submitter_for(&client, &network, &signer);
        submitter.send(&prepared(104)).await.unwrap();
        assert!(matches!(submitter.send(&prepared(104)).await.unwrap_err(), ChainError::Rejected(_)));
        assert_eq!(rpc.calls("sendTransaction").len(), 4);
    }

    #[tokio::test]
    async fn a_bad_sequence_at_send_is_bad_sequence_and_other_rejections_are_rejected() {
        let hash = "42".repeat(32);
        let rpc = ScriptedRpc::start().await;
        rpc.expect("sendTransaction", json!({"status": "ERROR", "hash": hash, "latestLedger": 1,
                   "errorResultXdr": result_b64(TransactionResultResult::TxBadSeq)}));
        rpc.expect("sendTransaction", json!({"status": "ERROR", "hash": hash, "latestLedger": 1,
                   "errorResultXdr": result_b64(TransactionResultResult::TxInsufficientFee)}));
        rpc.expect("sendTransaction", json!({"status": "DUPLICATE", "hash": hash, "latestLedger": 1}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = submitter_for(&client, &network, &signer);
        assert!(matches!(submitter.send(&prepared(104)).await.unwrap_err(), ChainError::BadSequence));
        assert!(matches!(submitter.send(&prepared(104)).await.unwrap_err(), ChainError::Rejected(_)));
        submitter.send(&prepared(104)).await.unwrap();
    }

    #[test]
    fn classify_decides_from_the_status_and_the_ledger_bound() {
        let prepared = prepared(104);
        let success = TransactionStatus::Success { ledger: 102, latest_ledger: 102, return_value: Some(ScVal::U32(7)) };
        assert!(matches!(classify(success, &prepared), Some(TxOutcome::Succeeded { ledger: 102, return_value: Some(ScVal::U32(7)), .. })));
        let result: TransactionResult = from_base64(&result_b64(TransactionResultResult::TxFailed(VecM::default()))).unwrap();
        let failed = TransactionStatus::Failed { ledger: 103, latest_ledger: 103, result, contract_error: Some(1205) };
        assert!(matches!(classify(failed, &prepared), Some(TxOutcome::Failed { ledger: 103, contract_error: Some(1205), .. })));
        assert!(classify(TransactionStatus::NotFound { latest_ledger: 103 }, &prepared).is_none());
        assert!(matches!(
            classify(TransactionStatus::NotFound { latest_ledger: 104 }, &prepared),
            Some(TxOutcome::Expired { max_ledger: 104, latest_ledger: 104, .. })
        ));
    }

    #[tokio::test]
    async fn wait_polls_until_a_terminal_status_or_expiry() {
        let rpc = ScriptedRpc::start().await;
        let not_found = |latest: u32| json!({"status": "NOT_FOUND", "latestLedger": latest, "oldestLedger": 1, "ledger": 0});
        rpc.expect("getTransaction", not_found(101));
        rpc.expect("getTransaction", not_found(102));
        rpc.expect(
            "getTransaction",
            json!({"status": "SUCCESS", "latestLedger": 102, "oldestLedger": 1, "ledger": 102,
                   "resultXdr": result_b64(TransactionResultResult::TxSuccess(VecM::default())),
                   "resultMetaXdr": meta_v4_b64(Some(ScVal::U32(7)), vec![])}),
        );
        rpc.expect("getTransaction", not_found(104));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = submitter_for(&client, &network, &signer);
        let outcome = submitter.wait(&prepared(104)).await.unwrap();
        assert!(matches!(outcome, TxOutcome::Succeeded { ledger: 102, return_value: Some(ScVal::U32(7)), .. }), "{outcome:?}");
        assert_eq!(rpc.calls("getTransaction").len(), 3);
        let expired = submitter.wait(&prepared(104)).await.unwrap();
        assert!(matches!(expired, TxOutcome::Expired { max_ledger: 104, latest_ledger: 104, .. }), "{expired:?}");
    }

    #[tokio::test]
    async fn wait_reports_a_failed_transaction_with_its_contract_error() {
        let rpc = ScriptedRpc::start().await;
        let failed = TransactionResultResult::TxFailed(VecM::default());
        rpc.expect(
            "getTransaction",
            json!({"status": "FAILED", "latestLedger": 103, "oldestLedger": 1, "ledger": 103,
                   "resultXdr": result_b64(failed), "resultMetaXdr": meta_v4_b64(None, vec![]),
                   "diagnosticEventsXdr": [diagnostic_error_b64(1205)]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = submitter_for(&client, &network, &signer);
        let outcome = submitter.wait(&prepared(104)).await.unwrap();
        assert!(matches!(outcome, TxOutcome::Failed { ledger: 103, contract_error: Some(1205), .. }), "{outcome:?}");
    }

    #[tokio::test]
    async fn wait_is_unknown_when_the_rpc_cannot_answer_for_the_whole_window() {
        // Nothing scripted: every poll is an HTTP 500, which is transient
        // from the caller's point of view, so wait keeps trying until its cap.
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = submitter_for(&client, &network, &signer);
        let started = Instant::now();
        let outcome = submitter.wait(&prepared(104)).await.unwrap();
        assert!(matches!(outcome, TxOutcome::Unknown { sequence: 42, max_ledger: 104, .. }), "{outcome:?}");
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(rpc.calls("getTransaction").len() >= 2);
    }

    #[tokio::test]
    async fn submit_runs_the_whole_path() {
        let rpc = ScriptedRpc::start().await;
        script_account(&rpc, 41, 100);
        script_fees(&rpc, 200, 200);
        script_simulation(&rpc, 300, 100, None);
        rpc.expect("sendTransaction", json!({"status": "PENDING", "hash": "77".repeat(32), "latestLedger": 100}));
        rpc.expect(
            "getTransaction",
            json!({"status": "SUCCESS", "latestLedger": 101, "oldestLedger": 1, "ledger": 101,
                   "resultXdr": result_b64(TransactionResultResult::TxSuccess(VecM::default())),
                   "resultMetaXdr": meta_v4_b64(Some(ScVal::I32(-1)), vec![])}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = submitter_for(&client, &network, &signer);
        let outcome = submitter.submit(operation(), Priority::High).await.unwrap();
        let TxOutcome::Succeeded { ledger, return_value, hash } = outcome else { panic!("succeeded") };
        assert_eq!((ledger, return_value), (101, Some(ScVal::I32(-1))));
        // The hash polled is the hash of the envelope that was sent.
        let sent = sent_transaction(&rpc, 0);
        assert_eq!(sent.fee, 10_000 + 300);
        assert_eq!(rpc.calls("getTransaction")[0]["hash"], hash.to_hex());
        assert_eq!(rpc.remaining(), 0);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib chain::tx`
Expected: compile errors for `send`, `wait`, `classify`, `submit` (Task 6 may have added `send`/`wait` already, in which case only the new tests fail).

- [ ] **Step 3: Implement**

Add to `src/chain/tx.rs`:

```rust
/// What a `getTransaction` answer means for `prepared`: a terminal outcome,
/// or `None` while the transaction may still land. `NotFound` becomes
/// `Expired` the moment the RPC's ledger reaches the bound.
#[must_use]
pub fn classify(status: TransactionStatus, prepared: &Prepared) -> Option<TxOutcome> {
    match status {
        TransactionStatus::Success {
            ledger,
            return_value,
            ..
        } => Some(TxOutcome::Succeeded {
            hash: prepared.hash,
            ledger,
            return_value,
        }),
        TransactionStatus::Failed {
            ledger,
            result,
            contract_error,
            ..
        } => Some(TxOutcome::Failed {
            hash: prepared.hash,
            ledger,
            contract_error,
            result,
        }),
        TransactionStatus::NotFound { latest_ledger } if latest_ledger >= prepared.max_ledger => {
            Some(TxOutcome::Expired {
                hash: prepared.hash,
                max_ledger: prepared.max_ledger,
                latest_ledger,
            })
        }
        TransactionStatus::NotFound { .. } => None,
    }
}

impl Submitter<'_> {
    /// `sendTransaction`, retrying a `TRY_AGAIN_LATER` once after a pause.
    /// A `TxBadSeq` rejection is `BadSequence`: the plan is stale and must be
    /// rebuilt, never resent. Any other rejection is `Rejected`.
    pub async fn send(&self, prepared: &Prepared) -> Result<(), ChainError> {
        let mut retried = false;
        loop {
            let status = self.rpc.send(&prepared.envelope).await?;
            match status.outcome {
                SendOutcome::Pending | SendOutcome::Duplicate => return Ok(()),
                SendOutcome::TryAgainLater if !retried => {
                    retried = true;
                    tokio::time::sleep(self.config.send_retry_pause).await;
                }
                SendOutcome::TryAgainLater => {
                    return Err(ChainError::Rejected("TRY_AGAIN_LATER twice".to_string()));
                }
                SendOutcome::Error {
                    result,
                    contract_error,
                } => {
                    if let Some(TransactionResult {
                        result: TransactionResultResult::TxBadSeq,
                        ..
                    }) = result
                    {
                        return Err(ChainError::BadSequence);
                    }
                    return Err(ChainError::Rejected(format!(
                        "{result:?} (contract error {contract_error:?})"
                    )));
                }
            }
        }
    }

    /// Polls `getTransaction` until the outcome is terminal, the chain has
    /// passed the ledger bound (`Expired`), or the wait cap passes without an
    /// answer (`Unknown`). RPC errors while polling are transient here: the
    /// transaction is in flight and only the chain can say what happened.
    pub async fn wait(&self, prepared: &Prepared) -> Result<TxOutcome, ChainError> {
        let deadline = Instant::now() + self.config.wait_cap;
        loop {
            match self.rpc.transaction(&prepared.hash).await {
                Ok(status) => {
                    if let Some(outcome) = classify(status, prepared) {
                        return Ok(outcome);
                    }
                }
                Err(error) => {
                    tracing::warn!(hash = %prepared.hash, %error, "getTransaction failed; polling again");
                }
            }
            if Instant::now() >= deadline {
                return Ok(TxOutcome::Unknown {
                    hash: prepared.hash,
                    sequence: prepared.sequence,
                    max_ledger: prepared.max_ledger,
                });
            }
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// The whole write path: prepare, send, wait.
    pub async fn submit(&self, operation: Operation, priority: Priority) -> Result<TxOutcome, ChainError> {
        let prepared = self.prepare(operation, priority).await?;
        tracing::info!(
            hash = %prepared.hash,
            sequence = prepared.sequence,
            max_ledger = prepared.max_ledger,
            fee = prepared.fee,
            "sending transaction"
        );
        self.send(&prepared).await?;
        self.wait(&prepared).await
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib chain::tx`
Expected: 12 passed.

- [ ] **Step 5: `make check`, then commit**

```bash
git add src/chain/tx.rs
git commit -m "feat(chain): send with one retry, bounded polling and outcome classification" -m "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 8: Pool reads and operation builders

**Files:**
- Modify: `src/chain/xdr/encode.rs` (`RequestType`, `Request`, `request`)
- Modify: `src/chain/xdr/mod.rs` (re-export `Request`, `RequestType`)
- Create: `src/chain/pool.rs`
- Modify: `src/chain/mod.rs` (declare `pub mod pool;`, re-export `PoolReader`, `PoolSnapshot`)

**Interfaces:**
- Consumes: `RpcClient::{ledger_entries, simulate}`; `chain::xdr::{decode, keys, encode}`; `math::{Reserve, Positions, OraclePrices, PositionData, calculate_position_data}`.
- Produces:
  - `pub enum RequestType { Supply, Withdraw, SupplyCollateral, WithdrawCollateral, Borrow, Repay, FillUserLiquidationAuction, FillBadDebtAuction, FillInterestAuction, DeleteLiquidationAuction }` with `code(self) -> u32` (0 to 9 in that order).
  - `pub struct Request { pub request_type: RequestType, pub address: String, pub amount: i128 }` and `pub fn request(request: &Request) -> Result<ScVal, XdrError>` (a struct map with keys `address`, `amount`, `request_type`).
  - `pub fn submit_op(pool, from, spender, to: &str, requests: &[Request]) -> Result<Operation, XdrError>`, `pub fn new_auction_op(pool: &str, auction_type: AuctionType, user: &str, bid: &[&str], lot: &[&str], percent: u32) -> Result<Operation, XdrError>`, `pub fn bad_debt_op(pool: &str, user: &str) -> Result<Operation, XdrError>`.
  - `pub struct PoolSnapshot { pub ledger: u32, pub pool: String, pub instance: PoolInstance, pub reserves: BTreeMap<u32, Reserve>, pub asset_index: BTreeMap<String, u32>, pub prices: OraclePrices, pub price_timestamps: BTreeMap<String, u64>, pub positions: BTreeMap<String, Positions> }` with `position_data(&self, user: &str, close_time: u64) -> Result<Option<PositionData>, ChainError>`.
  - `pub struct PoolReader<'a>` with `PoolReader::new(rpc: &'a RpcClient, pool: &str)`, `snapshot(&self, users: &[&str]) -> Result<PoolSnapshot, ChainError>`, `auction(&self, user: &str, auction_type: AuctionType) -> Result<Option<(u32, AuctionData)>, ChainError>`, `balance(&self, token: &str, account: &str) -> Result<(u32, i128), ChainError>`.

The contract facts (blend-contracts-v2 v2.0.0, `pool/src/contract.rs` and `pool/src/pool/actions.rs`): `submit(from: Address, spender: Address, to: Address, requests: Vec<Request>)`; `Request { request_type: u32, address: Address, amount: i128 }`; `new_auction(auction_type: u32, user: Address, bid: Vec<Address>, lot: Vec<Address>, percent: u32)`; `bad_debt(user: Address)`; a SEP-41 token's `balance(id: Address) -> i128`; the oracle's `decimals() -> u32` and `lastprice(asset: Asset) -> Option<PriceData>`.

- [ ] **Step 1: Write the failing encoding tests**

Append to the test module in `src/chain/xdr/encode.rs`:

```rust
    #[test]
    fn request_types_carry_the_contracts_numbers_in_order() {
        let all = [
            RequestType::Supply, RequestType::Withdraw, RequestType::SupplyCollateral,
            RequestType::WithdrawCollateral, RequestType::Borrow, RequestType::Repay,
            RequestType::FillUserLiquidationAuction, RequestType::FillBadDebtAuction,
            RequestType::FillInterestAuction, RequestType::DeleteLiquidationAuction,
        ];
        for (expected, request_type) in (0_u32..).zip(all) {
            assert_eq!(request_type.code(), expected);
        }
    }

    #[test]
    fn a_request_is_a_struct_map_with_sorted_keys() {
        let user = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
        let value = request(&Request {
            request_type: RequestType::FillUserLiquidationAuction,
            address: user.to_string(),
            amount: 60,
        })
        .expect("encodes");
        let ScVal::Map(Some(entries)) = value else { panic!("expected a map") };
        let keys: Vec<String> = entries
            .iter()
            .map(|entry| match &entry.key {
                ScVal::Symbol(symbol) => symbol.to_utf8_string_lossy(),
                other => panic!("non-symbol key {other:?}"),
            })
            .collect();
        assert_eq!(keys, ["address", "amount", "request_type"]);
        assert_eq!(entries[0].val, address(user).expect("address"));
        assert_eq!(entries[1].val, i128_val(60));
        assert_eq!(entries[2].val, ScVal::U32(6));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib chain::xdr::encode`
Expected: compile errors.

- [ ] **Step 3: Implement the request encoding**

Add to `src/chain/xdr/encode.rs`:

```rust
/// The pool's `Request.request_type` discriminants, in the contract's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestType {
    /// 0: supply to the pool (not as collateral).
    Supply,
    /// 1: withdraw supplied tokens.
    Withdraw,
    /// 2: supply as collateral.
    SupplyCollateral,
    /// 3: withdraw collateral.
    WithdrawCollateral,
    /// 4: borrow.
    Borrow,
    /// 5: repay.
    Repay,
    /// 6: fill a user liquidation auction; `address` is the liquidated
    /// user, `amount` the percent to fill, 1 to 100.
    FillUserLiquidationAuction,
    /// 7: fill a bad-debt auction.
    FillBadDebtAuction,
    /// 8: fill an interest auction.
    FillInterestAuction,
    /// 9: delete a liquidation auction whose user is healthy again.
    DeleteLiquidationAuction,
}

impl RequestType {
    /// The contract's number for this request type.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            Self::Supply => 0,
            Self::Withdraw => 1,
            Self::SupplyCollateral => 2,
            Self::WithdrawCollateral => 3,
            Self::Borrow => 4,
            Self::Repay => 5,
            Self::FillUserLiquidationAuction => 6,
            Self::FillBadDebtAuction => 7,
            Self::FillInterestAuction => 8,
            Self::DeleteLiquidationAuction => 9,
        }
    }
}

/// One entry of a `submit` call's request list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// What to do.
    pub request_type: RequestType,
    /// The asset for supply, withdraw, borrow and repay; the user for the
    /// auction requests.
    pub address: String,
    /// The amount in the asset's decimals, or the fill percent.
    pub amount: i128,
}

/// The contract's `Request` struct: a map keyed `address`, `amount`,
/// `request_type`, which is the order Soroban sorts the symbols into.
pub fn request(request: &Request) -> Result<ScVal, XdrError> {
    map(vec![
        (symbol("address")?, address(&request.address)?),
        (symbol("amount")?, i128_val(request.amount)),
        (symbol("request_type")?, ScVal::U32(request.request_type.code())),
    ])
}
```

In `src/chain/xdr/mod.rs` add `Request, RequestType` to the `encode` re-export list (and `request`).

- [ ] **Step 4: Write the failing pool tests**

Create `src/chain/pool.rs` with the module doc, stubs, and:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::script::{scval_b64, transaction_data_b64, ScriptedRpc};
    use crate::chain::xdr::encode::{
        i128_val, map, sc_address, symbol, to_base64, vec as sc_vec, RequestType,
    };
    use crate::chain::xdr::keys;
    use crate::fixture::{mainnet_fixed_v2, text};
    use serde_json::{json, Value};
    use stellar_xdr::{
        ContractDataDurability, ContractDataEntry, ExtensionPoint, HostFunction, LedgerEntryData,
        OperationBody, ScVal,
    };

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
    const FILLER: &str = "GCIH7OYRDHJ3IOPFEM7DMUX3SXTVHOO2XSWLGBMSVQ3EIHPHYUTNJ7OL";
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    fn invoke(operation: &Operation) -> (String, Vec<ScVal>) {
        let OperationBody::InvokeHostFunction(op) = &operation.body else { panic!("invoke") };
        let HostFunction::InvokeContract(args) = &op.host_function else { panic!("invoke contract") };
        (args.function_name.to_utf8_string_lossy(), args.args.to_vec())
    }

    #[test]
    fn submit_op_calls_submit_with_from_spender_to_and_the_requests() {
        let requests = [Request { request_type: RequestType::FillUserLiquidationAuction, address: USER.to_string(), amount: 60 }];
        let op = submit_op(POOL, FILLER, FILLER, FILLER, &requests).unwrap();
        let (function, args) = invoke(&op);
        assert_eq!(function, "submit");
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], address(FILLER).unwrap());
        assert_eq!(args[3], sc_vec(vec![request(&requests[0]).unwrap()]).unwrap());
    }

    #[test]
    fn new_auction_op_and_bad_debt_op_match_the_contract_signatures() {
        let op = new_auction_op(POOL, AuctionType::UserLiquidation, USER, &[USDC], &[], 50).unwrap();
        let (function, args) = invoke(&op);
        assert_eq!(function, "new_auction");
        assert_eq!(args[0], ScVal::U32(0));
        assert_eq!(args[1], address(USER).unwrap());
        assert_eq!(args[2], sc_vec(vec![address(USDC).unwrap()]).unwrap());
        assert_eq!(args[3], sc_vec(vec![]).unwrap());
        assert_eq!(args[4], ScVal::U32(50));
        let (function, args) = invoke(&bad_debt_op(POOL, USER).unwrap());
        assert_eq!((function.as_str(), args.len()), ("bad_debt", 1));
        assert!(new_auction_op(POOL, AuctionType::UserLiquidation, USER, &["not-an-address"], &[], 50).is_err());
    }

    fn entry(key: &stellar_xdr::LedgerKey, xdr: &str) -> Value {
        json!({"key": to_base64(key).unwrap(), "xdr": xdr, "lastModifiedLedgerSeq": 1, "liveUntilLedgerSeq": 99_999_999})
    }

    fn simulation(return_xdr: &str, ledger: u32) -> Value {
        json!({"transactionData": transaction_data_b64(1), "events": [], "minResourceFee": "1",
               "results": [{"auth": [], "xdr": return_xdr}], "latestLedger": ledger})
    }

    /// Scripts the fixture's ledger: the shape read, the full read, then the
    /// oracle's decimals and one lastprice per reserve in list order.
    fn script_fixture(rpc: &ScriptedRpc, fixture: &Value, second_ledger: u32) {
        let ledger = fixture["ledger"].as_u64().unwrap();
        rpc.expect("getLedgerEntries", json!({"latestLedger": ledger, "entries": [
            entry(&keys::instance(POOL).unwrap(), text(fixture, &["instance_entry_xdr"])),
            entry(&keys::reserve_list(POOL).unwrap(), text(fixture, &["res_list_entry_xdr"])),
        ]}));
        let mut entries = Vec::new();
        for reserve in fixture["reserves"].as_array().unwrap() {
            let asset = reserve["asset"].as_str().unwrap();
            entries.push(entry(&keys::reserve_config(POOL, asset).unwrap(), reserve["config_entry_xdr"].as_str().unwrap()));
            entries.push(entry(&keys::reserve_data(POOL, asset).unwrap(), reserve["data_entry_xdr"].as_str().unwrap()));
        }
        for user in fixture["users"].as_array().unwrap() {
            let account = user["account"].as_str().unwrap();
            entries.push(entry(&keys::positions(POOL, account).unwrap(), user["positions_entry_xdr"].as_str().unwrap()));
        }
        rpc.expect("getLedgerEntries", json!({"latestLedger": second_ledger, "entries": entries}));
        rpc.expect("simulateTransaction", simulation(text(fixture, &["oracle_decimals_return_xdr"]), second_ledger));
        for reserve in fixture["reserves"].as_array().unwrap() {
            rpc.expect("simulateTransaction", simulation(reserve["lastprice_return_xdr"].as_str().unwrap(), second_ledger));
        }
    }

    /// The payoff: read through the client, value through `math`, and land
    /// on the same golden health factors `chain::xdr::decode`'s test derives
    /// from the same attested inputs.
    #[tokio::test]
    async fn a_snapshot_of_the_fixture_ledger_values_its_users_to_the_golden_health_factors() {
        let fixture = mainnet_fixed_v2();
        let ledger = u32::try_from(fixture["ledger"].as_u64().unwrap()).unwrap();
        let close_time = fixture["ledger_close_time"].as_u64().unwrap();
        let rpc = ScriptedRpc::start().await;
        script_fixture(&rpc, &fixture, ledger);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let users: Vec<&str> = fixture["users"].as_array().unwrap().iter().map(|u| u["account"].as_str().unwrap()).collect();

        let snapshot = PoolReader::new(&client, POOL).snapshot(&users).await.unwrap();
        assert_eq!(snapshot.ledger, ledger);
        assert_eq!(snapshot.instance.config.oracle, fixture["oracle"].as_str().unwrap());
        assert_eq!(snapshot.reserves.len(), 3);
        assert_eq!(snapshot.asset_index.len(), 3);
        assert_eq!(snapshot.prices.decimals(), 7);
        assert_eq!(snapshot.price_timestamps.len(), 3);
        for reserve in snapshot.reserves.values() {
            assert!(snapshot.prices.price(&reserve.asset).unwrap() > 0);
            assert_eq!(snapshot.asset_index[&reserve.asset], reserve.config.index);
        }
        let first = snapshot.position_data(users[0], close_time).unwrap().unwrap();
        assert_eq!(first.health_factor(), Ok(Some(10_070_767)));
        let second = snapshot.position_data(users[1], close_time).unwrap().unwrap();
        assert_eq!(second.health_factor(), Ok(Some(10_100_345)));
        assert!(snapshot.position_data("GA…unknown", close_time).unwrap().is_none());
        // The oracle was asked for decimals, then one lastprice per reserve, in list order.
        let simulations = rpc.calls("simulateTransaction");
        assert_eq!(simulations.len(), 4);
        assert_eq!(rpc.remaining(), 0);
    }

    #[tokio::test]
    async fn a_snapshot_refuses_a_ledger_that_moved_between_reads() {
        let fixture = mainnet_fixed_v2();
        let ledger = u32::try_from(fixture["ledger"].as_u64().unwrap()).unwrap();
        let rpc = ScriptedRpc::start().await;
        script_fixture(&rpc, &fixture, ledger + 1);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let error = PoolReader::new(&client, POOL).snapshot(&[USER]).await.unwrap_err();
        assert!(matches!(error, ChainError::LedgerMoved { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_user_without_a_positions_entry_has_empty_positions() {
        let fixture = mainnet_fixed_v2();
        let ledger = u32::try_from(fixture["ledger"].as_u64().unwrap()).unwrap();
        let rpc = ScriptedRpc::start().await;
        // Script the fixture's users, then ask for a third the ledger does not hold.
        script_fixture(&rpc, &fixture, ledger);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let mut users: Vec<&str> = fixture["users"].as_array().unwrap().iter().map(|u| u["account"].as_str().unwrap()).collect();
        users.push(FILLER);
        let snapshot = PoolReader::new(&client, POOL).snapshot(&users).await.unwrap();
        assert!(snapshot.positions[FILLER].is_empty());
        assert!(snapshot.position_data(FILLER, 1).unwrap().is_none());
    }

    fn auction_entry_xdr() -> String {
        let side = |amount: i128| map(vec![(address(USDC).unwrap(), i128_val(amount))]).unwrap();
        let auction = map(vec![
            (symbol("bid").unwrap(), side(1_000)),
            (symbol("block").unwrap(), ScVal::U32(64_271_300)),
            (symbol("lot").unwrap(), side(2_000)),
        ])
        .unwrap();
        let auction_key = map(vec![(symbol("auct_type").unwrap(), ScVal::U32(0)), (symbol("user").unwrap(), address(USER).unwrap())]).unwrap();
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(POOL).unwrap(),
            key: sc_vec(vec![symbol("Auction").unwrap(), auction_key]).unwrap(),
            durability: ContractDataDurability::Temporary,
            val: auction,
        });
        to_base64(&entry).unwrap()
    }

    #[tokio::test]
    async fn an_auction_is_read_from_temporary_storage_and_absent_is_none() {
        let key = keys::auction(POOL, USER, AuctionType::UserLiquidation).unwrap();
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getLedgerEntries", json!({"latestLedger": 5, "entries": [entry(&key, &auction_entry_xdr())]}));
        rpc.expect("getLedgerEntries", json!({"latestLedger": 6, "entries": []}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let reader = PoolReader::new(&client, POOL);
        let (ledger, auction) = reader.auction(USER, AuctionType::UserLiquidation).await.unwrap().unwrap();
        assert_eq!(ledger, 5);
        assert_eq!(auction.block, 64_271_300);
        assert_eq!(auction.bid[USDC], 1_000);
        assert_eq!(auction.lot[USDC], 2_000);
        assert!(reader.auction(USER, AuctionType::UserLiquidation).await.unwrap().is_none());
        assert_eq!(rpc.calls("getLedgerEntries")[0]["keys"][0], to_base64(&key).unwrap());
    }

    #[tokio::test]
    async fn a_balance_is_simulated_on_the_token() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect("simulateTransaction", simulation(&scval_b64(&i128_val(123_456_789)), 9));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        assert_eq!(PoolReader::new(&client, POOL).balance(USDC, FILLER).await.unwrap(), (9, 123_456_789));
        let envelope: stellar_xdr::TransactionEnvelope =
            crate::chain::xdr::encode::from_base64(rpc.calls("simulateTransaction")[0]["transaction"].as_str().unwrap()).unwrap();
        let stellar_xdr::TransactionEnvelope::Tx(v1) = envelope else { panic!("v1") };
        let (function, args) = invoke(&v1.tx.operations[0]);
        assert_eq!((function.as_str(), args.len()), ("balance", 1));
    }
}
```

`USDC` is the fixture's USDC reserve address; confirm it against `reserves[1].asset` in `tests/fixtures/mainnet-fixed-v2.json` (the reserves are XLM, USDC, EURC in that order) and use the fixture's string verbatim.

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test --lib chain::pool`
Expected: compile errors.

- [ ] **Step 6: Implement the pool module**

`src/chain/pool.rs`:

```rust
//! Pool reads and the pool's three operations.
//!
//! A `PoolSnapshot` describes exactly one ledger: the reader takes the
//! instance and reserve list, then every reserve and requested user in one
//! batched read, then the oracle's decimals and prices by simulation, and
//! refuses the result if any of those reported a different `latestLedger`.
//! The snapshot holds reserves as stored; `position_data` accrues a copy to
//! a close time before valuing, the way the contract does inside a call.

use std::collections::BTreeMap;

use stellar_xdr::{Operation, ScVal};

use crate::chain::rpc::{RpcClient, SimulationOutcome};
use crate::chain::xdr::decode::{self, PoolInstance};
use crate::chain::xdr::encode::{
    address, invoke_contract_op, request, simulation_envelope, stellar_asset, vec as sc_vec,
    Request,
};
use crate::chain::xdr::{keys, AuctionType, XdrError};
use crate::chain::ChainError;
use crate::math::{
    calculate_position_data, AuctionData, OraclePrices, PositionData, Positions, Reserve,
};

/// `submit(from, spender, to, requests)`.
pub fn submit_op(
    pool: &str,
    from: &str,
    spender: &str,
    to: &str,
    requests: &[Request],
) -> Result<Operation, XdrError> {
    let requests = requests.iter().map(request).collect::<Result<Vec<_>, _>>()?;
    invoke_contract_op(
        pool,
        "submit",
        vec![address(from)?, address(spender)?, address(to)?, sc_vec(requests)?],
    )
}

/// `new_auction(auction_type, user, bid, lot, percent)`.
pub fn new_auction_op(
    pool: &str,
    auction_type: AuctionType,
    user: &str,
    bid: &[&str],
    lot: &[&str],
    percent: u32,
) -> Result<Operation, XdrError> {
    let addresses = |assets: &[&str]| {
        assets
            .iter()
            .map(|asset| address(asset))
            .collect::<Result<Vec<_>, _>>()
            .and_then(sc_vec)
    };
    invoke_contract_op(
        pool,
        "new_auction",
        vec![
            ScVal::U32(auction_type.code()),
            address(user)?,
            addresses(bid)?,
            addresses(lot)?,
            ScVal::U32(percent),
        ],
    )
}

/// `bad_debt(user)`.
pub fn bad_debt_op(pool: &str, user: &str) -> Result<Operation, XdrError> {
    invoke_contract_op(pool, "bad_debt", vec![address(user)?])
}

/// One ledger's view of a pool.
#[derive(Debug, Clone)]
pub struct PoolSnapshot {
    /// The ledger every field describes.
    pub ledger: u32,
    /// The pool contract.
    pub pool: String,
    /// Instance storage: admin, backstop, config.
    pub instance: PoolInstance,
    /// Reserves keyed by `config.index`, the key `Positions` uses, as
    /// stored — not yet accrued.
    pub reserves: BTreeMap<u32, Reserve>,
    /// Asset address to reserve index.
    pub asset_index: BTreeMap<String, u32>,
    /// The oracle's prices for every reserve that had one.
    pub prices: OraclePrices,
    /// When the oracle last updated each price, unix seconds, so a caller
    /// can judge staleness against the tick.
    pub price_timestamps: BTreeMap<String, u64>,
    /// Each requested user's positions; empty when the ledger holds none.
    pub positions: BTreeMap<String, Positions>,
}

impl PoolSnapshot {
    /// Values `user`'s positions at `close_time`: accrues a copy of the
    /// reserves to it with the pool's backstop rate, then computes the
    /// effective and raw totals. `None` when the user was not requested or
    /// holds no positions.
    pub fn position_data(&self, user: &str, close_time: u64) -> Result<Option<PositionData>, ChainError> {
        let Some(positions) = self.positions.get(user) else {
            return Ok(None);
        };
        if positions.is_empty() {
            return Ok(None);
        }
        let mut reserves = self.reserves.clone();
        for reserve in reserves.values_mut() {
            reserve.accrue(self.instance.config.bstop_rate, close_time)?;
        }
        Ok(Some(calculate_position_data(&reserves, &self.prices, positions)?))
    }
}

/// Reads one pool through an `RpcClient`.
#[derive(Debug, Clone, Copy)]
pub struct PoolReader<'a> {
    rpc: &'a RpcClient,
    pool: &'a str,
}

fn same_ledger(expected: u32, actual: u32) -> Result<(), ChainError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ChainError::LedgerMoved {
            first: expected,
            second: actual,
        })
    }
}

impl<'a> PoolReader<'a> {
    /// A reader for `pool`.
    #[must_use]
    pub fn new(rpc: &'a RpcClient, pool: &'a str) -> Self {
        Self { rpc, pool }
    }

    /// Simulates a view call and returns `(latest ledger, return value)`.
    async fn view(&self, contract: &str, function: &str, args: Vec<ScVal>) -> Result<(u32, ScVal), ChainError> {
        let envelope = simulation_envelope(invoke_contract_op(contract, function, args)?)?;
        let simulation = self.rpc.simulate(&envelope).await?;
        match simulation.outcome {
            SimulationOutcome::Success(call) => Ok((simulation.latest_ledger, call.return_value)),
            SimulationOutcome::Failure {
                message,
                contract_error,
            } => Err(ChainError::Simulation {
                message,
                contract_error,
            }),
        }
    }

    /// The instance and reserve list, and the ledger they came from.
    async fn shape(&self) -> Result<(u32, PoolInstance, Vec<String>), ChainError> {
        let instance_key = keys::instance(self.pool)?;
        let list_key = keys::reserve_list(self.pool)?;
        let entries = self.rpc.ledger_entries(&[instance_key.clone(), list_key.clone()]).await?;
        let missing = |what: &str| ChainError::Shape(format!("pool {} has no {what} entry", self.pool));
        let instance = decode::pool_instance(&entries.get(&instance_key)?.ok_or_else(|| missing("instance"))?.data)?;
        let assets = decode::reserve_list(&entries.get(&list_key)?.ok_or_else(|| missing("reserve list"))?.data)?;
        Ok((entries.latest_ledger, instance, assets))
    }

    /// The oracle's decimals and one price per asset, all at `ledger`.
    async fn prices(
        &self,
        oracle: &str,
        assets: &[String],
        ledger: u32,
    ) -> Result<(OraclePrices, BTreeMap<String, u64>), ChainError> {
        let (at, decimals) = self.view(oracle, "decimals", Vec::new()).await?;
        same_ledger(ledger, at)?;
        let decimals = decode::decimals(&decimals)?;
        let mut prices = BTreeMap::new();
        let mut timestamps = BTreeMap::new();
        for asset in assets {
            let (at, value) = self.view(oracle, "lastprice", vec![stellar_asset(asset)?]).await?;
            same_ledger(ledger, at)?;
            match decode::price_data(&value)? {
                Some(price) => {
                    prices.insert(asset.clone(), price.price);
                    timestamps.insert(asset.clone(), price.timestamp);
                }
                None => tracing::warn!(pool = self.pool, asset = %asset, "the oracle has no price; the asset is unpriced"),
            }
        }
        Ok((OraclePrices::new(decimals, prices)?, timestamps))
    }

    /// One ledger's view of the pool for `users`.
    pub async fn snapshot(&self, users: &[&str]) -> Result<PoolSnapshot, ChainError> {
        let (ledger, instance, assets) = self.shape().await?;
        let mut wanted = Vec::with_capacity(assets.len() * 2 + users.len());
        for asset in &assets {
            wanted.push(keys::reserve_config(self.pool, asset)?);
            wanted.push(keys::reserve_data(self.pool, asset)?);
        }
        for user in users {
            wanted.push(keys::positions(self.pool, user)?);
        }
        let entries = self.rpc.ledger_entries(&wanted).await?;
        same_ledger(ledger, entries.latest_ledger)?;

        let mut reserves = BTreeMap::new();
        let mut asset_index = BTreeMap::new();
        for asset in &assets {
            let missing = |what: &str| ChainError::Shape(format!("reserve {asset} has no {what} entry"));
            let config = decode::reserve_config(
                &entries.get(&keys::reserve_config(self.pool, asset)?)?.ok_or_else(|| missing("config"))?.data,
            )?;
            let data = decode::reserve_data(
                &entries.get(&keys::reserve_data(self.pool, asset)?)?.ok_or_else(|| missing("data"))?.data,
            )?;
            let reserve = Reserve::new(asset.clone(), config, data)?;
            asset_index.insert(asset.clone(), reserve.config.index);
            reserves.insert(reserve.config.index, reserve);
        }

        let mut positions = BTreeMap::new();
        for user in users {
            let entry = entries.get(&keys::positions(self.pool, user)?)?;
            let user_positions = match entry {
                Some(entry) => decode::positions(&entry.data)?,
                None => Positions::default(),
            };
            positions.insert((*user).to_string(), user_positions);
        }

        let (prices, price_timestamps) = self.prices(&instance.config.oracle, &assets, ledger).await?;
        Ok(PoolSnapshot {
            ledger,
            pool: self.pool.to_string(),
            instance,
            reserves,
            asset_index,
            prices,
            price_timestamps,
            positions,
        })
    }

    /// The user's open auction of that type, from temporary storage, with
    /// the ledger it was read at. `None` is "no auction", which is also what
    /// an expired temporary entry looks like.
    pub async fn auction(&self, user: &str, auction_type: AuctionType) -> Result<Option<(u32, AuctionData)>, ChainError> {
        let key = keys::auction(self.pool, user, auction_type)?;
        let entries = self.rpc.ledger_entries(std::slice::from_ref(&key)).await?;
        match entries.get(&key)? {
            Some(entry) => Ok(Some((entries.latest_ledger, decode::auction(&entry.data)?))),
            None => Ok(None),
        }
    }

    /// `account`'s balance of `token`, by simulating the token's `balance`,
    /// with the ledger it was read at.
    pub async fn balance(&self, token: &str, account: &str) -> Result<(u32, i128), ChainError> {
        let (ledger, value) = self.view(token, "balance", vec![address(account)?]).await?;
        match value {
            ScVal::I128(parts) => Ok((ledger, i128::from(parts))),
            other => Err(ChainError::Xdr(XdrError::Shape {
                expected: "i128 balance",
                got: format!("{other:?}"),
            })),
        }
    }
}
```

`i128::from(Int128Parts)`: `stellar_xdr` implements `From<Int128Parts> for i128`; `decode.rs` already has an `as_i128` helper for the same conversion — reuse it if it is `pub(crate)`, else make it so. If `snapshot` exceeds clippy's line limit, move the reserve loop into `fn reserves_from(entries, pool, assets)` and the positions loop into `fn positions_from(entries, pool, users)`.

In `src/chain/mod.rs` add `pub mod pool;` and `pub use pool::{PoolReader, PoolSnapshot};`.

- [ ] **Step 7: Run the tests**

Run: `cargo test --lib chain::pool chain::xdr::encode`
Expected: all pass; the snapshot test lands on the golden health factors 10_070_767 and 10_100_345.

- [ ] **Step 8: `make check`, then commit**

```bash
git add src/chain/mod.rs src/chain/pool.rs src/chain/xdr/encode.rs src/chain/xdr/mod.rs
git commit -m "feat(chain): pool snapshot, auction and balance reads, and the pool operations" -m "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 9: The live-snapshot example and the documentation

**Files:**
- Create: `examples/pool_snapshot.rs`
- Modify: `CLAUDE.md`, `CHANGELOG.md`, `src/chain/mod.rs` (module doc)

- [ ] **Step 1: Write the example**

`examples/pool_snapshot.rs`:

```rust
//! Prints a pool's reserves and, for each user given, the user's health
//! factor, read from a live RPC through the real client. The phase's
//! dry-run demonstration: nothing is signed or sent.
//!
//! ```text
//! RPC_URL=https://mainnet.sorobanrpc.com \
//!   cargo run --example pool_snapshot -- <pool> [user...]
//! ```
//!
//! `RPC_API_KEY_HEADER` and `RPC_API_KEY` are honoured together. The health
//! factor is computed at the latest ledger's close time, which may be one or
//! two ledgers after the snapshot's: that is the accrual the contract would
//! apply to a call landing now.

use std::error::Error;

use blend_liquidator::chain::pool::PoolReader;
use blend_liquidator::chain::rpc::RpcClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    let pool = arguments
        .get(1)
        .ok_or("usage: pool_snapshot <pool> [user...]")?;
    let users: Vec<&str> = arguments.iter().skip(2).map(String::as_str).collect();
    let url = std::env::var("RPC_URL").map_err(|_| "RPC_URL is required")?;
    let header = std::env::var("RPC_API_KEY_HEADER").ok();
    let key = std::env::var("RPC_API_KEY").ok();
    let api_key = match (&header, &key) {
        (Some(header), Some(key)) => Some((header.as_str(), key.as_str())),
        (None, None) => None,
        _ => return Err("RPC_API_KEY_HEADER and RPC_API_KEY come together".into()),
    };

    let client = RpcClient::new(&url, api_key)?;
    let latest = client.latest_ledger().await?;
    let snapshot = PoolReader::new(&client, pool).snapshot(&users).await?;

    println!(
        "pool {pool} at ledger {} (latest {} closed at {})",
        snapshot.ledger, latest.sequence, latest.close_time
    );
    println!(
        "  status {:?}, backstop rate {}, oracle {} with {} decimals",
        snapshot.instance.config.status,
        snapshot.instance.config.bstop_rate,
        snapshot.instance.config.oracle,
        snapshot.prices.decimals()
    );
    for reserve in snapshot.reserves.values() {
        let price = snapshot
            .prices
            .price(&reserve.asset)
            .map_or_else(|_| "none".to_string(), |price| price.to_string());
        println!(
            "  reserve {} index {} utilisation {} b_rate {} d_rate {} price {price}",
            reserve.asset,
            reserve.config.index,
            reserve.utilization()?,
            reserve.data.b_rate,
            reserve.data.d_rate
        );
    }
    for user in users {
        match snapshot.position_data(user, latest.close_time)? {
            Some(data) => println!(
                "  user {user}: collateral {} liabilities {} health factor {:?}",
                data.collateral_base,
                data.liability_base,
                data.health_factor()?
            ),
            None => println!("  user {user}: no positions"),
        }
    }
    Ok(())
}
```

`reserve.config.index`, `reserve.data.b_rate`, `reserve.data.d_rate`, `data.collateral_base`, `data.liability_base` are the Phase 1 field names; if a name differs, use the one `src/math` defines.

- [ ] **Step 2: Run it against mainnet and keep the output**

Run:

```bash
RPC_URL=https://mainnet.sorobanrpc.com cargo run --example pool_snapshot -- \
  CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD \
  GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE
```

Expected: the pool's three reserves with prices, and a health factor for the user (or "no positions" if the position has since closed). Paste the output into the task report; it goes into the pull request as the phase's dry-run excerpt.

- [ ] **Step 3: Update `CLAUDE.md`**

In the Status paragraph, replace the sentence about Phase 1 with: "Phase 1 landed the pure fixed-point math (`math`) and the ScVal/ledger-entry codecs (`chain::xdr`); Phase 2 landed the chain layer (`chain::rpc`, `chain::pool`, `chain::signer`, `chain::tx`), which can read a pool and sign and submit a transaction but is not yet driven by anything." Keep the next sentence about the binary still parsing configuration and exiting.

In the module map, after the `src/chain/xdr/` entry, add:

```markdown
- `src/chain/rpc.rs` — the Soroban JSON-RPC client: the eight methods the
  bot uses, their wire shapes, base64 XDR decoded at the boundary. Every
  result carries the ledger it was taken at.
- `src/chain/pool.rs` — pool reads: a single-ledger `PoolSnapshot` (instance,
  reserves, prices, positions) that `position_data` values with `math`,
  auction and balance reads, and the `submit`, `new_auction` and `bad_debt`
  operation builders.
- `src/chain/signer.rs` — the network id and the Ed25519 key. `Signer`
  renders as its address only.
- `src/chain/tx.rs` — the one write path: build with time and ledger bounds,
  simulate, restore archived entries, assemble, fee, sign, send, poll,
  classify into `TxOutcome`.
- `src/chain/script.rs` (`cfg(test)`) — a scripted JSON-RPC server the chain
  tests drive the real client through.
- `examples/pool_snapshot.rs` — prints a live pool's reserves and users'
  health factors.
```

and shorten the "phases still to land" sentence to: the store and ledger poller, the auctioneer, the filler and executor, unwind, and the operational surface.

In Gotchas, replace "The current dependency tree is small enough not to hit this. If you add a heavy stack, cap it:" with "Since Phase 2 the tree includes `reqwest`, `rustls` and `hyper`, and a cold build in a small container does hit this. Cap it:" and keep the command. Add two gotchas at the end:

```markdown
- `reqwest` is pinned to 0.12 with `rustls-tls-native-roots` and no default
  features on purpose: that feature set is the one whose licence tree
  `cargo deny` accepts. `rustls-tls` pulls `webpki-roots` (CDLA-Permissive)
  and 0.13's `rustls` feature goes through `aws-lc-rs` (OpenSSL licence);
  neither is in `deny.toml`, and adding them there is a licence decision,
  not a build fix.
- `getLedgerEntries` omits absent keys rather than returning nulls, so a
  lookup must go by key, never by position, and "the RPC returned fewer
  entries than keys" is the normal shape of "some of these do not exist".
```

- [ ] **Step 4: Update `CHANGELOG.md`**

Under `## [Unreleased]` → `### Added`, after the Phase 1 entries:

```markdown
- The chain layer (`src/chain/`): a hand-written Soroban JSON-RPC client
  (`rpc`) for the eight methods the bot uses, with every base64 XDR field
  decoded at the boundary and every result carrying its ledger; pool reads
  (`pool`) that assemble one ledger's instance, reserves, oracle prices and
  positions into a `PoolSnapshot` and refuse a ledger that moved between
  reads; the network id and an Ed25519 `Signer` that renders as its address
  only (`signer`); and the one write path (`tx`): build with a five-minute
  time bound and a ledger bound of `TX_POLL_LEDGERS`, simulate, restore
  archived entries, assemble, fee from the p70/p90 inclusion percentiles
  floored at `BASE_FEE`/`HIGH_FEE`, sign, send with one `TRY_AGAIN_LATER`
  retry, poll, and classify into succeeded, failed with the pool's error
  code, expired (provably never included) or unknown. A `TxBadSeq` at send
  is its own error, `BadSequence`, because the plan behind such a
  transaction is stale and must be rebuilt.
- Configuration for the chain: `NETWORK` or `NETWORK_PASSPHRASE`,
  `RPC_URL`, `RPC_API_KEY_HEADER` with `RPC_API_KEY` read from the
  environment only, `BASE_FEE`, `HIGH_FEE`, `TX_POLL_LEDGERS`.
- `cargo run --example pool_snapshot` prints a live pool's reserves and its
  users' health factors through the real client.
- Every chain test drives the real client through a scripted localhost
  JSON-RPC server (`src/chain/script.rs`), covering the restore,
  `TRY_AGAIN_LATER`, timeout and decoded-error paths without a network.
```

- [ ] **Step 5: `make check`, then commit**

```bash
git add examples/pool_snapshot.rs CLAUDE.md CHANGELOG.md src/chain/mod.rs
git commit -m "docs: live pool snapshot example, module map and changelog for phase 2" -m "Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

## Phase checklist

- [ ] Every task's `make check` green; `cargo deny check` green.
- [ ] The scripted-server tests cover restore, `TRY_AGAIN_LATER`, timeout (`Unknown`), expiry, and decoded contract errors, as section 9 requires.
- [ ] The pool snapshot test lands on the golden health factors that `chain::xdr::decode` derives from the same fixture.
- [ ] `pool_snapshot` output against mainnet is in the pull request.
- [ ] The pull request is opened against `main` with a summary of the rulings above.
