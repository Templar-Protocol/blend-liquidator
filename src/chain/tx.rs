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
            prepared.max_ledger, 104,
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
        assert_eq!((ledgers.min_ledger, ledgers.max_ledger), (0, 104));
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
            prepared.max_ledger, 105,
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
}
