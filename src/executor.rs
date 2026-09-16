//! The executor: one planned fill, from the contract's judgment to the
//! audit row, the submission and the settled reservation.
//!
//! [`crate::math::fill`] decides what to fill; this module is the I/O
//! around one such decision, in the order spec §5 sets out and
//! [`Executor::execute`] documents: the mode guards, the contract's
//! judgment through [`Submitter::simulate_only`], the `fills` row, the
//! submission on the filler's queue, and the settlement of the wallet
//! reservation the plan is holding. No arithmetic on money happens here
//! (ruling 1): every amount this module touches was computed by
//! [`crate::math::fill`] and is passed through unchanged.
//!
//! Two guarantees hold on every path through it.
//!
//! **A dry run signs nothing and sends nothing.** The only simulation it
//! makes is [`Submitter::simulate_only`], which builds unsigned and never
//! calls `sendTransaction`; [`Submitter::prepare`] signs unconditionally
//! and submits a `RestoreFootprint` of its own when a footprint is
//! archived, so it is reached only from behind the submission queue. A
//! queue handed to a dry-run executor — or to one with no signer, which
//! could judge nothing before sending — is refused outright, before
//! anything is simulated, recorded or enqueued: `Service::run` builds no
//! such combination, but the public pieces composed by hand must not be
//! able to make a dry run that sends.
//!
//! **The reservation is settled exactly once, on every non-panicking
//! path.** [`crate::inventory::Reservation`] releases itself when dropped
//! unsettled, and warns when it does; this module settles by value
//! instead, so that warning stays what it is meant to be — a bug's
//! signature, not the sound of an ordinary refusal.

use stellar_xdr::Operation;

use crate::chain::pool::submit_op;
use crate::chain::tx::{Judgment, Priority, Submitter};
use crate::chain::xdr::encode::{Request, RequestType};
use crate::chain::xdr::{AuctionType, XdrError};
use crate::chain::{ChainError, TxOutcome};
use crate::inventory::{Reservation, Settlement};
use crate::math::fill::{FillAction, FillDraft};
use crate::queue::{QueueError, Submission, SubmissionQueue, FILL_RETRIES};
use crate::store::{FillRecord, Store, StoreError};

/// `WithdrawCollateral`'s "all": the contract caps what it burns at the
/// position (`to_burn = min(to_b_token_up(amount), balance)`), and
/// `to_b_token_up(i64::MAX)` is `9.22e18 × 1e12 / b_rate`, far inside
/// `i128` — so this withdraws everything and cannot overflow the
/// contract's own arithmetic.
pub const WITHDRAW_ALL: i128 = 9_223_372_036_854_775_807;

/// The pool's `InvalidHf`: the post-submit health check failed.
const INVALID_HF: u32 = 1_205;

/// The pool's `MinCollateralNotMet`.
const MIN_COLLATERAL_NOT_MET: u32 = 1_224;

/// A planned fill, ready to execute.
#[derive(Debug, Clone)]
pub struct FillPlan {
    /// The pool contract.
    pub pool: String,
    /// The liquidated account.
    pub user: String,
    /// What `plan_fill` drafted.
    pub draft: FillDraft,
    /// The fee tier: high when the estimated profit reaches
    /// `HIGH_FEE_PROFIT_THRESHOLD`.
    pub priority: Priority,
}

/// One fill this executor recorded, whether or not it was sent: the row it
/// wrote, and what simulation and the queue found out that the `fills`
/// table has no column for.
#[derive(Debug)]
pub struct FillRecorded {
    /// The `fills` row this was written as, written before anything was
    /// submitted.
    pub fill_id: i64,
    /// Whether the contract was actually asked. `false` only when no
    /// filler key is configured at all — there is then no source account
    /// to simulate against, dry-run or not (ruling 4).
    pub simulated: bool,
    /// The bot's configured `DRY_RUN` mode, which is what the row's
    /// `dry_run` column means — never "whether this was sent", which is
    /// `tx_hash`.
    pub dry_run: bool,
    /// The outcome the queue reported, when one was given. `None` in
    /// dry-run, and for an armed executor that was handed no queue.
    pub submission: Option<TxOutcome>,
}

impl FillRecorded {
    /// Whether the chain applied this fill and it succeeded. `false` for a
    /// dry run, a failure, an expiry and an unresolved outcome alike — a
    /// caller that must tell those apart reads `submission` itself.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(self.submission, Some(TxOutcome::Succeeded { .. }))
    }
}

/// What [`Executor::execute`] did with one plan.
#[derive(Debug)]
pub enum ExecOutcome {
    /// The fill is on the audit, and — when armed — was submitted.
    Recorded(FillRecorded),
    /// The contract's own health check disagreed with the plan's
    /// projection. Spec §5 allows exactly one re-plan, at a lower percent
    /// (ruling 12); nothing was recorded.
    Replan {
        /// `InvalidHf` or `MinCollateralNotMet`.
        contract_error: u32,
    },
    /// The contract refused this fill for a reason no re-plan addresses,
    /// or its footprint could not be judged at all. Nothing was recorded;
    /// the caller leaves this auction until the next tick.
    Refused {
        /// The pool's error code, when the refusal carried one. `None`
        /// for a footprint that needs restoring, which is not a judgment
        /// on the fill at all.
        contract_error: Option<u32>,
    },
    /// Another transaction consumed this key's sequence number first, so
    /// the plan was built against state that has moved: it is re-planned
    /// from fresh state, never resent (spec §8). The row written before
    /// the send stays, with no hash.
    Stale,
}

