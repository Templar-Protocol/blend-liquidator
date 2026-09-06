//! The one write path: build, simulate, restore, assemble, fee, sign, send,
//! poll, classify — section 3 of the design spec.
//!
//! Every transaction carries two bounds. The time bound (now + 5 minutes)
//! is the network's convention; the ledger bounds — `min_ledger`, the
//! ledger it was built against, and `max_ledger`, `latest_ledger +
//! poll_ledgers + 1` and exclusive — are what make the outcome decidable:
//! once the RPC's `latestLedger` reaches `max_ledger`, a transaction the RPC
//! has not seen can never be applied. That alone is not proof, though: a
//! `NOT_FOUND` only means the RPC did not find it among the ledgers it
//! still retains, so `wait` reports `Expired` — provably not included —
//! only when the RPC's retention also still reaches back to `min_ledger`.
//! `Unknown` is kept both for the case where the RPC could not answer for
//! the whole window, and for the case where it answered but its retention
//! had already moved past `min_ledger`, so a `NOT_FOUND` proved nothing.
//!
//! This layer never consults `DRY_RUN`: nothing here should be called by a
//! dry-run path. The decision to sign and submit at all belongs to the
//! executor that owns it.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use stellar_xdr::{
    Duration as XdrDuration, ExtensionPoint, LedgerBounds, Memo, Operation, OperationBody,
    Preconditions, PreconditionsV2, RestoreFootprintOp, ScVal, SequenceNumber, TimeBounds,
    TimePoint, Transaction, TransactionEnvelope, TransactionExt, TransactionResult,
    TransactionResultResult, TransactionV1Envelope, VecM,
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
            // `Duration` holds up to 2^64 seconds and `poll_ledgers` is a
            // `u32` operator knob, so `10 * (poll_ledgers + 1)` seconds
            // never overflows either the multiplication or the `Duration`.
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

/// The ledgers a transaction may be applied in: `min_ledger` inclusive (the
/// ledger it was built against) to `max_ledger` exclusive (CAP-21). Built
/// only through `try_new`, so a window is never empty or inverted, and an
/// outcome is never classified against bounds that could not have been
/// signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerWindow {
    min_ledger: u32,
    max_ledger: u32,
}

impl LedgerWindow {
    /// A window from `min_ledger` (inclusive) to `max_ledger` (exclusive).
    ///
    /// # Errors
    ///
    /// `ChainError::Config` when `min_ledger >= max_ledger`: such a window
    /// is empty or inverted and could never have been signed into a
    /// transaction's `LedgerBounds`.
    pub fn try_new(min_ledger: u32, max_ledger: u32) -> Result<Self, ChainError> {
        if min_ledger < max_ledger {
            Ok(Self {
                min_ledger,
                max_ledger,
            })
        } else {
            Err(ChainError::Config(
                "a ledger window needs min_ledger < max_ledger",
            ))
        }
    }

    /// The ledger the transaction was built against; inclusive.
    #[must_use]
    pub fn min_ledger(self) -> u32 {
        self.min_ledger
    }