/// A failure that stopped one fill.
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    /// Reading or writing the store failed.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// Reading the chain, or judging the operation against it, failed.
    #[error("chain: {0}")]
    Chain(#[from] ChainError),
    /// The operation could not be encoded.
    #[error("xdr: {0}")]
    Xdr(#[from] XdrError),
    /// The submission queue could not carry this fill through.
    #[error("queue: {0}")]
    Queue(#[from] QueueError),
    /// The call composed capabilities this executor does not have: a
    /// settlement that does not match its mode, or a submission queue
    /// given to an executor that may not send.
    #[error("{0}")]
    Mode(&'static str),
}

/// The `submit` requests one draft becomes: the fill first, then the
/// draft's actions in the order it planned them.
///
/// The contract checks the filler's health once, after the whole request
/// list, so the order changes nothing but readability — except for the
/// fill itself, which must come first because every action after it acts
/// on what it took.
///
/// # Errors
///
/// [`XdrError`] when the percent is outside the contract's `1..=100` —
/// which [`crate::chain::xdr::FillPercent`] already rules out by
/// construction — or the request type is not a fill.
pub fn fill_requests(user: &str, draft: &FillDraft) -> Result<Vec<Request>, XdrError> {
    let mut requests = vec![Request::fill(
        RequestType::FillUserLiquidationAuction,
        user,
        draft.percent,
    )?];
    for action in &draft.actions {
        requests.push(match action {
            FillAction::Repay { asset, amount } => Request {
                request_type: RequestType::Repay,
                address: asset.clone(),
                amount: *amount,
            },
            FillAction::WithdrawAll { asset } => Request {
                request_type: RequestType::WithdrawCollateral,
                address: asset.clone(),
                amount: WITHDRAW_ALL,
            },
            FillAction::SupplyCollateral { asset, amount } => Request {
                request_type: RequestType::SupplyCollateral,
                address: asset.clone(),
                amount: *amount,
            },
        });
    }
    Ok(requests)
}

/// What becomes of the wallet reservation a plan was holding. Every path
/// out of `Executor::run` names one, so the token is settled by value
/// exactly once and never left to the drop guard.
#[derive(Debug, Clone, Copy)]
enum Settle {
    /// The wallet paid: the transaction landed, or may have (ruling 14).
    Consume,
    /// Nothing was spent: the plan was refused, failed, expired or was
    /// never sent.
    Release,
}

/// What the contract made of the plan, and what is left to do with it.
enum Judged {
    /// It accepted the operation — or, armed, only needs a restore that
    /// `Submitter::prepare` makes on the way to sending.
    Accepted(Operation),
    /// There is no signer to simulate as, so the plan is recorded
    /// unsimulated. It is never submitted: the mode guards refuse a queue
    /// to an executor with no signer.
    Unsimulated,
    /// It refused: nothing is recorded, and this is the answer.
    Refused(ExecOutcome),
}

/// `Replan` for the two errors that mean the contract's own health check
/// disagreed with the plan's projection — which a lower percent can fix —
/// and `Refused` for every other refusal, including one that carried no
/// code at all.
fn refusal(contract_error: Option<u32>) -> ExecOutcome {
    match contract_error {
        Some(code @ (INVALID_HF | MIN_COLLATERAL_NOT_MET)) => ExecOutcome::Replan {
            contract_error: code,
        },
        other => ExecOutcome::Refused {
            contract_error: other,
        },
    }
}

/// Executes one planned fill against one pool, for one signing key.
///
/// `dry_run` is the bot's configured mode, and it is not merely recorded:
/// it is half of what the mode guards check, the other half being whether
/// a submission queue was offered. A live executor is expected to hold a
/// signer — `DRY_RUN=false` requires `FILLER_SECRET_KEY` at startup — and
/// one that somehow does not is refused a queue rather than trusted with
/// one.
#[derive(Debug)]
pub struct Executor<'a> {
    store: &'a Store,
    /// Simulates as the filler, and signs for it behind the queue. `None`
    /// when no filler key is configured: a dry run then records what it
    /// would have done, unsimulated (ruling 4).
    submitter: Option<Submitter<'a>>,
    dry_run: bool,
}

impl<'a> Executor<'a> {
    /// An executor writing to `store`, judging and signing as `submitter`.
    #[must_use]
    pub fn new(store: &'a Store, submitter: Option<Submitter<'a>>, dry_run: bool) -> Self {
        Self {
            store,
            submitter,
            dry_run,
        }
    }

    /// The filler's own address, when a key is configured. This is the
    /// account every fill is submitted `from`, `spender` and `to`, and the
    /// one the planner reads positions and balances for.
    #[must_use]
    pub fn filler(&self) -> Option<&str> {
        self.submitter.as_ref().map(Submitter::source)
    }

    /// One planned fill, in spec §5's order.
    ///
    /// 1. **The mode guards**, before anything touches the chain. A
    ///    dry-run executor handed [`Settlement::Live`] fails, and releases
    ///    the reservation it was handed; a live one handed
    ///    [`Settlement::DryRun`] fails; a `queue` offered to an executor
    ///    that is dry-run, or has no signer to judge with, fails. Nothing
    ///    is simulated, recorded or enqueued on any of those paths.
    /// 2. **Simulate the exact `submit`** the queue would send —
    ///    `from`, `spender` and `to` all the filler's own address, the
    ///    requests [`fill_requests`] builds — through
    ///    [`Submitter::simulate_only`], which builds unsigned and neither
    ///    signs, restores nor sends. `InvalidHf` or `MinCollateralNotMet`
    ///    answers [`ExecOutcome::Replan`]; any other refusal answers
    ///    [`ExecOutcome::Refused`] with the code on a warn line. Nothing is
    ///    recorded for either. A footprint holding archived entries is
    ///    `Refused` in dry-run — restoring is a submission, and this is not
    ///    the code that makes one — and proceeds when armed, since
    ///    [`Submitter::prepare`] restores it behind the queue. With no
    ///    signer there is no source account to ask as, so the plan is
    ///    recorded unsimulated (ruling 4).
    /// 3. **Record before sending.** The `fills` row, and the structured
    ///    log event that mirrors it, both exist before the operation is
    ///    handed to the queue. A crash between them leaves a row that is at
    ///    worst incomplete, never a transaction on chain that nothing
    ///    recorded.
    /// 4. **Submit** on `queue`, when one is given, at the plan's priority
    ///    and with [`FILL_RETRIES`], and attach the transaction's hash to
    ///    the row for every outcome — a failed, expired or unresolved
    ///    transaction consumed a sequence number and is worth naming.
    ///    [`ChainError::BadSequence`] answers [`ExecOutcome::Stale`]; a
    ///    refusal at `prepare` answers as step 2 does.
    /// 5. **A dry run stops after step 3**: it took no reservation and
    ///    sends nothing.
    ///
    /// The reservation is settled by value on every path: consumed when the
    /// transaction landed or may have (ruling 14), released otherwise.
    ///
    /// # Errors
    ///
    /// [`ExecutorError`] for one fill: a store write, a chain read that
    /// could not be made at all, an operation that would not encode, a
    /// queue that refused the submission, or a mode guard. A contract's
    /// refusal is none of those — it is an [`ExecOutcome`]. An error is
    /// never a settlement of its own: a [`ExecutorError::Store`] raised
    /// *after* the chain has answered — the hash write — settles by that
    /// answer and then reports the failure, so a transaction that landed
    /// leaves the wallet debited whether or not its row was ever named.
    pub async fn execute(
        &self,
        plan: &FillPlan,
        settlement: Settlement,
        queue: Option<&SubmissionQueue>,
    ) -> Result<ExecOutcome, ExecutorError> {
        let reservation = match settlement {
            Settlement::Live(reservation) if self.dry_run => {
                reservation.release();
                return Err(ExecutorError::Mode(
                    "a live reservation was given to a dry-run executor; dry-run spends \
                     nothing, so the reservation is released rather than held",
                ));
            }
            Settlement::DryRun if !self.dry_run => {
                return Err(ExecutorError::Mode(
                    "a live executor was given no reservation; a fill that spends the \
                     wallet must hold one before it is sent",
                ));
            }
            Settlement::Live(reservation) => Some(reservation),
            Settlement::DryRun => None,
        };
        if queue.is_some() && self.dry_run {
            release(reservation);
            return Err(ExecutorError::Mode(
                "a submission queue was given to a dry-run executor; dry-run sends nothing, \
                 so the queue is refused rather than used",
            ));
        }
        if queue.is_some() && self.submitter.is_none() {
            release(reservation);
            return Err(ExecutorError::Mode(
                "a submission queue was given to an executor with no signer, so nothing \
                 could be judged before it was sent",
            ));
        }

        // The one settlement point, and it applies to the failing answer
        // too: what the reservation becomes is decided by what the *chain*
        // did, never by whether this module then managed to write it down.
        let (answer, settle) = self.run(plan, queue).await;
        match settle {
            Settle::Consume => {
                if let Some(reservation) = reservation {
                    reservation.consume();
                }
            }
            Settle::Release => release(reservation),
        }
        answer
    }

    /// Steps 2 to 4, and what the reservation the caller still holds
    /// becomes. The [`Settle`] is returned alongside the answer rather than
    /// inside it, error included: a failure *after* the chain has answered
    /// — the hash write that names a transaction already applied — must
    /// still settle by that answer, or the wallet's ledger hands back
    /// amounts the chain has spent.
    async fn run(
        &self,
        plan: &FillPlan,
        queue: Option<&SubmissionQueue>,
    ) -> (Result<ExecOutcome, ExecutorError>, Settle) {
        // Nothing of this fill has been sent while steps 2 and 3 run, so
        // every failure and every early answer in them releases.
        let judged = match self.judge(plan, queue.is_some()).await {
            Ok(judged) => judged,
            Err(error) => return (Err(error), Settle::Release),
        };
        let (operation, simulated) = match judged {
            Judged::Refused(outcome) => return (Ok(outcome), Settle::Release),
            Judged::Accepted(operation) => (Some(operation), true),
            Judged::Unsimulated => (None, false),
        };

        let draft = &plan.draft;
        let record = FillRecord {
            pool: plan.pool.clone(),
            account: plan.user.clone(),
            auction_type: AuctionType::UserLiquidation,
            fill_ledger: draft.fill_ledger,
            percent: draft.percent,
            bid: draft.to_fill.bid.clone(),
            lot: draft.to_fill.lot.clone(),
            bid_value: draft.bid_value,
            lot_value: draft.lot_value,
            est_profit: draft.est_profit,
            dry_run: self.dry_run,
        };
        let fill_id = match self.store.record_fill(&record).await {
            Ok(fill_id) => fill_id,
            Err(error) => return (Err(ExecutorError::Store(error)), Settle::Release),
        };
        tracing::info!(
            fill_id,
            pool = %record.pool,
            account = %record.account,
            auction_type = ?record.auction_type,
            fill_ledger = record.fill_ledger,
            percent = record.percent.get(),
            bid = ?record.bid,
            lot = ?record.lot,
            bid_value = record.bid_value,
            lot_value = record.lot_value,
            est_profit = record.est_profit,
            dry_run = record.dry_run,
            armed = queue.is_some(),
            simulated,
            "fill recorded"
        );
        let Some(queue) = queue else {
            return (Ok(self.recorded(fill_id, simulated, None)), Settle::Release);
        };
        // The mode guards refuse a queue to an executor with no signer, so
        // an operation was built above. Recording without submitting is
        // the safe answer to a composition that somehow got past them, and
        // is never a panic.
        let Some(operation) = operation else {
            return (Ok(self.recorded(fill_id, simulated, None)), Settle::Release);
        };
        self.submit_recorded(queue, plan, fill_id, simulated, operation)
            .await
    }

    /// The answer for a fill this executor recorded, sent or not.
    fn recorded(
        &self,
        fill_id: i64,
        simulated: bool,
        submission: Option<TxOutcome>,
    ) -> ExecOutcome {
        ExecOutcome::Recorded(FillRecorded {
            fill_id,
            simulated,
            dry_run: self.dry_run,
            submission,
        })
    }

    /// Step 4: hands an already-recorded fill to the queue, attaches the
    /// transaction it became to its row, and says what the reservation
    /// becomes.
    ///
    /// The order is the audit's: the row exists before the submission, so
    /// the hash is a second write and a row with `dry_run = false` and no
    /// hash is an armed attempt whose transaction was never named. A row
    /// that has gone missing between the two writes is logged, not raised
    /// — nothing deletes a fill, and failing here would report a
    /// submission that has already happened as one that did not.
    async fn submit_recorded(
        &self,
        queue: &SubmissionQueue,
        plan: &FillPlan,
        fill_id: i64,
        simulated: bool,
        operation: Operation,
    ) -> (Result<ExecOutcome, ExecutorError>, Settle) {
        match queue
            .enqueue(Submission {
                operation,
                priority: plan.priority,
                label: format!("fill {} on {}", plan.user, plan.pool),
                retries: FILL_RETRIES,
            })
            .await
        {
            Ok(outcome) => {
                let hash = outcome.hash().to_hex();
                tracing::info!(
                    fill_id,
                    pool = %plan.pool,
                    account = %plan.user,
                    tx_hash = %hash,
                    status = outcome.status(),
                    "fill submitted"
                );
                // Decided from the chain's answer, and decided *before*
                // the hash is written: an `Unknown` may yet land and spend
                // the wallet, so the ledger assumes it did until the next
                // balance read says otherwise (ruling 14) — and a store
                // failure below must not turn that into a release, which
                // would hand the next plan amounts the chain has taken.
                let settle = match outcome {
                    TxOutcome::Succeeded { .. } | TxOutcome::Unknown { .. } => Settle::Consume,
                    TxOutcome::Failed { .. } | TxOutcome::Expired { .. } => Settle::Release,
                };
                match self.store.attach_fill_tx(fill_id, &hash).await {
                    Ok(true) => {}
                    Ok(false) => tracing::warn!(
                        fill_id,
                        tx_hash = %hash,
                        "no fill row to attach this transaction to"
                    ),
                    Err(error) => return (Err(ExecutorError::Store(error)), settle),
                }
                (Ok(self.recorded(fill_id, simulated, Some(outcome))), settle)
            }
            Err(QueueError::Chain(ChainError::BadSequence)) => {
                tracing::info!(
                    fill_id,
                    pool = %plan.pool,
                    account = %plan.user,
                    "another transaction spent this key's sequence first; this plan is stale \
                     and is re-planned rather than resent"
                );
                (Ok(ExecOutcome::Stale), Settle::Release)
            }
            Err(QueueError::Chain(ChainError::Simulation {
                contract_error,
                message,
            })) => {
                tracing::warn!(
                    fill_id,
                    pool = %plan.pool,
                    account = %plan.user,
                    contract_error,
                    %message,
                    "the contract refused this fill when it was prepared"
                );
                (Ok(refusal(contract_error)), Settle::Release)
            }
            // Releasing is safe here because of the contract
            // [`QueueError::Chain`] states: the queue answers it only for a
            // failure that provably sent nothing of this submission — a
            // `prepare` that failed, a send the RPC refused, a stale
            // sequence — never for a transaction that may be in flight,
            // which it resolves by hash instead. A queue that stopped
            // honouring that would make this arm hand back a wallet the
            // chain had already spent.
            Err(error) => (Err(ExecutorError::Queue(error)), Settle::Release),
        }
    }

    /// Step 2: the exact `submit` the queue would send, judged by the
    /// contract through [`Submitter::simulate_only`].
    ///
    /// `armed` decides only what an archived footprint means: a submission
    /// restores it on the way through [`Submitter::prepare`], and a dry run
    /// has no business making that submission, so it refuses instead.
    async fn judge(&self, plan: &FillPlan, armed: bool) -> Result<Judged, ExecutorError> {
        let requests = fill_requests(&plan.user, &plan.draft)?;
        let Some(submitter) = self.submitter.as_ref() else {
            tracing::debug!(
                pool = %plan.pool,
                account = %plan.user,
                "no filler key configured; recording the fill unsimulated"
            );
            return Ok(Judged::Unsimulated);
        };
        // The filler pays the bid from its own wallet, takes the lot into
        // its own position, and acts for itself: one account in all three
        // roles.
        let filler = submitter.source();
        let operation = submit_op(&plan.pool, filler, filler, filler, &requests)?;
        match submitter.simulate_only(&operation).await? {
            Judgment::Accepted => Ok(Judged::Accepted(operation)),
            Judgment::Refused {
                contract_error,
                message,
            } => {
                let outcome = refusal(contract_error);
                if matches!(outcome, ExecOutcome::Replan { .. }) {
                    tracing::info!(
                        pool = %plan.pool,
                        account = %plan.user,
                        contract_error,
                        %message,
                        "the contract's health check refused this fill; re-planning it lower"
                    );
                } else {
                    tracing::warn!(
                        pool = %plan.pool,
                        account = %plan.user,
                        contract_error,
                        %message,
                        "the contract refused this fill; skipping it this tick"
                    );
                }
                Ok(Judged::Refused(outcome))
            }
            Judgment::NeedsRestore if armed => {
                tracing::info!(
                    pool = %plan.pool,
                    account = %plan.user,
                    "this fill's footprint holds archived entries; the submission restores \
                     them before it sends"
                );
                Ok(Judged::Accepted(operation))
            }
            Judgment::NeedsRestore => {
                tracing::info!(
                    pool = %plan.pool,
                    account = %plan.user,
                    "this fill could not be judged: its footprint holds archived entries, \
                     which only an armed submission restores; skipping it"
                );
                Ok(Judged::Refused(ExecOutcome::Refused {
                    contract_error: None,
                }))
            }
        }
    }
}

/// Releases a reservation there may not be one of. The `Option` is what a
/// dry run holds, and releasing nothing is exactly right for it.
fn release(reservation: Option<Reservation>) {
    if let Some(reservation) = reservation {
        reservation.release();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use stellar_xdr::{TransactionResult, TransactionResultExt, TransactionResultResult, VecM};
    use tokio::sync::watch;

    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::{
        script_prepare_prelude, script_send, script_simulate_accepted,
        script_simulate_needs_restore, script_simulate_prelude, script_simulate_refused,
        script_transaction_success, ScriptedRpc,
    };
    use crate::chain::signer::{Network, Signer};
    use crate::chain::tx::TxConfig;
    use crate::chain::xdr::FillPercent;
    use crate::chain::TxHash;
    use crate::harness;
    use crate::inventory::Inventory;
    use crate::math::AuctionData;
    use crate::queue::{run_queue_with, RetryPolicy};

    /// The fixture's XLM and USDC reserves. Real strkeys, because every
    /// request this module builds has to encode as an `ScAddress`.
    const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    /// The filler's key. Copied from `auctioneer.rs`'s test module rather
    /// than shared with it: a test signer is scaffolding, not an interface.
    fn filler_signer() -> Signer {
        let key = ed25519_dalek::SigningKey::from_bytes(&[11_u8; 32]);
        let secret = stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string();
        Signer::from_secret(&secret).expect("signer")
    }

    /// Fee and polling policy short enough that a scripted `getTransaction`
    /// answer is read back without any real wait.
    fn tx_config() -> TxConfig {
        TxConfig {
            poll_interval: Duration::from_millis(1),
            send_retry_pause: Duration::from_millis(1),
            wait_cap: Duration::from_millis(200),
            ..TxConfig::new(100, 200, 3)
        }
    }

    fn queue_capacity() -> NonZeroUsize {
        NonZeroUsize::new(4).expect("a test capacity is never zero")
    }

    /// No test here means to wait on a backoff or a resolution pause.
    fn retry_policy() -> RetryPolicy {
        RetryPolicy {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(1),
            resolve_pause: Duration::from_millis(1),
        }
    }

    /// The draft every test executes: 80% of an auction that hands over
    /// 5,000 XLM for 100 USDC, repaying 10 USDC, withdrawing the XLM lot
    /// and supplying 7 XLM back as collateral.
    fn draft() -> FillDraft {
        FillDraft {
            fill_ledger: 64_271_400,
            percent: FillPercent::try_from(80).expect("80 is in range"),
            actions: vec![
                FillAction::Repay {
                    asset: USDC.to_string(),
                    amount: 10,
                },
                FillAction::WithdrawAll {
                    asset: XLM.to_string(),
                },
                FillAction::SupplyCollateral {
                    asset: XLM.to_string(),
                    amount: 7,
                },
            ],
            to_fill: AuctionData {
                bid: BTreeMap::from([(USDC.to_string(), 100)]),
                lot: BTreeMap::from([(XLM.to_string(), 5_000)]),
                block: 64_271_000,
            },
            lot_value: 900,
            bid_value: 400,
            est_profit: 500,
            spend: BTreeMap::from([(USDC.to_string(), 10), (XLM.to_string(), 7)]),
            projected_health: Some(15_000_000),
        }
    }

    fn plan(priority: Priority) -> FillPlan {
        FillPlan {
            pool: harness::POOL.to_string(),
            user: harness::USER_ONE.to_string(),
            draft: draft(),
            priority,
        }
    }

    /// The wallet the live tests reserve the draft's `spend` out of, with
    /// no fee reserve withheld so the arithmetic in each assertion is the
    /// balance itself.
    fn inventory() -> Inventory {
        let inventory = Inventory::new(XLM.to_string(), 0);
        inventory.record_balances(
            BTreeMap::from([(XLM.to_string(), 1_000), (USDC.to_string(), 500)]),
            Instant::now(),
        );
        inventory
    }

    /// The recorded fill an executed plan answers with, or a failure naming
    /// what came back instead.
    fn recorded(outcome: ExecOutcome) -> FillRecorded {
        match outcome {
            ExecOutcome::Recorded(recorded) => recorded,
            other => panic!("expected a recorded fill, got {other:?}"),
        }
    }

    /// The fill comes first, then the plan's actions in order; "withdraw
    /// all" is `WITHDRAW_ALL`.
    #[test]
    fn fill_requests_put_the_fill_first() {
        let requests = fill_requests(harness::USER_ONE, &draft()).expect("requests");

        assert_eq!(requests.len(), 4, "the fill plus the draft's three actions");
        assert_eq!(
            requests[0],
            Request {
                request_type: RequestType::FillUserLiquidationAuction,
                address: harness::USER_ONE.to_string(),
                amount: 80,
            },
            "the fill is the first request: everything after it acts on what it took"
        );
        assert_eq!(
            requests[1],
            Request {
                request_type: RequestType::Repay,
                address: USDC.to_string(),
                amount: 10,
            }
        );
        assert_eq!(
            requests[2],
            Request {
                request_type: RequestType::WithdrawCollateral,
                address: XLM.to_string(),
                amount: WITHDRAW_ALL,
            }
        );
        assert_eq!(
            requests[3],
            Request {
                request_type: RequestType::SupplyCollateral,
                address: XLM.to_string(),
                amount: 7,
            }
        );
        assert_eq!(
            WITHDRAW_ALL,
            i128::from(i64::MAX),
            "the contract caps what it burns at the position, so i64::MAX is 'all'"
        );
    }

    /// Dry-run simulates through `simulate_only` and records, and never
    /// reaches the signing path: no fee stats, no send.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_dry_run_records_the_fill_and_sends_nothing(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let executor = Executor::new(&store, Some(submitter), true);

        let outcome = recorded(
            executor
                .execute(&plan(Priority::Normal), Settlement::DryRun, None)
                .await
                .expect("execute"),
        );

        assert!(
            outcome.simulated,
            "a dry run still lets the contract judge the fill"
        );
        assert!(outcome.dry_run);
        assert!(outcome.submission.is_none(), "and sends nothing");
        assert!(
            !outcome.succeeded(),
            "nothing landed, because nothing was sent"
        );

        let row = sqlx::query!(
            r#"SELECT tx_hash, dry_run, pool, account, auction_type, fill_ledger, percent,
                      bid, lot, bid_value::text AS "bid_value!",
                      lot_value::text AS "lot_value!", est_profit::text AS "est_profit!"
               FROM fills WHERE id = $1"#,
            outcome.fill_id,
        )
        .fetch_one(store.pool())
        .await?;
        assert_eq!(row.tx_hash, None, "nothing was sent to name it with");
        assert!(row.dry_run);
        assert_eq!(row.pool, harness::POOL);
        assert_eq!(
            row.account,
            harness::USER_ONE,
            "the account is the liquidated user, never the filler"
        );
        assert_eq!(
            (row.auction_type, row.fill_ledger, row.percent),
            (0, 64_271_400, 80)
        );
        assert_eq!(row.bid[USDC], serde_json::json!("100"));
        assert_eq!(row.lot[XLM], serde_json::json!("5000"));
        assert_eq!(
            (
                row.bid_value.as_str(),
                row.lot_value.as_str(),
                row.est_profit.as_str()
            ),
            ("400", "900", "500")
        );

        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "a dry run never sends"
        );
        assert!(
            rpc.calls("getFeeStats").is_empty(),
            "and never reaches the signing path, which is what pays a fee"
        );
        assert_eq!(
            rpc.remaining(),
            0,
            "every scripted answer was used: an over-scripted test would hide a chain call \
             this module never made"
        );
        Ok(())
    }

    /// With no key there is no account to simulate as; the plan is still
    /// recorded, and says it was not simulated.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_keyless_dry_run_records_the_plan_unsimulated(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        // No submitter at all, so no RPC client either: "unsimulated" is
        // this executor's shape, not a mode it chose.
        let executor = Executor::new(&store, None, true);

        let outcome = recorded(
            executor
                .execute(&plan(Priority::Normal), Settlement::DryRun, None)
                .await
                .expect("execute"),
        );

        assert!(
            !outcome.simulated,
            "there is no source account to ask the contract as"
        );
        assert!(outcome.dry_run);
        assert!(outcome.submission.is_none());

        let row = sqlx::query!(
            "SELECT tx_hash, dry_run, percent FROM fills WHERE id = $1",
            outcome.fill_id,
        )
        .fetch_one(store.pool())
        .await?;
        assert_eq!(row.tx_hash, None);
        assert!(row.dry_run, "the plan is on the audit all the same");
        assert_eq!(row.percent, 80);
        Ok(())
    }

    /// Armed: the fill is recorded before it is sent, sent on the queue with
    /// the plan's priority and the fill budget, its hash attached, and its
    /// reservation consumed.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_live_fill_is_sent_recorded_and_its_reservation_consumed(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // Round 1: the executor's own judgment, through `simulate_only` —
        // an account read and a simulation, and no fee stats, because
        // nothing on that path is ever signed or paid for.
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        // Round 2: the queue's own `prepare`, which reads the sequence
        // again from fresh state and pays for what it signs.
        script_prepare_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(&rpc, "PENDING", None, 100);
        script_transaction_success(&rpc, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let executor = Executor::new(&store, Some(submitter), false);
        let inventory = inventory();
        let fill = plan(Priority::Normal);
        let reservation = inventory
            .reserve(&fill.draft.spend)
            .expect("the wallet holds the spend");
        assert_eq!(
            inventory.available()[USDC],
            490,
            "the reservation is out of what later plans may spend"
        );

        let (queue, receiver) = SubmissionQueue::new(queue_capacity());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        // `run_queue_with` returns only once its sender is dropped, so the
        // queue is dropped inside this future the moment `execute` is done
        // with it — `auctioneer.rs`'s armed test's shape, for the same
        // borrow: a `Submitter` over this test's locals is not `'static`.
        let execute_and_drop = async {
            let outcome = executor
                .execute(&fill, Settlement::Live(reservation), Some(&queue))
                .await;
            drop(queue);
            outcome
        };
        let (outcome, ()) = tokio::join!(
            execute_and_drop,
            run_queue_with(&submitter, receiver, &shutdown_rx, retry_policy())
        );
        let outcome = recorded(outcome.expect("execute"));

        assert!(outcome.succeeded(), "this one landed");
        assert!(
            !outcome.dry_run,
            "an armed fill is recorded as dry_run = false"
        );
        let hash = match &outcome.submission {
            Some(submission) => submission.hash().to_hex(),
            None => panic!("armed, the fill goes through the queue"),
        };
        assert_eq!(hash.len(), 64, "a tx hash renders as 64 hex digits");

        let row = sqlx::query!(
            "SELECT tx_hash, dry_run FROM fills WHERE id = $1",
            outcome.fill_id,
        )
        .fetch_one(store.pool())
        .await?;
        assert_eq!(
            row.tx_hash,
            Some(hash),
            "the row recorded before the send carries the transaction it became"
        );
        assert!(!row.dry_run);

        assert!(
            inventory.reserved().values().all(|held| *held == 0),
            "the reservation is settled"
        );
        assert_eq!(
            inventory.available()[USDC],
            490,
            "consumed: the balance itself is down by the spend, not merely held"
        );
        assert_eq!(inventory.available()[XLM], 993);
        assert_eq!(
            rpc.remaining(),
            0,
            "every scripted answer was used: an over-scripted test would hide a chain call \
             this module never made"
        );
        Ok(())
    }

    /// A fill the chain failed frees what it reserved; the attempt stays on
    /// the audit, hash and all.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_fill_the_chain_failed_releases_its_reservation(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let executor = Executor::new(&store, Some(submitter), false);
        let inventory = inventory();
        let fill = plan(Priority::Normal);
        let reservation = inventory.reserve(&fill.draft.spend).expect("reserve");

        // A stand-in worker, as `service.rs`'s tests use: what this test
        // is about is what the executor does with a failure, and scripting
        // a whole failing submission would exercise the queue instead.
        let (queue, mut receiver) = SubmissionQueue::new(queue_capacity());
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Failed {
                    hash: TxHash([9_u8; 32]),
                    ledger: 1,
                    contract_error: Some(1_207),
                    result: TransactionResult {
                        fee_charged: 100,
                        result: TransactionResultResult::TxFailed(VecM::default()),
                        ext: TransactionResultExt::V0,
                    },
                }));
            }
        });

        let outcome = recorded(
            executor
                .execute(&fill, Settlement::Live(reservation), Some(&queue))
                .await
                .expect("execute"),
        );
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert!(!outcome.succeeded());
        assert!(
            matches!(outcome.submission, Some(TxOutcome::Failed { .. })),
            "the chain applied it and it failed"
        );

        let row = sqlx::query!("SELECT tx_hash FROM fills WHERE id = $1", outcome.fill_id,)
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            row.tx_hash,
            Some(TxHash([9_u8; 32]).to_hex()),
            "a transaction that consumed a sequence number is named whatever became of it"
        );

        assert!(inventory.reserved().values().all(|held| *held == 0));
        assert_eq!(
            inventory.available()[USDC],
            500,
            "released: the fill failed, so the wallet never paid it"
        );
        assert_eq!(inventory.available()[XLM], 1_000);
        assert_eq!(
            rpc.remaining(),
            0,
            "every scripted answer was used: an over-scripted test would hide a chain call \
             this module never made"
        );
        Ok(())
    }

    /// What the reservation becomes is the chain's answer, not this
    /// module's bookkeeping: a hash write that fails after a transaction
    /// landed still consumes, or the wallet's ledger would hand the next
    /// plan amounts the chain has already spent.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_store_failure_after_a_success_still_consumes(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let executor = Executor::new(&store, Some(submitter), false);
        let inventory = inventory();
        let fill = plan(Priority::Normal);
        let reservation = inventory.reserve(&fill.draft.spend).expect("reserve");

        // The row is written before the submission; closing the pool
        // between the submission and the hash write makes that second
        // write fail for real. A deleted row would not do: `attach_fill_tx`
        // answers `false` for one, which is a warning, not an error.
        let pool = store.pool().clone();
        let (queue, mut receiver) = SubmissionQueue::new(queue_capacity());
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                pool.close().await;
                let _ = queued.respond.send(Ok(TxOutcome::Succeeded {
                    hash: TxHash([5_u8; 32]),
                    ledger: 1,
                    return_value: None,
                }));
            }
        });

        let error = executor
            .execute(&fill, Settlement::Live(reservation), Some(&queue))
            .await
            .expect_err("the transaction could not be written onto its row");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert!(matches!(error, ExecutorError::Store(_)), "got {error:?}");
        assert!(
            inventory.reserved().values().all(|held| *held == 0),
            "settled, not left to the drop guard"
        );
        assert_eq!(
            inventory.available()[USDC],
            490,
            "consumed: the fill landed, whatever became of its row"
        );
        assert_eq!(inventory.available()[XLM], 993);
        assert_eq!(
            rpc.remaining(),
            0,
            "every scripted answer was used: an over-scripted test would hide a chain call \
             this module never made"
        );
        Ok(())
    }

    /// The contract's health check disagreed with the plan's projection: one
    /// re-plan, nothing recorded, the reservation released.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_health_refusal_asks_for_a_replan(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        // Live, with no queue: the refusal has a real reservation to
        // release, and nothing is armed to send what was refused.
        let executor = Executor::new(&store, Some(submitter), false);
        let inventory = inventory();
        let fill = plan(Priority::Normal);

        // 1205 is `InvalidHf`, 1224 `MinCollateralNotMet`: both say the
        // contract's own view of the filler's health disagreed with the
        // plan's projection, which a lower percent can fix.
        for code in [1_205_u32, 1_224] {
            script_simulate_prelude(&rpc, &signer, 10, 100);
            script_simulate_refused(&rpc, code, 100);
            let reservation = inventory.reserve(&fill.draft.spend).expect("reserve");

            let outcome = executor
                .execute(&fill, Settlement::Live(reservation), None)
                .await
                .expect("execute");

            assert!(
                matches!(outcome, ExecOutcome::Replan { contract_error } if contract_error == code),
                "{code} asks for one re-plan, got {outcome:?}"
            );
            assert!(
                inventory.reserved().values().all(|held| *held == 0),
                "the refusal released what it had reserved"
            );
            assert_eq!(inventory.available()[USDC], 500);
            assert_eq!(inventory.available()[XLM], 1_000);
        }

        let rows = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            rows.n,
            Some(0),
            "a refusal is not an attempt: nothing reached the audit"
        );
        assert_eq!(
            rpc.remaining(),
            0,
            "every scripted answer was used: an over-scripted test would hide a chain call \
             this module never made"
        );
        Ok(())
    }

    /// Any other refusal skips this auction until the next tick, code on the
    /// log.
    #[sqlx::test(migrations = "./migrations")]
    async fn another_refusal_skips_without_recording(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // 1212 is `AuctionInProgress`: not a judgment on the plan's
        // health, so no lower percent would help this tick.
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_refused(&rpc, 1_212, 100);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let executor = Executor::new(&store, Some(submitter), true);

        let outcome = executor
            .execute(&plan(Priority::Normal), Settlement::DryRun, None)
            .await
            .expect("execute");

        assert!(
            matches!(
                outcome,
                ExecOutcome::Refused {
                    contract_error: Some(1_212)
                }
            ),
            "got {outcome:?}"
        );
        let rows = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(rows.n, Some(0));
        assert_eq!(
            rpc.remaining(),
            0,
            "every scripted answer was used: an over-scripted test would hide a chain call \
             this module never made"
        );
        Ok(())
    }

    /// A bad sequence means someone else spent this key's sequence first: the
    /// plan is stale. The attempt was recorded before the send, so its row
    /// stays — with no hash, an armed attempt that was never named.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_bad_sequence_is_stale(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let executor = Executor::new(&store, Some(submitter), false);
        let inventory = inventory();
        let fill = plan(Priority::Normal);
        let reservation = inventory.reserve(&fill.draft.spend).expect("reserve");

        let (queue, mut receiver) = SubmissionQueue::new(queue_capacity());
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued
                    .respond
                    .send(Err(QueueError::Chain(ChainError::BadSequence)));
            }
        });

        let outcome = executor
            .execute(&fill, Settlement::Live(reservation), Some(&queue))
            .await
            .expect("execute");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert!(
            matches!(outcome, ExecOutcome::Stale),
            "a stale plan is re-planned from fresh state, never resent; got {outcome:?}"
        );
        assert!(
            inventory.reserved().values().all(|held| *held == 0),
            "nothing was sent, so nothing was spent"
        );
        assert_eq!(inventory.available()[USDC], 500);

        let row = sqlx::query!("SELECT tx_hash, dry_run FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            row.tx_hash, None,
            "an armed attempt whose transaction was never named"
        );
        assert!(!row.dry_run);
        assert_eq!(
            rpc.remaining(),
            0,
            "every scripted answer was used: an over-scripted test would hide a chain call \
             this module never made"
        );
        Ok(())
    }

    /// Spec §5: the two settlements cannot be mixed up.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_settlement_must_match_the_mode(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let inventory = inventory();
        let fill = plan(Priority::Normal);

        let dry = Executor::new(&store, Some(submitter), true);
        let reservation = inventory.reserve(&fill.draft.spend).expect("reserve");
        let error = dry
            .execute(&fill, Settlement::Live(reservation), None)
            .await
            .expect_err("a dry run spends nothing, so it holds no reservation");
        assert!(matches!(error, ExecutorError::Mode(_)), "got {error:?}");
        assert!(
            inventory.reserved().values().all(|held| *held == 0),
            "the guard released it rather than leaving it to the drop guard"
        );
        assert_eq!(inventory.available()[USDC], 500);

        let live = Executor::new(&store, Some(submitter), false);
        let error = live
            .execute(&fill, Settlement::DryRun, None)
            .await
            .expect_err("a live fill spends the wallet, so it must hold a reservation");
        assert!(matches!(error, ExecutorError::Mode(_)), "got {error:?}");

        assert!(
            rpc.received().await.is_empty(),
            "refused before any chain call at all"
        );
        let rows = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(rows.n, Some(0), "and before anything was recorded");
        Ok(())
    }

    /// A queue offered to a dry-run executor is refused before anything is
    /// simulated, recorded or enqueued — as is one offered to an executor
    /// with no signer, which could judge nothing before sending it.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_queue_offered_to_a_dry_run_executor_is_refused(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let inventory = inventory();
        let fill = plan(Priority::Normal);
        let (queue, mut receiver) = SubmissionQueue::new(queue_capacity());

        let dry = Executor::new(&store, Some(submitter), true);
        let error = dry
            .execute(&fill, Settlement::DryRun, Some(&queue))
            .await
            .expect_err("dry-run sends nothing, so the queue is refused rather than used");
        assert!(matches!(error, ExecutorError::Mode(_)), "got {error:?}");

        // The other half of the same coupling: a queue means "send this",
        // and sending what no contract was asked about is the one thing
        // this module exists to prevent.
        let keyless = Executor::new(&store, None, false);
        let reservation = inventory.reserve(&fill.draft.spend).expect("reserve");
        let error = keyless
            .execute(&fill, Settlement::Live(reservation), Some(&queue))
            .await
            .expect_err("nothing could judge this fill before it was sent");
        assert!(matches!(error, ExecutorError::Mode(_)), "got {error:?}");
        assert!(
            inventory.reserved().values().all(|held| *held == 0),
            "that guard released the reservation too"
        );

        assert!(
            rpc.calls("simulateTransaction").is_empty(),
            "refused before anything was simulated"
        );
        assert!(
            matches!(
                receiver.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "and nothing was enqueued"
        );
        let rows = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(rows.n, Some(0), "nor recorded");
        Ok(())
    }

    /// The priority and the retry budget reach the queue as the plan set
    /// them.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_fill_carries_its_priority_and_the_fill_budget(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let executor = Executor::new(&store, Some(submitter), false);
        let inventory = inventory();
        let fill = plan(Priority::High);
        let reservation = inventory.reserve(&fill.draft.spend).expect("reserve");

        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let (queue, mut receiver) = SubmissionQueue::new(queue_capacity());
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                recorder
                    .lock()
                    .expect("the recorder mutex")
                    .push((queued.submission.priority, queued.submission.retries));
                let _ = queued.respond.send(Ok(TxOutcome::Succeeded {
                    hash: TxHash([3_u8; 32]),
                    ledger: 1,
                    return_value: None,
                }));
            }
        });

        let outcome = recorded(
            executor
                .execute(&fill, Settlement::Live(reservation), Some(&queue))
                .await
                .expect("execute"),
        );
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert!(outcome.succeeded());
        assert_eq!(
            *seen.lock().expect("the recorder mutex"),
            vec![(Priority::High, FILL_RETRIES)],
            "a fill worth paying to land first, with the fill budget spec §8 gives it"
        );
        assert_eq!(
            rpc.remaining(),
            0,
            "every scripted answer was used: an over-scripted test would hide a chain call \
             this module never made"
        );
        Ok(())
    }

    /// An archived footprint cannot be judged in dry-run: only an armed
    /// submission restores.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_dry_run_that_needs_a_restore_is_refused(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_needs_restore(&rpc, 100);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let executor = Executor::new(&store, Some(submitter), true);

        let outcome = executor
            .execute(&plan(Priority::Normal), Settlement::DryRun, None)
            .await
            .expect("execute");

        assert!(
            matches!(
                outcome,
                ExecOutcome::Refused {
                    contract_error: None
                }
            ),
            "the contract said nothing about the fill itself; got {outcome:?}"
        );
        let rows = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            rows.n,
            Some(0),
            "nothing that could not be judged is recorded"
        );
        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "and restoring, which is a submission, is not this path's business"
        );
        assert_eq!(
            rpc.remaining(),
            0,
            "every scripted answer was used: an over-scripted test would hide a chain call \
             this module never made"
        );
        Ok(())
    }
}