    /// The ledger after which the transaction can no longer be applied;
    /// exclusive.
    #[must_use]
    pub fn max_ledger(self) -> u32 {
        self.max_ledger
    }
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
    /// The ledgers this may be applied in: `min_ledger` is `latest_ledger`
    /// at prepare or restore time, so the transaction cannot be applied
    /// before it, and a `NOT_FOUND` only proves the transaction expired
    /// when the RPC's retention still reaches back that far.
    pub window: LedgerWindow,
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
    /// The chain passed the ledger bound without applying it, and the RPC's
    /// retention still reached back to `window`'s `min_ledger`, so the
    /// `NOT_FOUND` is proof, not merely the RPC having forgotten the window:
    /// it never will apply. A retry with a fresh sequence number is safe.
    Expired {
        /// The transaction.
        hash: TxHash,
        /// The window it missed.
        window: LedgerWindow,
        /// The ledger the RPC had when that became certain.
        latest_ledger: u32,
    },
    /// The RPC could not say within the window — either it never answered
    /// in time, it answered `NOT_FOUND` after its retention had already
    /// moved past the transaction's `min_ledger` (which proves nothing), or
    /// a permanent error meant polling could not continue at all. The
    /// transaction may still land; the caller keeps polling `getTransaction`
    /// for `hash` until it does or the chain passes `window`'s `max_ledger`.
    /// When the RPC's retention has moved past the whole window,
    /// reconciliation needs another source: the account's sequence number —
    /// if it has passed `sequence`, this transaction or another one
    /// consumed it.
    Unknown {
        /// The transaction.
        hash: TxHash,
        /// The sequence it would consume if it lands.
        sequence: i64,
        /// The window it was built against.
        window: LedgerWindow,
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
    pub fn new(
        rpc: &'a RpcClient,
        network: &'a Network,
        signer: &'a Signer,
        config: TxConfig,
    ) -> Self {
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

    /// An unsigned transaction with both ledger bounds: `min_ledger` is the
    /// ledger this was built against — `latest_ledger` at build time — so
    /// the transaction cannot be applied before the ledger it was built
    /// from; `max_ledger` is exclusive. Returns the window alongside the
    /// transaction.
    fn unsigned(
        &self,
        operation: Operation,
        sequence: i64,
        latest_ledger: u32,
        fee: u32,
    ) -> Result<(Transaction, LedgerWindow), ChainError> {
        let now = unix_now()?;
        let max_ledger = latest_ledger
            .checked_add(self.config.poll_ledgers)
            .and_then(|ledger| ledger.checked_add(1))
            .ok_or(ChainError::Config("the ledger bound overflows u32"))?;
        // `max_ledger = latest_ledger + poll_ledgers + 1 > latest_ledger`
        // whenever the checked additions above succeeded, so `try_new` can
        // never fail here; it is used anyway, since it is the only way to
        // build a `LedgerWindow`, and the `?` costs nothing.
        let window = LedgerWindow::try_new(latest_ledger, max_ledger)?;
        let tx = Transaction {
            source_account: self.signer.muxed(),
            fee,
            seq_num: SequenceNumber(sequence),
            cond: Preconditions::V2(PreconditionsV2 {
                time_bounds: Some(TimeBounds {
                    min_time: TimePoint(0),
                    // `saturating_add` never actually saturates here: that
                    // would need the system clock to read within 300
                    // seconds of `u64::MAX`, i.e. the year 584942417355.
                    max_time: TimePoint(now.saturating_add(TIME_BOUND_SECS)),
                }),
                ledger_bounds: Some(LedgerBounds {
                    min_ledger: window.min_ledger(),
                    max_ledger: window.max_ledger(),
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
        Ok((tx, window))
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
            SimulationOutcome::Success(call) => Ok((*call, simulation.latest_ledger)),
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
    fn assemble(
        mut tx: Transaction,
        call: &SimulatedCall,
        inclusion: u32,
    ) -> Result<Transaction, ChainError> {
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
        let (mut tx, window) = self.unsigned(operation, sequence, latest_ledger, fee)?;
        tx.ext = TransactionExt::V1(preamble.transaction_data);
        let envelope = self.signer.sign(&tx, self.network)?;
        let hash = TxHash(envelope.hash(self.network.id).map_err(XdrError::Xdr)?);
        let prepared = Prepared {
            envelope,
            hash,
            sequence,
            window,
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
    pub async fn prepare(
        &self,
        operation: Operation,
        priority: Priority,
    ) -> Result<Prepared, ChainError> {
        let account = self.rpc.account(self.signer.address()).await?;
        let fees = self.rpc.fee_stats().await?;
        let inclusion = self.inclusion_fee(&fees, priority);
        let mut sequence = account
            .sequence
            .checked_add(1)
            .ok_or_else(|| ChainError::Shape("the account sequence overflows i64".to_string()))?;
        let (mut call, mut latest_ledger) = self
            .simulate_call(&operation, sequence, account.latest_ledger)
            .await?;
        if let Some(preamble) = call.restore.take() {
            self.restore(preamble, sequence, latest_ledger, inclusion)
                .await?;
            sequence = sequence.checked_add(1).ok_or_else(|| {
                ChainError::Shape("the account sequence overflows i64".to_string())
            })?;
            (call, latest_ledger) = self
                .simulate_call(&operation, sequence, latest_ledger)
                .await?;
            if call.restore.is_some() {
                return Err(ChainError::Restore(
                    "archived entries remain after a restore".to_string(),
                ));
            }
        }
        let (tx, window) = self.unsigned(operation, sequence, latest_ledger, 0)?;
        let tx = Self::assemble(tx, &call, inclusion)?;
        let envelope = self.signer.sign(&tx, self.network)?;
        let hash = TxHash(envelope.hash(self.network.id).map_err(XdrError::Xdr)?);
        Ok(Prepared {
            envelope,
            hash,
            sequence,
            window,
            fee: tx.fee,
            resource_fee: call.min_resource_fee,
        })
    }
}

/// What a `getTransaction` answer means for a transaction identified by
/// `hash` and bounded by `window`: a terminal outcome, or `None` while the
/// transaction may still land. Two things must both hold for a `NotFound`
/// to become `Expired`: the chain must have passed `window`'s upper bound
/// (`latest_ledger >= window.max_ledger()`), and the RPC's retention must
/// still reach back to its lower bound (`oldest_ledger <=
/// window.min_ledger()`) — otherwise the `NOT_FOUND` only reflects that the
/// RPC no longer holds the ledgers in question, not that the transaction
/// never applied, so this returns `None` and the caller keeps polling until
/// the wait cap yields `Unknown`. Takes the hash and window rather than a
/// whole `Prepared` so a caller resuming an `Unknown` outcome — which
/// carries only those fields, not a `Prepared` — can call it too.
#[must_use]
pub fn classify(
    status: TransactionStatus,
    hash: TxHash,
    window: LedgerWindow,
) -> Option<TxOutcome> {
    match status {
        TransactionStatus::Success {
            ledger,
            return_value,
            ..
        } => Some(TxOutcome::Succeeded {
            hash,
            ledger,
            return_value,
        }),
        TransactionStatus::Failed {
            ledger,
            result,
            contract_error,
            ..
        } => Some(TxOutcome::Failed {
            hash,
            ledger,
            contract_error,
            result,
        }),
        TransactionStatus::NotFound {
            latest_ledger,
            oldest_ledger,
        } if latest_ledger >= window.max_ledger() && oldest_ledger <= window.min_ledger() => {
            Some(TxOutcome::Expired {
                hash,
                window,
                latest_ledger,
            })
        }
        TransactionStatus::NotFound { .. } => None,
    }
}

/// Whether `error` is worth retrying while polling for a transaction's
/// outcome: a transport failure or an HTTP 429/5xx status can plausibly
/// resolve on the next poll of the same query. Anything else — a JSON-RPC
/// error object, an unreadable response body, or any other HTTP status —
/// means the request itself was rejected, and polling again cannot change
/// that, so `wait_for` stops polling immediately and reports `Unknown`
/// rather than retrying toward one at the wait cap.
fn is_transient(error: &ChainError) -> bool {
    matches!(
        error,
        ChainError::Transport(_) | ChainError::Http(429 | 500..=599)
    )
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

    /// This is how a queue resumes an `Unknown` outcome from the hash,
    /// sequence and window it recorded before sending, after a restart or a
    /// send that timed out — the fields `Unknown` carries and `Prepared`
    /// does not outlive.
    ///
    /// Polls `getTransaction` until the outcome is terminal, the chain has
    /// passed the ledger bound and the RPC's retention still proves it
    /// (`Expired`), or the wait cap passes without a provable answer
    /// (`Unknown`, carrying the same fields for a later resumption). Only a
    /// transient failure — a transport error, or an HTTP 429/5xx status — is
    /// retried while polling; a permanent one — a JSON-RPC error object, a
    /// malformed response or any other HTTP status — stops polling right
    /// away and is reported as `Unknown` rather than propagated as an
    /// error, because only the chain itself can say what became of a
    /// transaction that may already have been sent, and polling the same
    /// query again cannot change what the RPC just said about it. This
    /// never returns `Err` while the transaction could still be in flight:
    /// the caller keeps the hash, sequence and window either way.
    pub async fn wait_for(
        &self,
        hash: TxHash,
        sequence: i64,
        window: LedgerWindow,
    ) -> Result<TxOutcome, ChainError> {
        let deadline = Instant::now() + self.config.wait_cap;
        loop {
            match self.rpc.transaction(&hash).await {
                Ok(status) => {
                    if let Some(outcome) = classify(status, hash, window) {
                        return Ok(outcome);
                    }
                }
                Err(error) if is_transient(&error) => {
                    tracing::warn!(%hash, %error, "getTransaction failed; polling again");
                }
                Err(error) => {
                    tracing::error!(
                        %hash,
                        %error,
                        "getTransaction failed permanently; the outcome is unknown"
                    );
                    return Ok(TxOutcome::Unknown {
                        hash,
                        sequence,
                        window,
                    });
                }
            }
            if Instant::now() >= deadline {
                return Ok(TxOutcome::Unknown {
                    hash,
                    sequence,
                    window,
                });
            }
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// `wait_for` on the hash, sequence and window `prepared` carries.
    pub async fn wait(&self, prepared: &Prepared) -> Result<TxOutcome, ChainError> {
        self.wait_for(prepared.hash, prepared.sequence, prepared.window)
            .await
    }

    /// The whole write path: prepare, send, wait. A convenience for a
    /// caller that can afford to lose the handle when `send` itself fails
    /// in transport: on that failure this has already returned the error,
    /// and there is no `Prepared` left to resume from. Once `send`
    /// succeeds, `wait` always returns an outcome — even a permanent
    /// polling failure becomes `Unknown` rather than an `Err` — so `send`
    /// failing in transport is the only way this loses the handle. A
    /// submission queue does not call this — it calls `prepare`, records
    /// the returned `Prepared`'s hash, sequence and window as its own
    /// crash-recovery state, then `send` and `wait`, so a send that timed
    /// out is resumed with `wait_for` from the recorded fields instead of
    /// resent: a stale plan is never resent (section 8 of the spec).
    pub async fn submit(
        &self,
        operation: Operation,
        priority: Priority,
    ) -> Result<TxOutcome, ChainError> {
        let prepared = self.prepare(operation, priority).await?;
        tracing::info!(
            hash = %prepared.hash,
            sequence = prepared.sequence,
            max_ledger = prepared.window.max_ledger(),
            fee = prepared.fee,
            "sending transaction"
        );
        self.send(&prepared).await?;
        self.wait(&prepared).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::{
        account_entry_b64, diagnostic_error_b64, meta_v4_b64, result_b64, scval_b64,
        transaction_data_b64, ScriptedRpc,
    };
    use crate::chain::xdr::encode::{address, from_base64, invoke_contract_op, to_base64};
    use serde_json::json;
    use stellar_xdr::{
        LedgerKey, LedgerKeyAccount, OperationBody, Preconditions, TransactionEnvelope,
        TransactionExt, TransactionResultResult, VecM,
    };

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";

    fn signer() -> Signer {
        let key = ed25519_dalek::SigningKey::from_bytes(&[9_u8; 32]);
        Signer::from_secret(&stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string())
            .unwrap()
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
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: signer().account_id(),
        });
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

    fn script_simulation(
        rpc: &ScriptedRpc,
        resource_fee: i64,
        ledger: u32,
        restore_fee: Option<i64>,
    ) {
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
        let envelope: TransactionEnvelope =
            from_base64(params[index]["transaction"].as_str().unwrap()).unwrap();
        let TransactionEnvelope::Tx(v1) = envelope else {
            panic!("expected a v1 envelope")
        };
        v1.tx
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
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

        let prepared = submitter
            .prepare(operation(), Priority::Normal)
            .await
            .unwrap();
        assert_eq!(prepared.sequence, 42);
        assert_eq!(
            prepared.window.max_ledger(),
            104,
            "latest 100 + poll 3 + 1, exclusive"
        );
        assert_eq!(
            prepared.fee,
            5_000 + 446_953,
            "p70 200 is below the 5000 floor"
        );
        assert_eq!(prepared.resource_fee, 446_953);
        let TransactionEnvelope::Tx(v1) = &prepared.envelope else {
            panic!("v1")
        };
        assert_eq!(v1.signatures.len(), 1);
        assert_eq!(v1.tx.seq_num.0, 42);
        assert_eq!(v1.tx.source_account, signer.muxed());
        assert_eq!(v1.tx.fee, prepared.fee);
        let Preconditions::V2(cond) = &v1.tx.cond else {
            panic!("v2 preconditions")
        };
        let time = cond.time_bounds.as_ref().unwrap();
        assert_eq!(time.min_time.0, 0);
        assert!(
            (unix_now() + TIME_BOUND_SECS - 5..=unix_now() + TIME_BOUND_SECS)
                .contains(&time.max_time.0)
        );
        let ledgers = cond.ledger_bounds.as_ref().unwrap();
        assert_eq!(
            (ledgers.min_ledger, ledgers.max_ledger),
            (100, 104),
            "the lower bound is the ledger the transaction was built against"
        );
        assert_eq!(prepared.window.min_ledger(), 100);
        let TransactionExt::V1(data) = &v1.tx.ext else {
            panic!("soroban data attached")
        };
        assert_eq!(data.resource_fee, 446_953);
        assert_eq!(prepared.hash.0, prepared.envelope.hash(network.id).unwrap());
        // The simulation saw the real source account and sequence, unsigned.
        let simulated: TransactionEnvelope = from_base64(
            rpc.calls("simulateTransaction")[0]["transaction"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let TransactionEnvelope::Tx(sim) = simulated else {
            panic!("v1")
        };
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
        let fees = FeeStats {
            soroban_percentile_70: 7_000,
            soroban_percentile_90: 9_000,
            latest_ledger: 1,
        };
        assert_eq!(submitter.inclusion_fee(&fees, Priority::Normal), 7_000);
        assert_eq!(submitter.inclusion_fee(&fees, Priority::High), 10_000);
        let quiet = FeeStats {
            soroban_percentile_70: 100,
            soroban_percentile_90: 100,
            latest_ledger: 1,
        };
        assert_eq!(submitter.inclusion_fee(&quiet, Priority::Normal), 5_000);
        assert_eq!(submitter.inclusion_fee(&quiet, Priority::High), 10_000);
        let busy = FeeStats {
            soroban_percentile_70: 20_000,
            soroban_percentile_90: 30_000,
            latest_ledger: 1,
        };
        assert_eq!(submitter.inclusion_fee(&busy, Priority::High), 30_000);
    }

    #[tokio::test]
    async fn prepare_restores_archived_entries_then_simulates_again() {
        let rpc = ScriptedRpc::start().await;
        script_account(&rpc, 41, 100);
        script_fees(&rpc, 200, 200);
        script_simulation(&rpc, 10, 100, Some(77));
        rpc.expect(
            "sendTransaction",
            json!({"status": "PENDING", "hash": "11".repeat(32), "latestLedger": 100}),
        );
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

        let prepared = submitter
            .prepare(operation(), Priority::Normal)
            .await
            .unwrap();
        assert_eq!(prepared.sequence, 43, "the restore consumed sequence 42");
        assert_eq!(prepared.fee, 5_000 + 500);
        assert_eq!(
            prepared.window.max_ledger(),
            105,
            "bounded from the second simulation's ledger"
        );
        let restore = sent_transaction(&rpc, 0);
        assert_eq!(restore.seq_num.0, 42);
        assert!(matches!(
            restore.operations[0].body,
            OperationBody::RestoreFootprint(_)
        ));
        assert_eq!(restore.fee, 5_000 + 77);
        let TransactionExt::V1(data) = &restore.ext else {
            panic!("restore data attached")
        };
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
        rpc.expect(
            "sendTransaction",
            json!({"status": "PENDING", "hash": "11".repeat(32), "latestLedger": 100}),
        );
        rpc.expect(
            "getTransaction",
            json!({"status": "FAILED", "latestLedger": 101, "oldestLedger": 1, "ledger": 101,
                   "resultXdr": result_b64(TransactionResultResult::TxInsufficientBalance),
                   "resultMetaXdr": meta_v4_b64(None, vec![])}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = Submitter::new(&client, &network, &signer, config());
        assert!(matches!(
            submitter
                .prepare(operation(), Priority::Normal)
                .await
                .unwrap_err(),
            ChainError::Restore(_)
        ));
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
        let error = submitter
            .prepare(operation(), Priority::Normal)
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                ChainError::Simulation {
                    contract_error: Some(1212),
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "nothing is sent after a failed simulation"
        );
    }

    fn prepared(max_ledger: u32) -> Prepared {
        let envelope = crate::chain::xdr::encode::simulation_envelope(operation()).unwrap();
        Prepared {
            envelope,
            hash: TxHash([0x42; 32]),
            sequence: 42,
            window: LedgerWindow::try_new(100, max_ledger).unwrap(),
            fee: 100,
            resource_fee: 0,
        }
    }

    fn submitter_for<'a>(
        client: &'a RpcClient,
        network: &'a Network,
        signer: &'a Signer,
    ) -> Submitter<'a> {
        // The test config: millisecond pauses, a 200 ms wait cap.
        Submitter::new(client, network, signer, config())
    }

    #[tokio::test]
    async fn send_retries_try_again_later_exactly_once() {
        let hash = "42".repeat(32);
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "sendTransaction",
            json!({"status": "TRY_AGAIN_LATER", "hash": hash, "latestLedger": 1}),
        );
        rpc.expect(
            "sendTransaction",
            json!({"status": "PENDING", "hash": hash, "latestLedger": 1}),
        );
        rpc.expect(
            "sendTransaction",
            json!({"status": "TRY_AGAIN_LATER", "hash": hash, "latestLedger": 1}),
        );
        rpc.expect(
            "sendTransaction",
            json!({"status": "TRY_AGAIN_LATER", "hash": hash, "latestLedger": 1}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = submitter_for(&client, &network, &signer);
        submitter.send(&prepared(104)).await.unwrap();
        assert!(matches!(
            submitter.send(&prepared(104)).await.unwrap_err(),
            ChainError::Rejected(_)
        ));
        assert_eq!(rpc.calls("sendTransaction").len(), 4);
    }

    #[tokio::test]
    async fn a_bad_sequence_at_send_is_bad_sequence_and_other_rejections_are_rejected() {
        let hash = "42".repeat(32);
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "sendTransaction",
            json!({"status": "ERROR", "hash": hash, "latestLedger": 1,
                   "errorResultXdr": result_b64(TransactionResultResult::TxBadSeq)}),
        );
        rpc.expect(
            "sendTransaction",
            json!({"status": "ERROR", "hash": hash, "latestLedger": 1,
                   "errorResultXdr": result_b64(TransactionResultResult::TxInsufficientFee)}),
        );
        rpc.expect(
            "sendTransaction",
            json!({"status": "DUPLICATE", "hash": hash, "latestLedger": 1}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = submitter_for(&client, &network, &signer);
        assert!(matches!(
            submitter.send(&prepared(104)).await.unwrap_err(),
            ChainError::BadSequence
        ));
        assert!(matches!(
            submitter.send(&prepared(104)).await.unwrap_err(),
            ChainError::Rejected(_)
        ));
        submitter.send(&prepared(104)).await.unwrap();
    }

    #[test]
    fn classify_decides_from_the_status_and_the_ledger_bound() {
        let prepared = prepared(104);
        let success = TransactionStatus::Success {
            ledger: 102,
            latest_ledger: 102,
            return_value: Some(ScVal::U32(7)),
        };
        assert!(matches!(
            classify(success, prepared.hash, prepared.window),
            Some(TxOutcome::Succeeded {
                ledger: 102,
                return_value: Some(ScVal::U32(7)),
                ..
            })
        ));
        let result: TransactionResult = from_base64(&result_b64(
            TransactionResultResult::TxFailed(VecM::default()),
        ))
        .unwrap();
        let failed = TransactionStatus::Failed {
            ledger: 103,
            latest_ledger: 103,
            result,
            contract_error: Some(1205),
        };
        assert!(matches!(
            classify(failed, prepared.hash, prepared.window),
            Some(TxOutcome::Failed {
                ledger: 103,
                contract_error: Some(1205),
                ..
            })
        ));
        assert!(classify(
            TransactionStatus::NotFound {
                latest_ledger: 103,
                oldest_ledger: 1
            },
            prepared.hash,
            prepared.window
        )
        .is_none());
        assert!(matches!(
            classify(
                TransactionStatus::NotFound {
                    latest_ledger: 104,
                    oldest_ledger: 1
                },
                prepared.hash,
                prepared.window
            ),
            Some(TxOutcome::Expired {
                latest_ledger: 104,
                ..
            })
        ));
    }

    #[test]
    fn classify_never_calls_a_transaction_expired_when_the_rpc_forgot_the_window() {
        // The chain has passed max_ledger (104), but the RPC's oldest_ledger
        // (103) has already moved past min_ledger (100): its NOT_FOUND
        // cannot distinguish "never applied" from "applied in a ledger this
        // RPC no longer retains", so classify refuses to call it Expired.
        let hash = TxHash([0x42; 32]);
        assert!(classify(
            TransactionStatus::NotFound {
                latest_ledger: 104,
                oldest_ledger: 103
            },
            hash,
            LedgerWindow::try_new(100, 104).unwrap()
        )
        .is_none());
    }

    #[test]
    fn a_ledger_window_must_not_be_empty_or_inverted() {
        assert!(matches!(
            LedgerWindow::try_new(5, 5),
            Err(ChainError::Config(_))
        ));
        assert!(matches!(
            LedgerWindow::try_new(6, 5),
            Err(ChainError::Config(_))
        ));
        assert!(LedgerWindow::try_new(5, 6).is_ok());
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
        assert!(
            matches!(
                outcome,
                TxOutcome::Succeeded {
                    ledger: 102,
                    return_value: Some(ScVal::U32(7)),
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(rpc.calls("getTransaction").len(), 3);
        let expired = submitter.wait(&prepared(104)).await.unwrap();
        assert!(
            matches!(
                expired,
                TxOutcome::Expired {
                    window,
                    latest_ledger: 104,
                    ..
                } if window.max_ledger() == 104
            ),
            "{expired:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_resumes_from_the_recorded_fields() {
        // No `Prepared` in sight: a queue resuming an `Unknown` outcome
        // after a restart has only the hash, sequence and bound it
        // persisted, not the envelope that produced them.
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "getTransaction",
            json!({"status": "NOT_FOUND", "latestLedger": 101, "oldestLedger": 1, "ledger": 0}),
        );
        rpc.expect(
            "getTransaction",
            json!({"status": "SUCCESS", "latestLedger": 102, "oldestLedger": 1, "ledger": 102,
                   "resultXdr": result_b64(TransactionResultResult::TxSuccess(VecM::default())),
                   "resultMetaXdr": meta_v4_b64(Some(ScVal::U32(7)), vec![])}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = submitter_for(&client, &network, &signer);
        let hash = TxHash([0x42; 32]);
        let window = LedgerWindow::try_new(100, 104).unwrap();
        let outcome = submitter.wait_for(hash, 42, window).await.unwrap();
        assert!(
            matches!(
                outcome,
                TxOutcome::Succeeded {
                    hash: resumed,
                    ledger: 102,
                    ..
                } if resumed == hash
            ),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_reports_unknown_on_a_permanent_rpc_error() {
        // A JSON-RPC error object is not something polling again can fix,
        // and the transaction may already have been sent, so the handle
        // (hash, sequence, window) must survive as `Unknown` rather than
        // being discarded behind an `Err`.
        let rpc = ScriptedRpc::start().await;
        rpc.expect_error("getTransaction", -32_602, "invalid hash");
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (network, signer) = (Network::testnet(), signer());
        let submitter = submitter_for(&client, &network, &signer);
        let hash = TxHash([0x42; 32]);
        let window = LedgerWindow::try_new(100, 104).unwrap();
        let outcome = submitter.wait_for(hash, 42, window).await.unwrap();
        assert!(
            matches!(
                outcome,
                TxOutcome::Unknown {
                    hash: reported,
                    ..
                } if reported == hash
            ),
            "{outcome:?}"
        );
        assert_eq!(rpc.calls("getTransaction").len(), 1);
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
        assert!(
            matches!(
                outcome,
                TxOutcome::Failed {
                    ledger: 103,
                    contract_error: Some(1205),
                    ..
                }
            ),
            "{outcome:?}"
        );
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
        assert!(
            matches!(
                outcome,
                TxOutcome::Unknown {
                    sequence: 42,
                    window,
                    ..
                } if window.max_ledger() == 104
            ),
            "{outcome:?}"
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(rpc.calls("getTransaction").len() >= 2);
    }

    #[tokio::test]
    async fn submit_runs_the_whole_path() {
        let rpc = ScriptedRpc::start().await;
        script_account(&rpc, 41, 100);
        script_fees(&rpc, 200, 200);
        script_simulation(&rpc, 300, 100, None);
        rpc.expect(
            "sendTransaction",
            json!({"status": "PENDING", "hash": "77".repeat(32), "latestLedger": 100}),
        );
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
        let TxOutcome::Succeeded {
            ledger,
            return_value,
            hash,
        } = outcome
        else {
            panic!("succeeded")
        };
        assert_eq!((ledger, return_value), (101, Some(ScVal::I32(-1))));
        // The hash polled is the hash of the envelope that was sent.
        let sent = sent_transaction(&rpc, 0);
        assert_eq!(sent.fee, 10_000 + 300);
        assert_eq!(rpc.calls("getTransaction")[0]["hash"], hash.to_hex());
        assert_eq!(rpc.remaining(), 0);
    }
}
