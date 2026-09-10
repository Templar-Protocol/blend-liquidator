//! The auctioneer: which borrowers are liquidatable, what to do about it,
//! and — once armed — doing it.
//!
//! The arithmetic is [`crate::math::liquidation`]'s; this module is the I/O
//! around it. [`Auctioneer::decide`] reads one snapshot per batch and values
//! each borrower at the tick's close time — the same instant the tracker
//! valued them at, so the decision and the stored health factor cannot
//! disagree — and answers with a [`Decision`] per user. [`Auctioneer::act`]
//! turns one decision into an operation, lets the contract judge it through
//! simulation, records what was decided, and — only when a submission queue
//! is given — sends it.
//!
//! `decide` needs no signer at all: a [`Decision`] is an answer, not an
//! action, and this module's `decide` tests drive it through a scripted RPC
//! with none configured. `act` needs one only to simulate *against* — as a
//! source account, never as a signature: every simulation this module makes
//! goes through [`Submitter::simulate_only`], which builds the transaction
//! unsigned and neither signs, restores nor sends. The signing happens
//! behind the submission queue, and only when one is given. With no signer
//! configured at all, `act` records the plan the contract was never asked
//! about and says so — see `act`'s own doc.

use std::collections::{BTreeMap, BTreeSet};

use stellar_xdr::Operation;

use crate::chain::pool::{bad_debt_op, new_auction_op, PoolReader, PoolSnapshot};
use crate::chain::rpc::RpcClient;
use crate::chain::tx::{Judgment, Priority, Submitter};
use crate::chain::xdr::{AuctionType, FillPercent};
use crate::chain::{ChainError, TxHash, TxOutcome};
use crate::ledger::LedgerTick;
use crate::math::liquidation::{plan_liquidation, position_values, LiquidationPlan};
use crate::math::{mul_floor, MathError, Reserve, SCALAR_7};
use crate::queue::{QueueError, Submission, SubmissionQueue};
use crate::store::{CreationKind, CreationRecord, Store, StoreError, TrackedUser};

/// `PoolError::InvalidLiqTooLarge`: the liquidation would leave the
/// borrower's health factor at or above `1.15`, so the percent is too high.
const INVALID_LIQ_TOO_LARGE: u32 = 1_213;

/// `PoolError::InvalidLiqTooSmall`: the liquidation would leave the
/// borrower's health factor below `1.03`, so the percent is too low. The
/// contract only raises this for a partial liquidation.
const INVALID_LIQ_TOO_SMALL: u32 = 1_214;

/// Why a borrower was not acted on. Every skip is a decision, and a decision
/// worth naming: an operator asking "why did nothing happen" is asking about
/// exactly this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Above `LIQ_HF_THRESHOLD`.
    Healthy,
    /// An auction for this user already exists.
    AuctionOpen,
    /// Liquidatable, but no selection of positions closes the excess.
    NoPlan,
    /// The filler's or the auctioneer's own account.
    OwnAccount,
    /// No liabilities: nothing to liquidate.
    NoLiabilities,
}

/// What to do about one borrower.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Create this auction.
    Liquidate(LiquidationPlan),
    /// Move this user's debt to the backstop.
    BadDebt,
    /// Nothing, for this reason.
    Skip(SkipReason),
}

/// The batch-level policy [`Auctioneer::decide`] reads: the thresholds a
/// health factor is judged against, and the accounts it must never name.
#[derive(Debug, Clone)]
pub struct AuctioneerConfig {
    /// The health factor at or below which a borrower is liquidatable, 7
    /// decimals (`LIQ_HF_THRESHOLD`).
    pub liquidation_health_factor: i128,
    /// The health factor a liquidation aims to leave the borrower at, 7
    /// decimals (`TARGET_HF`).
    pub target_health_factor: i128,
    /// How many times a rejected percent is adjusted against the contract's
    /// own answer before the borrower is left until the next recheck.
    /// `decide` does not read this: a plan's own percent is never
    /// resimulated before it is returned. `act`'s percent-adjustment loop is
    /// what bounds itself by it.
    pub plan_iterations: u32,
    /// The bot's own accounts, filler included. The contract refuses to let
    /// the bot liquidate itself, but in dry-run there is no contract to
    /// refuse, and a bot that would have tried is one that will try when
    /// armed.
    pub own_addresses: BTreeSet<String>,
}

/// A failure deciding who is liquidatable, or acting on that decision.
#[derive(Debug, thiserror::Error)]
pub enum AuctioneerError {
    /// Reading or writing the chain failed.
    #[error("chain: {0}")]
    Chain(#[from] ChainError),
    /// Reading or writing the store failed.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// Checked arithmetic on chain values failed.
    #[error("math: {0}")]
    Math(#[from] MathError),
    /// The submission queue could not carry a creation through.
    #[error("queue: {0}")]
    Queue(#[from] QueueError),
}

/// One auctioneer submission, whether or not it was sent: the same shape
/// [`CreationRecord`] persists, plus what simulation and the queue found out
/// that the store's `creations` table has no column for (see `act`'s doc).
#[derive(Debug, Clone)]
pub struct CreationOutcome {
    /// What kind of submission this was.
    pub kind: CreationKind,
    /// The account acted on.
    pub account: String,
    /// The percent the auction named; `None` for bad debt, which has none.
    pub percent: Option<FillPercent>,
    /// Whether the contract was actually asked. `false` only when no
    /// auctioneer key is configured at all — there is then no source
    /// account to simulate against, dry-run or not.
    pub simulated: bool,
    /// The `creations` row this was written as, written before anything was
    /// submitted. What a caller reconciling an unresolved submission names
    /// the row by.
    pub creation_id: i64,
    /// What the chain made of it: `None` in dry-run, where nothing was
    /// sent.
    ///
    /// Every terminal state comes back here, not just the good one. A
    /// caller must match on it rather than read `Some` as success:
    /// [`TxOutcome::Failed`] consumed a sequence number and charged a fee,
    /// [`TxOutcome::Expired`] provably never applied, and
    /// [`TxOutcome::Unknown`] may still land — it carries the `sequence`
    /// and `window` [`Submitter::wait_for`] resumes from, which is the only
    /// way to find out which.
    pub submission: Option<TxOutcome>,
}

impl CreationOutcome {
    /// Whether this went through the submission queue at all. `false` in
    /// dry-run, and never `true` without a transaction hash.
    #[must_use]
    pub fn submitted(&self) -> bool {
        self.submission.is_some()
    }

    /// The transaction this became, once there is one. Every
    /// [`TxOutcome`] carries a hash, a failed or unresolved one included:
    /// a transaction that consumed a sequence number is worth naming
    /// whatever became of it.
    #[must_use]
    pub fn tx_hash(&self) -> Option<TxHash> {
        self.submission.as_ref().map(outcome_hash)
    }

    /// Whether the chain applied it and it succeeded. `false` for a
    /// dry-run, a failure, an expiry and an unresolved outcome alike — the
    /// three that are not successes are not interchangeable, so a caller
    /// that needs to tell them apart reads `submission` itself.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(self.submission, Some(TxOutcome::Succeeded { .. }))
    }
}

/// Decides who is liquidatable, against one store and one chain client, and
/// acts on that decision through one signer's simulation.
#[derive(Debug, Clone)]
pub struct Auctioneer<'a> {
    rpc: &'a RpcClient,
    store: &'a Store,
    config: AuctioneerConfig,
    /// Simulates and signs for `act`. `None` when no auctioneer key is
    /// configured: `decide` never needs one, and `act` then records a plan
    /// unsimulated rather than asking a contract with no source account to
    /// ask as.
    submitter: Option<Submitter<'a>>,
}

/// Accrues a clone of the snapshot's reserves to `close_time`, once for the
/// whole batch.
///
/// `PoolSnapshot::reserves` is stored, not accrued. [`position_values`]
/// needs the accrued numbers to price each position individually, the same
/// instant [`PoolSnapshot::position_data`] accrues its own clone to for the
/// health-factor gate — accruing a second time here, to the same
/// `close_time`, is what keeps the two agreeing. Doing that accrual inside
/// the per-user loop would accrue the whole reserve set again for every
/// borrower the batch finds liquidatable; a full scan of a thousand
/// borrowers is exactly the cadence a repeated accrual like that turns into
/// a stall, so this runs once, before the loop, and every user's plan
/// borrows from the one result.
fn accrue_reserves(
    snapshot: &PoolSnapshot,
    close_time: u64,
) -> Result<BTreeMap<u32, Reserve>, MathError> {
    let mut reserves = snapshot.reserves.clone();
    for reserve in reserves.values_mut() {
        reserve.accrue(snapshot.instance.config.bstop_rate, close_time)?;
    }
    Ok(reserves)
}

impl<'a> Auctioneer<'a> {
    /// An auctioneer reading through `rpc`, checking open auctions against
    /// `store`, judging against `config`, and — when `submitter` is given —
    /// simulating and signing `act`'s operations with it.
    #[must_use]
    pub fn new(
        rpc: &'a RpcClient,
        store: &'a Store,
        config: AuctioneerConfig,
        submitter: Option<Submitter<'a>>,
    ) -> Self {
        Self {
            rpc,
            store,
            config,
            submitter,
        }
    }

    /// Decides every one of `users`, against one snapshot read for the
    /// whole batch: the decision for a thousand borrowers is one
    /// `getLedgerEntries`, not a thousand. Returns one `(account,
    /// Decision)` per user that could be decided, in the order `users` gave
    /// them — one user's failure is never allowed to withhold every other
    /// user's decision.
    ///
    /// A store failure is fatal to the whole batch: it means the bot cannot
    /// trust what it read about who to check or what is already open, and
    /// nothing downstream of that is safe to act on. A chain or math failure
    /// is only ever this one user's — a stale positions entry naming a
    /// reserve the snapshot no longer has, say — so it is logged with the
    /// account and skips that user, leaving the rest of the batch to
    /// proceed; a poisoned row must not stall every other borrower's tick.
    pub async fn decide(
        &self,
        pool: &str,
        users: &[TrackedUser],
        tick: LedgerTick,
    ) -> Result<Vec<(String, Decision)>, AuctioneerError> {
        if users.is_empty() {
            return Ok(Vec::new());
        }
        let accounts: Vec<&str> = users.iter().map(|user| user.account.as_str()).collect();
        let snapshot = PoolReader::new(self.rpc, pool).snapshot(&accounts).await?;
        let reserves = accrue_reserves(&snapshot, tick.close_time)?;

        let mut decisions = Vec::with_capacity(users.len());
        for user in users {
            match self
                .decide_one(pool, &user.account, &snapshot, &reserves, tick)
                .await
            {
                Ok(decision) => decisions.push((user.account.clone(), decision)),
                Err(AuctioneerError::Store(error)) => return Err(AuctioneerError::Store(error)),
                Err(error) => {
                    tracing::warn!(
                        pool,
                        account = %user.account,
                        %error,
                        "could not decide this borrower; skipping it for this batch"
                    );
                }
            }
        }
        Ok(decisions)
    }

    /// One borrower's decision against an already-read `snapshot` and its
    /// already-accrued `reserves`. The six steps are the module's whole
    /// policy; see the module doc for why each exists.
    async fn decide_one(
        &self,
        pool: &str,
        account: &str,
        snapshot: &PoolSnapshot,
        reserves: &BTreeMap<u32, Reserve>,
        tick: LedgerTick,
    ) -> Result<Decision, AuctioneerError> {
        if self.config.own_addresses.contains(account) {
            return Ok(Decision::Skip(SkipReason::OwnAccount));
        }
        // The store, not the chain: the tracker maintains this table from
        // the pool's own events, and a chain read per user is exactly the
        // round trip batching this decision exists to avoid.
        if self
            .store
            .auction(pool, account, AuctionType::UserLiquidation)
            .await?
            .is_some()
        {
            return Ok(Decision::Skip(SkipReason::AuctionOpen));
        }
        let Some(data) = snapshot.position_data(account, tick.close_time)? else {
            return Ok(Decision::Skip(SkipReason::NoLiabilities));
        };
        if data.liability_base > 0 && data.collateral_base == 0 {
            return Ok(Decision::BadDebt);
        }
        // `health_factor` is `None` only when there are no liabilities. A
        // positions entry can be non-empty on collateral or plain supply
        // alone, which step 3 above does not catch, so this is a second,
        // real path to the same answer rather than an unreachable guard.
        let Some(health) = data.health_factor()? else {
            return Ok(Decision::Skip(SkipReason::NoLiabilities));
        };
        // Normalised to 7 decimals exactly as the tracker normalises the
        // value it stores, so the two are comparable against one threshold
        // regardless of the oracle's own decimals.
        let normalized = mul_floor(health, SCALAR_7, data.scalar)?;
        if normalized > self.config.liquidation_health_factor {
            return Ok(Decision::Skip(SkipReason::Healthy));
        }
        // `position_data` already proved this account holds a positions
        // entry; this is the same lookup, kept fallible rather than
        // unwrapped.
        let Some(positions) = snapshot.positions.get(account) else {
            return Ok(Decision::Skip(SkipReason::NoLiabilities));
        };
        let (collateral, liabilities) = position_values(reserves, &snapshot.prices, positions)?;
        let plan = plan_liquidation(
            &data,
            &collateral,
            &liabilities,
            self.config.target_health_factor,
            snapshot.instance.config.max_positions,
        )?;
        Ok(match plan {
            Some(plan) => Decision::Liquidate(plan),
            None => Decision::Skip(SkipReason::NoPlan),
        })
    }

    /// Acts on one decision: builds the operation, lets the contract judge
    /// the percent, then records what it decided and — when armed —
    /// submits it.
    ///
    /// `submit` is `None` in dry-run, and that is the whole of the
    /// difference: the same *simulation* runs either way, through
    /// [`Submitter::simulate_only`], which builds unsigned and neither
    /// signs, restores nor sends. So the percent recorded in dry-run is one
    /// the contract would have accepted rather than one the bot merely
    /// hoped for, and it costs this key no signature and no sequence
    /// number. A recorded creation that could not have been made would make
    /// the audit worse than useless — unless there is no auctioneer key
    /// configured at all, in which case there is no source account to
    /// simulate against and the plan is recorded unsimulated,
    /// [`CreationOutcome::simulated`] and the log line both saying so.
    ///
    /// The audit is written **before** the submission, never after: the
    /// row, and the structured log event that mirrors it, both exist before
    /// the operation is handed to the queue, and the transaction's hash is
    /// attached to that row once there is one. A crash at any point
    /// therefore leaves a row that is at worst incomplete, never a
    /// transaction on chain that nothing recorded — and a submission the
    /// queue refused leaves the same row a dry-run would, rather than
    /// nothing at all.
    ///
    /// A [`Decision::Skip`] acts on nothing and returns `Ok(None)`, with the
    /// reason on a debug line: an operator asking "why did nothing happen"
    /// is asking about exactly this. Bad debt is submitted without the
    /// percent walk below: `bad_debt(user)` takes no percent, and the
    /// contract sizes it itself.
    ///
    /// # Errors
    ///
    /// This is one borrower's function, so **every** failure it reports is
    /// one borrower's: a store write, a chain read the bot could not even
    /// make, or a queue that refused the submission. Isolating them is the
    /// caller's responsibility — [`Auctioneer::decide`] does the same job
    /// for the deciding half of the tick — and a caller that lets an `Err`
    /// from one account end the batch lets one refused borrower stop every
    /// other borrower being acted on. The one failure worth ending a batch
    /// over is [`AuctioneerError::Store`], for the reason `decide` gives.
    pub async fn act(
        &self,
        pool: &str,
        account: &str,
        decision: &Decision,
        tick: LedgerTick,
        submit: Option<&SubmissionQueue>,
    ) -> Result<Option<CreationOutcome>, AuctioneerError> {
        let (kind, percent, bid, lot, operation, simulated) = match decision {
            Decision::Skip(reason) => {
                tracing::debug!(pool, account, reason = ?reason, "skip: no action taken");
                return Ok(None);
            }
            Decision::BadDebt => {
                let operation = bad_debt_op(pool, account).map_err(ChainError::from)?;
                let Some(simulated) = self.accept_bad_debt(pool, account, &operation).await? else {
                    return Ok(None);
                };
                (
                    CreationKind::BadDebt,
                    None,
                    Vec::new(),
                    Vec::new(),
                    operation,
                    simulated,
                )
            }
            Decision::Liquidate(plan) => {
                let Some((percent, operation, simulated)) =
                    self.accept_percent(pool, account, plan).await?
                else {
                    return Ok(None);
                };
                (
                    CreationKind::Auction,
                    Some(percent),
                    plan.bid.clone(),
                    plan.lot.clone(),
                    operation,
                    simulated,
                )
            }
        };

        // Written first, and logged first: the spec's audit trail is meant
        // to survive a database loss, so the row and the log line carry the
        // same fields, and both precede the submission they describe.
        let record = CreationRecord {
            kind,
            pool: pool.to_string(),
            account: account.to_string(),
            percent,
            bid,
            lot,
            ledger: tick.sequence,
            dry_run: submit.is_none(),
            tx_hash: None,
        };
        let creation_id = self.store.record_creation(&record).await?;
        tracing::info!(
            creation_id,
            pool,
            account,
            kind = ?record.kind,
            percent = record.percent.map(FillPercent::get),
            bid = ?record.bid,
            lot = ?record.lot,
            ledger = record.ledger,
            dry_run = record.dry_run,
            simulated,
            "creation recorded"
        );

        let submission = match submit {
            Some(queue) => Some(
                self.submit_recorded(queue, creation_id, &record, operation)
                    .await?,
            ),
            None => None,
        };

        Ok(Some(CreationOutcome {
            kind,
            account: account.to_string(),
            percent,
            simulated,
            creation_id,
            submission,
        }))
    }

    /// Whether the contract accepts `bad_debt` for this borrower, and
    /// whether it was actually asked: `Some(true)` accepted and simulated,
    /// `Some(false)` accepted because there was nothing to ask with, `None`
    /// skip this borrower.
    ///
    /// With no auctioneer key there is no source account to simulate
    /// against, so the operation is recorded unsimulated — the mirror of
    /// `accept_percent`'s own no-key answer, and honest for the same
    /// reason. Otherwise the contract judges it through
    /// [`Submitter::simulate_only`], which builds unsigned: a dry-run may
    /// take this path precisely because nothing in it signs or sends.
    ///
    /// A footprint holding archived entries is a skip, not a refusal:
    /// restoring it is a submission, and this is not the code that means to
    /// make one.
    async fn accept_bad_debt(
        &self,
        pool: &str,
        account: &str,
        operation: &Operation,
    ) -> Result<Option<bool>, AuctioneerError> {
        let Some(submitter) = self.submitter.as_ref() else {
            tracing::debug!(
                pool,
                account,
                "no auctioneer key configured; recording bad debt unsimulated"
            );
            return Ok(Some(false));
        };
        match submitter.simulate_only(operation).await? {
            Judgment::Accepted => Ok(Some(true)),
            Judgment::Refused {
                contract_error,
                message,
            } => {
                tracing::debug!(
                    pool,
                    account,
                    contract_error,
                    %message,
                    "bad debt refused by simulation; skipping"
                );
                Ok(None)
            }
            Judgment::NeedsRestore => {
                tracing::info!(
                    pool,
                    account,
                    "bad debt could not be judged: its footprint holds archived entries, \
                     which only an armed submission restores; skipping"
                );
                Ok(None)
            }
        }
    }

    /// Hands an already-recorded creation to the queue, then attaches the
    /// transaction it became to the row `creation_id` names.
    ///
    /// The order is the audit's: the row exists before the submission, so
    /// the hash is a second write and a row with `dry_run = false` and no
    /// hash is an armed attempt whose transaction was never named — either
    /// never submitted or submitted with the outcome unrecorded. A row that
    /// has gone missing between the two writes is logged, not raised —
    /// nothing deletes a creation, and failing here would report a
    /// submission that has already happened as one that did not.
    async fn submit_recorded(
        &self,
        queue: &SubmissionQueue,
        creation_id: i64,
        record: &CreationRecord,
        operation: Operation,
    ) -> Result<TxOutcome, AuctioneerError> {
        let label = format!("{:?} {} on {}", record.kind, record.account, record.pool);
        let outcome = queue
            .enqueue(Submission {
                operation,
                priority: Priority::Normal,
                label,
            })
            .await?;
        let hash = outcome_hash(&outcome).to_hex();
        tracing::info!(
            creation_id,
            pool = %record.pool,
            account = %record.account,
            tx_hash = %hash,
            status = outcome_status(&outcome),
            "creation submitted"
        );
        if !self.store.attach_creation_tx(creation_id, &hash).await? {
            tracing::warn!(
                creation_id,
                tx_hash = %hash,
                "no creation row to attach this transaction to"
            );
        }
        Ok(outcome)
    }

    /// Walks a liquidation plan's percent to one the contract accepts.
    ///
    /// With no auctioneer key configured there is no source account to
    /// simulate against, so the plan's own percent is returned unsimulated
    /// at once — see `act`'s doc for why that is the honest answer rather
    /// than a silent skip of the walk.
    ///
    /// Otherwise, each attempt simulates `new_auction` at the current
    /// percent through [`Submitter::simulate_only`], which builds the
    /// transaction unsigned: the walk may run in dry-run precisely because
    /// nothing in it signs, restores or sends.
    /// `InvalidLiqTooSmall` (1214, the post-liquidation health factor below
    /// `1.03`) raises the percent by one; `InvalidLiqTooLarge` (1213, at or
    /// above `1.15`) lowers it by one — `checked_add`/`checked_sub` and a
    /// [`FillPercent`] range check rather than raw arithmetic, so the walk
    /// can never wrap past `1..=100` and a percent that would leave that
    /// range ends the walk at once instead of retrying a value the contract
    /// could not possibly accept. Any other contract error ends the walk
    /// immediately too: adjusting a percent against, say,
    /// `AuctionInProgress` would be more round trips to learn what the first
    /// one already said. The walk also ends, giving up, after
    /// `plan_iterations` attempts — a contract that refuses forever must
    /// cost this one borrower a bounded number of simulations, not the
    /// batch's whole cadence.
    ///
    /// A simulation that comes back needing archived entries restored is
    /// none of those: it is not a judgment on the percent at all, so the
    /// percent is not adjusted and the walk does not retry. The borrower is
    /// simply one this bot cannot judge right now, and the restore belongs
    /// to a submission that means to spend a sequence number on it.
    ///
    /// This never recomputes the plan's asset lists: `bounded_plan` may
    /// have trimmed the selection to the pool's `max_positions` after the
    /// percent was already chosen for the untrimmed one, so the percent
    /// this loop starts from can already be stale relative to the assets it
    /// will actually name. That is an accepted approximation precisely
    /// because this loop corrects it against the contract's own answer.
    async fn accept_percent(
        &self,
        pool: &str,
        account: &str,
        plan: &LiquidationPlan,
    ) -> Result<Option<(FillPercent, Operation, bool)>, AuctioneerError> {
        let bid: Vec<&str> = plan.bid.iter().map(String::as_str).collect();
        let lot: Vec<&str> = plan.lot.iter().map(String::as_str).collect();
        let build = |percent: FillPercent| {
            new_auction_op(
                pool,
                AuctionType::UserLiquidation,
                account,
                &bid,
                &lot,
                percent,
            )
            .map_err(ChainError::from)
            .map_err(AuctioneerError::from)
        };

        let Some(submitter) = self.submitter.as_ref() else {
            tracing::debug!(
                pool,
                account,
                "no auctioneer key configured; recording the plan unsimulated"
            );
            return Ok(Some((plan.percent, build(plan.percent)?, false)));
        };

        let mut percent = plan.percent;
        for _ in 0..self.config.plan_iterations {
            let operation = build(percent)?;
            match submitter.simulate_only(&operation).await? {
                Judgment::Accepted => return Ok(Some((percent, operation, true))),
                Judgment::Refused {
                    contract_error: Some(INVALID_LIQ_TOO_SMALL),
                    ..
                } => {
                    if let Some(next) = percent
                        .get()
                        .checked_add(1)
                        .and_then(|value| FillPercent::try_from(value).ok())
                    {
                        percent = next;
                    } else {
                        tracing::debug!(
                            pool,
                            account,
                            "no larger percent to try against InvalidLiqTooSmall; skipping"
                        );
                        return Ok(None);
                    }
                }
                Judgment::Refused {
                    contract_error: Some(INVALID_LIQ_TOO_LARGE),
                    ..
                } => {
                    if let Some(next) = percent
                        .get()
                        .checked_sub(1)
                        .and_then(|value| FillPercent::try_from(value).ok())
                    {
                        percent = next;
                    } else {
                        tracing::debug!(
                            pool,
                            account,
                            "no smaller percent to try against InvalidLiqTooLarge; skipping"
                        );
                        return Ok(None);
                    }
                }
                Judgment::Refused {
                    contract_error,
                    message,
                } => {
                    tracing::debug!(
                        pool,
                        account,
                        contract_error,
                        %message,
                        "liquidation refused by simulation; skipping"
                    );
                    return Ok(None);
                }
                Judgment::NeedsRestore => {
                    tracing::info!(
                        pool,
                        account,
                        percent = percent.get(),
                        "this borrower could not be judged: the auction's footprint holds \
                         archived entries, which only an armed submission restores; skipping \
                         without adjusting the percent"
                    );
                    return Ok(None);
                }
            }
        }
        tracing::warn!(
            pool,
            account,
            iterations = self.config.plan_iterations,
            "percent adjustment exhausted its iterations; skipping until the next recheck"
        );
        Ok(None)
    }
}

/// What a submitted transaction's terminal state is called on a log line.
/// A short label rather than `TxOutcome`'s `Debug`, whose `Failed` variant
/// carries a whole decoded `TransactionResult`.
fn outcome_status(outcome: &TxOutcome) -> &'static str {
    match outcome {
        TxOutcome::Succeeded { .. } => "succeeded",
        TxOutcome::Failed { .. } => "failed",
        TxOutcome::Expired { .. } => "expired",
        TxOutcome::Unknown { .. } => "unknown",
    }
}

/// The hash every [`TxOutcome`] variant carries, whatever the transaction's
/// terminal state: even a failed, expired or unresolved transaction
/// consumed a sequence number and is worth recording by its hash.
fn outcome_hash(outcome: &TxOutcome) -> TxHash {
    match outcome {
        TxOutcome::Succeeded { hash, .. }
        | TxOutcome::Failed { hash, .. }
        | TxOutcome::Expired { hash, .. }
        | TxOutcome::Unknown { hash, .. } => *hash,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::{json, Value};
    use stellar_xdr::{
        ContractDataDurability, ContractDataEntry, ExtensionPoint, InvokeHostFunctionResult,
        LedgerEntryData, LedgerKey, LedgerKeyAccount, OperationResult, OperationResultTr,
        TransactionResultResult, VecM,
    };

    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::{
        account_entry_b64, diagnostic_error_b64, meta_v4_b64, result_b64, scval_b64,
        transaction_data_b64, ScriptedRpc,
    };
    use crate::chain::signer::{Network, Signer};
    use crate::chain::tx::TxConfig;
    use crate::chain::xdr::encode::{
        address, i128_val, map, sc_address, symbol, to_base64, vec as sc_vec,
    };
    use crate::chain::xdr::keys;
    use crate::fixture::{mainnet_fixed_v2, text};
    use crate::harness::{self, GOLDEN_HEALTH, POOL, USER_ONE, USER_TWO};
    use crate::math::liquidation::PositionValue;
    use crate::math::PositionData;
    use crate::queue::run_queue;
    use crate::store::TrackedAuction;

    /// A bid or lot asset for the open-auction test: any reserve address
    /// works, since that test never reaches the math that would care which
    /// one.
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    /// A valid, distinct account strkey from `byte` alone — no real key
    /// behind it, and none needed: every test here only ever reads chain
    /// state through the scripted RPC, never signs anything.
    fn synthetic_account(byte: u8) -> String {
        stellar_strkey::ed25519::PublicKey([byte; 32]).to_string()
    }

    /// The brief's own default thresholds
    /// (`LIQ_HF_THRESHOLD=0.998`, `TARGET_HF=1.06`, `PLAN_ITERATIONS=5`),
    /// with `own_addresses` supplied per test.
    fn config(own_addresses: BTreeSet<String>) -> AuctioneerConfig {
        AuctioneerConfig {
            liquidation_health_factor: 9_980_000,
            target_health_factor: 10_600_000,
            plan_iterations: 5,
            own_addresses,
        }
    }

    /// A `TrackedUser` naming only `account`: `decide` re-derives
    /// everything else from the fresh snapshot, so no other field is read.
    fn tracked_user(account: &str) -> TrackedUser {
        TrackedUser {
            pool: POOL.to_string(),
            account: account.to_string(),
            health_factor: 0,
            collateral: BTreeMap::new(),
            liabilities: BTreeMap::new(),
            updated_ledger: 0,
            recheck_ledger: None,
        }
    }

    fn entry(key: &stellar_xdr::LedgerKey, xdr: &str) -> Value {
        json!({
            "key": to_base64(key).expect("key"),
            "xdr": xdr,
            "lastModifiedLedgerSeq": 1,
            "liveUntilLedgerSeq": 99_999_999_u32,
        })
    }

    fn simulation(return_xdr: &str, ledger: u32) -> Value {
        json!({
            "transactionData": crate::chain::script::transaction_data_b64(1),
            "events": [],
            "minResourceFee": "1",
            "results": [{"auth": [], "xdr": return_xdr}],
            "latestLedger": ledger,
        })
    }

    /// A `Positions` ledger entry built by hand: `collateral` and
    /// `liabilities` are `(reserve index, token amount)` pairs, `supply`
    /// always empty — nothing here reads plain supply.
    fn positions_entry_xdr(
        account: &str,
        collateral: &[(u32, i128)],
        liabilities: &[(u32, i128)],
    ) -> String {
        let side = |amounts: &[(u32, i128)]| {
            map(amounts
                .iter()
                .map(|(index, amount)| (stellar_xdr::ScVal::U32(*index), i128_val(*amount)))
                .collect())
            .expect("side map")
        };
        let value = map(vec![
            (symbol("collateral").expect("symbol"), side(collateral)),
            (symbol("liabilities").expect("symbol"), side(liabilities)),
            (symbol("supply").expect("symbol"), side(&[])),
        ])
        .expect("positions map");
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(POOL).expect("pool address"),
            key: sc_vec(vec![
                symbol("Positions").expect("symbol"),
                address(account).expect("address"),
            ])
            .expect("key"),
            durability: ContractDataDurability::Persistent,
            val: value,
        });
        to_base64(&entry).expect("encodes")
    }

    /// Scripts one snapshot for one account exactly as
    /// `harness::script_snapshot` does — same shape read, same reserve
    /// configs and data, same oracle prices, all straight from the fixture
    /// — except that the account gets a hand-built positions entry rather
    /// than whatever (if anything) the fixture itself holds for it.
    ///
    /// This is what stands in for `harness::script_snapshot` in the
    /// liquidatable and bad-debt tests below. The fixture's own two
    /// borrowers each hold their collateral and liability on the very same
    /// reserve (see `chain::xdr::decode`'s golden-health test), so their
    /// health factor is invariant to any single reserve's price — neither
    /// can be pushed underwater by moving a price, only by a positions
    /// entry that spans two reserves, which the fixture does not have. The
    /// prices and reserve configuration stay the fixture's real, attested
    /// ones throughout; only the position is fabricated.
    fn script_snapshot_with_positions(
        rpc: &ScriptedRpc,
        account: &str,
        collateral: &[(u32, i128)],
        liabilities: &[(u32, i128)],
    ) {
        let fixture = mainnet_fixed_v2();
        let ledger = fixture["ledger"].as_u64().expect("ledger");
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                entry(&keys::instance(POOL).expect("key"), text(&fixture, &["instance_entry_xdr"])),
                entry(&keys::reserve_list(POOL).expect("key"), text(&fixture, &["res_list_entry_xdr"])),
            ]}),
        );
        let mut entries = Vec::new();
        for reserve in fixture["reserves"].as_array().expect("reserves") {
            let asset = reserve["asset"].as_str().expect("asset");
            entries.push(entry(
                &keys::reserve_config(POOL, asset).expect("key"),
                reserve["config_entry_xdr"].as_str().expect("config"),
            ));
            entries.push(entry(
                &keys::reserve_data(POOL, asset).expect("key"),
                reserve["data_entry_xdr"].as_str().expect("data"),
            ));
        }
        entries.push(entry(
            &keys::positions(POOL, account).expect("key"),
            &positions_entry_xdr(account, collateral, liabilities),
        ));
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": entries}),
        );
        let ledger = u32::try_from(ledger).expect("ledger fits");
        rpc.expect(
            "simulateTransaction",
            simulation(text(&fixture, &["oracle_decimals_return_xdr"]), ledger),
        );
        for reserve in fixture["reserves"].as_array().expect("reserves") {
            rpc.expect(
                "simulateTransaction",
                simulation(
                    reserve["lastprice_return_xdr"].as_str().expect("price"),
                    ledger,
                ),
            );
        }
    }

    /// One account's fabricated `(collateral, liabilities)` positions, each
    /// a slice of `(reserve index, token amount)` pairs — the per-account
    /// element `script_snapshot_with_many_positions` takes one of per
    /// borrower.
    type FabricatedPositions<'a> = (&'a str, &'a [(u32, i128)], &'a [(u32, i128)]);

    /// The multi-account form of `script_snapshot_with_positions`: one
    /// snapshot naming several accounts' hand-built positions at once, for
    /// tests that need more than one fabricated borrower in the same batch.
    fn script_snapshot_with_many_positions(
        rpc: &ScriptedRpc,
        accounts: &[FabricatedPositions<'_>],
    ) {
        let fixture = mainnet_fixed_v2();
        let ledger = fixture["ledger"].as_u64().expect("ledger");
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                entry(&keys::instance(POOL).expect("key"), text(&fixture, &["instance_entry_xdr"])),
                entry(&keys::reserve_list(POOL).expect("key"), text(&fixture, &["res_list_entry_xdr"])),
            ]}),
        );
        let mut entries = Vec::new();
        for reserve in fixture["reserves"].as_array().expect("reserves") {
            let asset = reserve["asset"].as_str().expect("asset");
            entries.push(entry(
                &keys::reserve_config(POOL, asset).expect("key"),
                reserve["config_entry_xdr"].as_str().expect("config"),
            ));
            entries.push(entry(
                &keys::reserve_data(POOL, asset).expect("key"),
                reserve["data_entry_xdr"].as_str().expect("data"),
            ));
        }
        for (account, collateral, liabilities) in accounts {
            entries.push(entry(
                &keys::positions(POOL, account).expect("key"),
                &positions_entry_xdr(account, collateral, liabilities),
            ));
        }
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": entries}),
        );
        let ledger = u32::try_from(ledger).expect("ledger fits");
        rpc.expect(
            "simulateTransaction",
            simulation(text(&fixture, &["oracle_decimals_return_xdr"]), ledger),
        );
        for reserve in fixture["reserves"].as_array().expect("reserves") {
            rpc.expect(
                "simulateTransaction",
                simulation(
                    reserve["lastprice_return_xdr"].as_str().expect("price"),
                    ledger,
                ),
            );
        }
    }

    /// A signing key for `act`'s tests: no real funds and no relation to
    /// any fixture account. `act` only ever simulates and signs against
    /// what the scripted RPC hands back for it, never real chain state.
    fn auctioneer_signer() -> Signer {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7_u8; 32]);
        let secret = stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string();
        Signer::from_secret(&secret).expect("signer")
    }

    /// Fee and polling policy for `act`'s tests: short enough that a
    /// scripted `getTransaction` answer is read back without any real wait.
    fn tx_config() -> TxConfig {
        TxConfig {
            poll_interval: std::time::Duration::from_millis(1),
            send_retry_pause: std::time::Duration::from_millis(1),
            wait_cap: std::time::Duration::from_millis(200),
            ..TxConfig::new(100, 200, 3)
        }
    }

    /// One `Submitter::simulate_only` attempt's prelude: the source
    /// account's entry, and nothing else. A simulate-only call needs no fee
    /// stats — it never assembles a transaction to pay for — so a test that
    /// scripts one and sees it consumed would be scripting the signing path
    /// by mistake.
    fn script_simulate_prelude(rpc: &ScriptedRpc, signer: &Signer, sequence: i64, ledger: u32) {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: signer.account_id(),
        });
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                {"key": to_base64(&key).expect("key"),
                 "xdr": account_entry_b64(signer.address(), sequence),
                 "lastModifiedLedgerSeq": 1}
            ]}),
        );
    }

    /// One `Submitter::prepare` attempt's prelude: the account read above,
    /// plus the fee stats a transaction that will actually be signed and
    /// paid for needs. Only the queue's own submission path takes this
    /// route.
    fn script_prepare_prelude(rpc: &ScriptedRpc, signer: &Signer, sequence: i64, ledger: u32) {
        script_simulate_prelude(rpc, signer, sequence, ledger);
        rpc.expect(
            "getFeeStats",
            json!({"sorobanInclusionFee": {"p70": "100", "p90": "100"},
                   "inclusionFee": {"p70": "100", "p90": "100"}, "latestLedger": ledger}),
        );
    }

    /// The `simulateTransaction` answer for an attempt the contract accepts.
    fn script_simulate_accepted(rpc: &ScriptedRpc, ledger: u32) {
        rpc.expect(
            "simulateTransaction",
            json!({"transactionData": transaction_data_b64(10),
                   "events": [],
                   "minResourceFee": "10",
                   "results": [{"auth": [], "xdr": scval_b64(&stellar_xdr::ScVal::Void)}],
                   "latestLedger": ledger}),
        );
    }

    /// The `simulateTransaction` answer for an attempt the contract refuses
    /// with `code`, both in the diagnostic events and the error message —
    /// exactly the two places `contract_error_in_events` and
    /// `contract_error_in_message` read it from.
    fn script_simulate_refused(rpc: &ScriptedRpc, code: u32, ledger: u32) {
        rpc.expect(
            "simulateTransaction",
            json!({"error": format!("HostError: Error(Contract, #{code})"),
                   "events": [diagnostic_error_b64(code)],
                   "latestLedger": ledger}),
        );
    }

    /// The `simulateTransaction` answer for an operation whose footprint
    /// holds archived entries: a simulation that succeeded as far as it
    /// could, carrying a `restorePreamble` the caller would have to submit a
    /// `RestoreFootprint` transaction for before the call itself can be
    /// judged. This is what a mainnet pool answers when a reserve or
    /// positions entry has fallen out of the live state.
    fn script_simulate_needs_restore(rpc: &ScriptedRpc, ledger: u32) {
        rpc.expect(
            "simulateTransaction",
            json!({"transactionData": transaction_data_b64(10),
                   "events": [],
                   "minResourceFee": "10",
                   "results": [{"auth": [], "xdr": scval_b64(&stellar_xdr::ScVal::Void)}],
                   "restorePreamble": {"minResourceFee": "7",
                                       "transactionData": transaction_data_b64(7)},
                   "latestLedger": ledger}),
        );
    }

    /// A borrower above the threshold is left alone. The threshold sits
    /// below the contract's own strict test, so "not liquidatable yet" is
    /// the common answer and must be cheap and silent.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_healthy_borrower_is_skipped(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[USER_TWO]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), None);
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(POOL, &[tracked_user(USER_TWO)], tick)
            .await
            .expect("decide");
        assert_eq!(
            decisions,
            vec![(USER_TWO.to_string(), Decision::Skip(SkipReason::Healthy))]
        );
        // Not a vacuous pass: the golden health factor really is above the
        // threshold this config used.
        let golden = GOLDEN_HEALTH
            .iter()
            .find(|(account, _)| *account == USER_TWO)
            .expect("golden entry")
            .1;
        assert!(golden > 9_980_000);
        Ok(())
    }

    /// A borrower under the threshold with collateral and liabilities gets
    /// a plan naming both sides and a percent in 1..=100.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_underwater_borrower_gets_a_plan(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let account = synthetic_account(1);
        // 10 units of reserve 0 as collateral against 5,000 units of
        // reserve 2 as a liability: at the fixture's real, unmodified
        // prices this is wildly underwater (health factor a small fraction
        // of a percent), so the plan is not a borderline case.
        script_snapshot_with_positions(&rpc, &account, &[(0, 100_000_000)], &[(2, 50_000_000_000)]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), None);
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(POOL, &[tracked_user(&account)], tick)
            .await
            .expect("decide");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].0, account);
        let Decision::Liquidate(plan) = &decisions[0].1 else {
            panic!("expected a plan, got {:?}", decisions[0].1);
        };
        // A single collateral position against a single liability position
        // leaves the selection nothing to add on either side, so this
        // lands on `plan_liquidation`'s exhaustion fallback deterministically:
        // percent 100, still within the 1..=100 the brief asks for.
        assert_eq!(plan.percent.get(), 100);
        let fixture = mainnet_fixed_v2();
        let asset_of = |index: usize| {
            fixture["reserves"][index]["asset"]
                .as_str()
                .expect("asset")
                .to_string()
        };
        assert_eq!(
            plan.lot,
            vec![asset_of(0)],
            "the lot is the collateral paid out"
        );
        assert_eq!(
            plan.bid,
            vec![asset_of(2)],
            "the bid is the liability taken over"
        );
        Ok(())
    }

    /// Liabilities with no collateral is bad debt, which is a different
    /// submission entirely: `bad_debt(user)` takes no percent and no asset
    /// lists, because the contract sizes it.
    #[sqlx::test(migrations = "./migrations")]
    async fn liabilities_without_collateral_are_bad_debt(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let account = synthetic_account(2);
        script_snapshot_with_positions(&rpc, &account, &[], &[(1, 10_000_000_000)]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), None);
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(POOL, &[tracked_user(&account)], tick)
            .await
            .expect("decide");
        assert_eq!(decisions, vec![(account, Decision::BadDebt)]);
        Ok(())
    }

    /// A borrower with an auction already open is skipped: the contract
    /// would answer `AuctionInProgress`, and a simulation spent finding
    /// that out is a round trip for nothing.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_user_with_an_open_auction_is_skipped(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        store
            .upsert_auction(&TrackedAuction {
                pool: POOL.to_string(),
                account: USER_ONE.to_string(),
                auction_type: AuctionType::UserLiquidation,
                start_ledger: 1,
                fill_ledger: None,
                percent: None,
                bid: BTreeMap::from([(USDC.to_string(), 1_000)]),
                lot: BTreeMap::from([(USDC.to_string(), 2_000)]),
                updated_ledger: 1,
            })
            .await
            .expect("seed an open auction");
        // The auction is judged from the store, before the position is
        // ever read from chain, but the snapshot is still one per batch
        // regardless of who is skipped, so it is still scripted here.
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), None);
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(POOL, &[tracked_user(USER_ONE)], tick)
            .await
            .expect("decide");
        assert_eq!(
            decisions,
            vec![(
                USER_ONE.to_string(),
                Decision::Skip(SkipReason::AuctionOpen)
            )]
        );
        Ok(())
    }

    /// The bot never liquidates itself. The contract refuses it, but in
    /// dry-run there is no contract to refuse, and a bot that would have
    /// tried is a bot that will try when armed.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_bots_own_accounts_are_never_liquidated(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let own_addresses = BTreeSet::from([USER_ONE.to_string()]);
        let auctioneer = Auctioneer::new(&client, &store, config(own_addresses), None);
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(POOL, &[tracked_user(USER_ONE)], tick)
            .await
            .expect("decide");
        assert_eq!(
            decisions,
            vec![(USER_ONE.to_string(), Decision::Skip(SkipReason::OwnAccount))],
            "USER_ONE is healthy anyway, so a wrong check here would still \
             pass by accident unless it runs before the health factor is read"
        );
        Ok(())
    }

    /// One snapshot serves every user in the batch: the decision for
    /// twenty borrowers is one `getLedgerEntries`, not twenty.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_batch_of_users_takes_one_snapshot(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let extra: Vec<String> = (0..18u8).map(synthetic_account).collect();
        let mut accounts: Vec<&str> = vec![USER_ONE, USER_TWO];
        accounts.extend(extra.iter().map(String::as_str));
        assert_eq!(accounts.len(), 20, "twenty users, not two");
        harness::script_snapshot(&rpc, &accounts);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), None);
        let tick = harness::fixture_tick();
        let users: Vec<TrackedUser> = accounts
            .iter()
            .map(|account| tracked_user(account))
            .collect();

        let decisions = auctioneer.decide(POOL, &users, tick).await.expect("decide");
        assert_eq!(decisions.len(), 20);
        assert_eq!(
            decisions[0],
            (USER_ONE.to_string(), Decision::Skip(SkipReason::Healthy))
        );
        assert_eq!(
            decisions[1],
            (USER_TWO.to_string(), Decision::Skip(SkipReason::Healthy))
        );
        for (account, decision) in &decisions[2..] {
            assert_eq!(
                *decision,
                Decision::Skip(SkipReason::NoLiabilities),
                "{account} holds no positions entry in the fixture ledger"
            );
        }

        // The assertion that actually pins it: a snapshot per user would
        // be twenty `getLedgerEntries` calls (or more), not two.
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            2,
            "one snapshot (a shape read and a batched full read) for all \
             twenty users, not one per user"
        );
        assert_eq!(rpc.remaining(), 0, "every scripted answer was consumed");
        Ok(())
    }

    /// A batch is not held hostage by one poisoned row: a positions entry
    /// naming a reserve index the snapshot does not have is a math failure
    /// for that one borrower, not a reason to withhold every other
    /// borrower's decision. The store, not the chain or the math, is the
    /// one failure `decide` cannot recover from — it is what the bot's
    /// whole picture of who to check and what is already open is built
    /// from.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_poisoned_user_does_not_stall_the_rest_of_the_batch(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let poisoned = synthetic_account(90);
        let healthy = synthetic_account(91);
        // Reserve index 99 does not exist in the fixture's three-reserve
        // pool: `position_data` fails with `MathError::MissingReserve` for
        // this one account, before any health factor is ever computed for
        // it.
        script_snapshot_with_many_positions(
            &rpc,
            &[
                (
                    poisoned.as_str(),
                    &[] as &[(u32, i128)],
                    &[(99, 10_000_000_000)],
                ),
                (
                    healthy.as_str(),
                    &[] as &[(u32, i128)],
                    &[] as &[(u32, i128)],
                ),
            ],
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), None);
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(
                POOL,
                &[tracked_user(&poisoned), tracked_user(&healthy)],
                tick,
            )
            .await
            .expect("decide must not fail for the whole batch");
        assert_eq!(
            decisions,
            vec![(healthy.clone(), Decision::Skip(SkipReason::NoLiabilities))],
            "the poisoned account produced no decision at all; the healthy \
             account still got one"
        );
        Ok(())
    }

    /// The contract's own answer is authoritative: a percent it calls too
    /// small is raised a point and simulated again, and the accepted
    /// percent is the one recorded.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_percent_the_contract_calls_too_small_is_raised(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(1);
        let bid_asset = synthetic_account(2);
        let lot_asset = synthetic_account(3);

        // Three attempts: too small, too small again, then accepted.
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_refused(&rpc, 1214, 100);
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_refused(&rpc, 1214, 100);
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let plan = LiquidationPlan {
            bid: vec![bid_asset],
            lot: vec![lot_asset],
            percent: FillPercent::try_from(50).expect("50 is in range"),
        };
        let decision = Decision::Liquidate(plan);
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &decision, tick, None)
            .await
            .expect("act")
            .expect("a creation");
        assert_eq!(
            outcome.percent,
            Some(FillPercent::try_from(52).expect("52 is in range")),
            "the planned percent plus two"
        );
        assert!(
            outcome.simulated,
            "an auctioneer key was configured, so this was simulated"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            3,
            "exactly three simulations were attempted"
        );
        Ok(())
    }

    /// And one it calls too large is lowered.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_percent_the_contract_calls_too_large_is_lowered(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(4);
        let bid_asset = synthetic_account(5);
        let lot_asset = synthetic_account(6);

        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_refused(&rpc, 1213, 100);
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let plan = LiquidationPlan {
            bid: vec![bid_asset],
            lot: vec![lot_asset],
            percent: FillPercent::try_from(60).expect("60 is in range"),
        };
        let decision = Decision::Liquidate(plan);
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &decision, tick, None)
            .await
            .expect("act")
            .expect("a creation");
        assert_eq!(
            outcome.percent,
            Some(FillPercent::try_from(59).expect("59 is in range")),
            "the planned percent minus one"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            2,
            "exactly two simulations were attempted"
        );
        Ok(())
    }

    /// The adjustment is bounded. A contract that answers
    /// `InvalidLiqTooSmall` forever must cost this borrower five
    /// simulations and no more, or one borrower starves the cadence.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_percent_adjustment_gives_up_after_plan_iterations(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(7);
        let bid_asset = synthetic_account(8);
        let lot_asset = synthetic_account(9);

        // `config`'s `plan_iterations` is 5; every one of the five attempts
        // is refused the same way.
        for _ in 0..5 {
            script_simulate_prelude(&rpc, &signer, 10, 100);
            script_simulate_refused(&rpc, 1214, 100);
        }

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let plan = LiquidationPlan {
            bid: vec![bid_asset],
            lot: vec![lot_asset],
            percent: FillPercent::try_from(50).expect("50 is in range"),
        };
        let decision = Decision::Liquidate(plan);
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &decision, tick, None)
            .await
            .expect("act");
        assert!(
            outcome.is_none(),
            "no percent was ever accepted, so nothing was recorded"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            5,
            "five simulations, no more: plan_iterations bounds the retry"
        );
        Ok(())
    }

    /// Any other contract error skips the borrower immediately, with the
    /// code on the log line: adjusting a percent against
    /// `AuctionInProgress` would be four more round trips to learn what the
    /// first one said.
    #[sqlx::test(migrations = "./migrations")]
    async fn another_contract_error_skips_without_retrying(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(11);
        let bid_asset = synthetic_account(12);
        let lot_asset = synthetic_account(13);

        // AuctionInProgress (1212) is neither 1213 nor 1214.
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_refused(&rpc, 1212, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let plan = LiquidationPlan {
            bid: vec![bid_asset],
            lot: vec![lot_asset],
            percent: FillPercent::try_from(50).expect("50 is in range"),
        };
        let decision = Decision::Liquidate(plan);
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &decision, tick, None)
            .await
            .expect("act");
        assert!(
            outcome.is_none(),
            "an unrelated contract error skips the borrower"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            1,
            "one simulation, not a retry: 1212 is not the percent loop's business"
        );
        Ok(())
    }

    /// Dry-run simulates and records, and sends nothing. This is the
    /// headline safety invariant of the whole repository, so the test
    /// asserts on the scripted server: no `sendTransaction` was ever
    /// called, not merely that a row was recorded with `dry_run = true`.
    #[sqlx::test(migrations = "./migrations")]
    async fn dry_run_records_the_creation_and_sends_nothing(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(20);
        let bid_asset = synthetic_account(21);
        let lot_asset = synthetic_account(22);

        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let plan = LiquidationPlan {
            bid: vec![bid_asset],
            lot: vec![lot_asset],
            percent: FillPercent::try_from(75).expect("75 is in range"),
        };
        let decision = Decision::Liquidate(plan);
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &decision, tick, None)
            .await
            .expect("act")
            .expect("a creation");
        assert!(!outcome.submitted(), "dry-run never submits");
        assert!(outcome.tx_hash().is_none(), "dry-run never has a hash");
        assert!(
            outcome.submission.is_none(),
            "dry-run never has a chain outcome to carry back"
        );
        assert!(
            outcome.simulated,
            "an auctioneer key was configured, so the percent was checked"
        );

        let row = sqlx::query!(
            "SELECT dry_run, tx_hash FROM creations WHERE account = $1",
            account.as_str(),
        )
        .fetch_one(store.pool())
        .await
        .expect("the creation was recorded");
        assert!(row.dry_run, "dry-run must be recorded as dry_run = true");
        assert!(row.tx_hash.is_none(), "dry-run never records a hash");

        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "dry-run must never call sendTransaction, not merely skip \
             recording one"
        );
        Ok(())
    }

    /// Armed, the creation goes through the queue and the recorded row
    /// carries the transaction it became.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_armed_creation_is_queued_and_recorded_with_its_hash(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(30);
        let bid_asset = synthetic_account(31);
        let lot_asset = synthetic_account(32);

        // Round 1: `act`'s own percent-discovery simulation.
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        // Round 2: the queue's own `Submitter::submit` prepares from fresh
        // state before it ever sends — a submission queue never reuses a
        // simulation another caller already ran, since the sequence it
        // reads must be the one still current at send time. This is the
        // signing path, so it reads fee stats too, which the simulate-only
        // round above never does.
        script_prepare_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        rpc.expect(
            "sendTransaction",
            json!({"status": "PENDING", "hash": "$ENVELOPE_HASH",
                   "latestLedger": 100, "latestLedgerCloseTime": "1"}),
        );
        rpc.expect(
            "getTransaction",
            json!({"status": "SUCCESS", "latestLedger": 101, "oldestLedger": 1,
                   "ledger": 100, "createdAt": "1", "txHash": "ab".repeat(32),
                   "envelopeXdr": "AAAA",
                   "resultXdr": result_b64(TransactionResultResult::TxSuccess(VecM::default())),
                   "resultMetaXdr": meta_v4_b64(None, vec![]),
                   "diagnosticEventsXdr": []}),
        );

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let (queue, receiver) = SubmissionQueue::new(4);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let plan = LiquidationPlan {
            bid: vec![bid_asset],
            lot: vec![lot_asset],
            percent: FillPercent::try_from(80).expect("80 is in range"),
        };
        let decision = Decision::Liquidate(plan);
        let tick = harness::fixture_tick();

        // `run_queue` only returns once its sender is dropped, so `queue`
        // is dropped inside this future right after `act` is done with it —
        // the same shape `queue`'s own shutdown test uses to join the two
        // without `tokio::spawn`'s `'static` bound, which a `Submitter`
        // borrowing this test's local `client`/`network`/`signer` could
        // not satisfy.
        let act_and_drop = async {
            let outcome = auctioneer
                .act(POOL, &account, &decision, tick, Some(&queue))
                .await;
            drop(queue);
            outcome
        };
        let (outcome, ()) =
            tokio::join!(act_and_drop, run_queue(&submitter, receiver, &shutdown_rx));
        let outcome = outcome.expect("act").expect("a creation");

        assert!(outcome.submitted(), "armed, the creation is submitted");
        assert!(outcome.succeeded(), "this one landed");
        let hash = outcome.tx_hash().expect("a hash").to_hex();
        assert_eq!(hash.len(), 64, "a tx hash renders as 64 hex digits");

        let row = sqlx::query!(
            "SELECT dry_run, tx_hash FROM creations WHERE account = $1",
            account.as_str(),
        )
        .fetch_one(store.pool())
        .await
        .expect("the creation was recorded");
        assert!(
            !row.dry_run,
            "an armed creation is recorded as dry_run = false"
        );
        assert_eq!(
            row.tx_hash,
            Some(hash),
            "the recorded row carries the transaction it became"
        );
        Ok(())
    }

    /// Bad debt is a different submission: no percent, no asset lists, and
    /// it is never routed through the percent-adjustment loop.
    #[sqlx::test(migrations = "./migrations")]
    async fn bad_debt_submits_without_a_percent(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(40);

        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let decision = Decision::BadDebt;
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &decision, tick, None)
            .await
            .expect("act")
            .expect("a creation");
        assert_eq!(outcome.kind, CreationKind::BadDebt);
        assert_eq!(outcome.percent, None, "bad debt carries no percent");
        assert!(outcome.simulated);
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            1,
            "exactly one simulation: bad debt is never routed through the \
             percent loop"
        );

        let row = sqlx::query!(
            "SELECT kind, percent, bid, lot FROM creations WHERE account = $1",
            account.as_str(),
        )
        .fetch_one(store.pool())
        .await
        .expect("the creation was recorded");
        assert_eq!(row.kind, "bad_debt");
        assert!(row.percent.is_none(), "no percent column value");
        assert_eq!(row.bid, json!([]), "bad debt carries no bid list");
        assert_eq!(row.lot, json!([]), "bad debt carries no lot list");
        Ok(())
    }

    /// `plan_liquidation` chooses a percent for a selection, and
    /// `bounded_plan` then trims that selection to the pool's
    /// `max_positions` without recomputing the percent — an accepted
    /// approximation precisely because this percent-adjustment loop
    /// corrects it against the contract's own answer.
    ///
    /// So the plan here is not hand-built: it comes out of
    /// `plan_liquidation` itself, against a position whose excess only two
    /// liabilities can close, under a cap of two positions in total. The
    /// selection therefore grows to two liabilities, picks its percent for
    /// that pair, and is then trimmed back to one asset per side —
    /// `bounded_plan`'s floor — carrying a percent chosen for a selection
    /// twice its size. That is the stale percent this loop exists to
    /// correct, and the contract rejects it once before accepting.
    ///
    /// The arithmetic is `math::liquidation`'s own worked example, split
    /// across two liabilities: cf 0.75, lf 1.25, incentive 1.2, recovered
    /// 0.425 and excess 310 give 91 for the pair.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_trimmed_plans_stale_percent_still_converges(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(60);

        let data = PositionData {
            collateral_base: 7_500_000_000,
            collateral_raw: 10_000_000_000,
            liability_base: 10_000_000_000,
            liability_raw: 8_000_000_000,
            scalar: SCALAR_7,
        };
        let collateral = vec![PositionValue {
            index: 0,
            asset: synthetic_account(61),
            raw: 10_000_000_000,
            effective: 7_500_000_000,
        }];
        let liabilities = vec![
            PositionValue {
                index: 1,
                asset: synthetic_account(62),
                raw: 4_000_000_000,
                effective: 5_000_000_000,
            },
            PositionValue {
                index: 2,
                asset: synthetic_account(63),
                raw: 4_000_000_000,
                effective: 5_000_000_000,
            },
        ];
        // A cap of two: one liability cannot close the excess, so the
        // selection grows to both — three positions in all — and the trim
        // has to cut it back.
        let plan = plan_liquidation(&data, &collateral, &liabilities, 10_600_000, 2)
            .expect("plan")
            .expect("an underwater position has a plan");
        assert_eq!(
            (plan.bid.len(), plan.lot.len()),
            (1, 1),
            "the cap really did trim the selection the percent was chosen \
             for; without the trim the bid would name both liabilities"
        );
        assert_eq!(
            plan.percent.get(),
            91,
            "the percent is the one chosen for the untrimmed pair"
        );

        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_refused(&rpc, INVALID_LIQ_TOO_SMALL, 100);
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let decision = Decision::Liquidate(plan);
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &decision, tick, None)
            .await
            .expect("act")
            .expect("the loop converges on an accepted percent");
        assert_eq!(
            outcome.percent,
            Some(FillPercent::try_from(92).expect("92 is in range")),
            "the trimmed selection's stale percent is corrected upward by \
             the loop, not recomputed from the assets"
        );
        Ok(())
    }

    /// The headline safety invariant, in the case that used to breach it: a
    /// simulation that comes back needing archived entries restored.
    ///
    /// `Submitter::prepare` answers that by *submitting* a
    /// `RestoreFootprint` transaction from the auctioneer's own key — a
    /// real fee, a consumed sequence number — before simulating again, so a
    /// dry-run that simulated through it would have written to mainnet.
    /// `Submitter::simulate_only` never does: the borrower is skipped, and
    /// this test proves the absence three ways over. `ScriptedRpc` records
    /// every request before it looks for a scripted answer, so an empty
    /// call list is proof, not merely an unconsumed script.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_dry_run_never_restores_an_archived_footprint(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(70);
        let bid_asset = synthetic_account(71);
        let lot_asset = synthetic_account(72);

        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_needs_restore(&rpc, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let plan = LiquidationPlan {
            bid: vec![bid_asset],
            lot: vec![lot_asset],
            percent: FillPercent::try_from(50).expect("50 is in range"),
        };
        let decision = Decision::Liquidate(plan);
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &decision, tick, None)
            .await
            .expect("act");
        assert!(
            outcome.is_none(),
            "a borrower that cannot be judged is skipped, not recorded"
        );

        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "a dry-run must never send anything — a RestoreFootprint \
             transaction least of all"
        );
        assert!(
            rpc.calls("getFeeStats").is_empty(),
            "the signing path was never entered at all: `prepare` reads fee \
             stats before it simulates, and `simulate_only` never does"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            1,
            "one simulation and no retry: a restore is not an opinion about \
             the percent, so the walk neither adjusts it nor tries again"
        );

        let rows = sqlx::query!(
            "SELECT count(*) AS \"count!\" FROM creations WHERE account = $1",
            account.as_str(),
        )
        .fetch_one(store.pool())
        .await
        .expect("count the rows");
        assert_eq!(rows.count, 0, "nothing was decided, so nothing is recorded");
        Ok(())
    }

    /// Bad debt takes the same simulate-only path, and skips the same way.
    #[sqlx::test(migrations = "./migrations")]
    async fn bad_debt_needing_a_restore_is_skipped_too(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(73);

        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_needs_restore(&rpc, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &Decision::BadDebt, tick, None)
            .await
            .expect("act");
        assert!(outcome.is_none(), "skipped, not recorded");
        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "no RestoreFootprint transaction was sent"
        );
        assert!(rpc.calls("getFeeStats").is_empty(), "nothing was prepared");
        Ok(())
    }

    /// With no auctioneer key there is no source account to simulate
    /// against, so both `act` paths record the plan the contract was never
    /// asked about and say so. Nothing is signed because nothing is even
    /// asked: the chain is not touched at all.
    #[sqlx::test(migrations = "./migrations")]
    async fn with_no_key_the_creation_is_recorded_unsimulated(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        // Deliberately scripted with nothing: any request at all would come
        // back a 500 and fail the test.
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), None);
        let account = synthetic_account(80);
        let bad_debt_account = synthetic_account(81);
        let plan = LiquidationPlan {
            bid: vec![synthetic_account(82)],
            lot: vec![synthetic_account(83)],
            percent: FillPercent::try_from(64).expect("64 is in range"),
        };
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &Decision::Liquidate(plan), tick, None)
            .await
            .expect("act")
            .expect("a creation");
        assert!(
            !outcome.simulated,
            "no key, no source account, no simulation"
        );
        assert_eq!(
            outcome.percent,
            Some(FillPercent::try_from(64).expect("64 is in range")),
            "the plan's own percent stands: there was no contract answer to \
             adjust it against"
        );
        assert!(!outcome.submitted(), "and nothing was submitted");

        let bad_debt = auctioneer
            .act(POOL, &bad_debt_account, &Decision::BadDebt, tick, None)
            .await
            .expect("act")
            .expect("a creation");
        assert!(
            !bad_debt.simulated,
            "the bad-debt path answers the same way"
        );
        assert_eq!(bad_debt.kind, CreationKind::BadDebt);

        let row = sqlx::query!(
            "SELECT dry_run, tx_hash FROM creations WHERE account = $1",
            account.as_str(),
        )
        .fetch_one(store.pool())
        .await
        .expect("the creation was recorded");
        assert!(row.dry_run, "recorded as a dry run");
        assert!(row.tx_hash.is_none(), "with no transaction behind it");

        assert!(
            rpc.calls("simulateTransaction").is_empty(),
            "the contract was never asked"
        );
        assert!(
            rpc.calls("getLedgerEntries").is_empty(),
            "not even the source account was read"
        );
        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "and nothing was sent"
        );
        Ok(())
    }

    /// A submission that lands and fails is not a submission that worked.
    /// `act` carries every terminal `TxOutcome` back to the caller, and the
    /// row still gets the hash: a failed transaction consumed a sequence
    /// number and charged a fee, which is exactly what an audit is for.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_failed_submission_comes_back_as_failed(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(90);

        // `act`'s own simulate-only round, then the queue's prepare-and-send.
        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_prepare_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        rpc.expect(
            "sendTransaction",
            json!({"status": "PENDING", "hash": "$ENVELOPE_HASH",
                   "latestLedger": 100, "latestLedgerCloseTime": "1"}),
        );
        let failed = TransactionResultResult::TxFailed(
            VecM::try_from(vec![OperationResult::OpInner(
                OperationResultTr::InvokeHostFunction(InvokeHostFunctionResult::Trapped),
            )])
            .expect("one operation result"),
        );
        rpc.expect(
            "getTransaction",
            json!({"status": "FAILED", "latestLedger": 101, "oldestLedger": 1,
                   "ledger": 100, "createdAt": "1", "txHash": "ab".repeat(32),
                   "envelopeXdr": "AAAA",
                   "resultXdr": result_b64(failed),
                   "resultMetaXdr": meta_v4_b64(None, vec![]),
                   "diagnosticEventsXdr": [diagnostic_error_b64(1_205)]}),
        );

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let (queue, receiver) = SubmissionQueue::new(4);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tick = harness::fixture_tick();

        let act_and_drop = async {
            let outcome = auctioneer
                .act(POOL, &account, &Decision::BadDebt, tick, Some(&queue))
                .await;
            drop(queue);
            outcome
        };
        let (outcome, ()) =
            tokio::join!(act_and_drop, run_queue(&submitter, receiver, &shutdown_rx));
        let outcome = outcome.expect("act").expect("a creation");

        assert!(outcome.submitted(), "it did go through the queue");
        assert!(
            !outcome.succeeded(),
            "but it failed, and `succeeded` must not read `Some` as success"
        );
        let Some(TxOutcome::Failed { contract_error, .. }) = &outcome.submission else {
            panic!("expected a Failed outcome, got {:?}", outcome.submission);
        };
        assert_eq!(
            *contract_error,
            Some(1_205),
            "the pool's own error code survives the trip back to the caller"
        );

        let row = sqlx::query!(
            "SELECT dry_run, tx_hash FROM creations WHERE account = $1",
            account.as_str(),
        )
        .fetch_one(store.pool())
        .await
        .expect("the creation was recorded");
        assert!(!row.dry_run);
        assert_eq!(
            row.tx_hash,
            outcome.tx_hash().map(|hash| hash.to_hex()),
            "a failed transaction is still the transaction this creation \
             became, and the row says so"
        );
        Ok(())
    }

    /// The creation row exists before the submission does, so a submission
    /// the queue refuses leaves the same audit trail a dry-run would —
    /// never less. The queue here is shut down before `act` reaches it, so
    /// the enqueue is answered with a refusal rather than a transaction.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_refused_submission_still_leaves_its_row(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(95);

        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_accepted(&rpc, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let (queue, receiver) = SubmissionQueue::new(4);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(true);
        let tick = harness::fixture_tick();

        let act_and_drop = async {
            let outcome = auctioneer
                .act(POOL, &account, &Decision::BadDebt, tick, Some(&queue))
                .await;
            drop(queue);
            outcome
        };
        let (outcome, ()) =
            tokio::join!(act_and_drop, run_queue(&submitter, receiver, &shutdown_rx));
        drop(shutdown_tx);
        assert!(
            matches!(outcome, Err(AuctioneerError::Queue(_))),
            "the refusal reaches this borrower's caller"
        );

        let row = sqlx::query!(
            "SELECT dry_run, tx_hash FROM creations WHERE account = $1",
            account.as_str(),
        )
        .fetch_one(store.pool())
        .await
        .expect("the row was written before the submission was attempted");
        assert!(
            !row.dry_run,
            "it was an armed attempt, and the audit says so"
        );
        assert!(
            row.tx_hash.is_none(),
            "an armed attempt whose transaction was never named — here \
             because the queue refused it before anything was submitted"
        );
        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "the queue refused it before anything was sent"
        );
        Ok(())
    }

    /// A percent of 100 the contract still calls too small has nowhere
    /// higher to go: the walk ends there rather than wrapping to 101, which
    /// is not a `FillPercent` at all.
    #[sqlx::test(migrations = "./migrations")]
    async fn too_small_at_one_hundred_percent_degrades_to_a_skip(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(100);

        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_refused(&rpc, INVALID_LIQ_TOO_SMALL, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let plan = LiquidationPlan {
            bid: vec![synthetic_account(101)],
            lot: vec![synthetic_account(102)],
            percent: FillPercent::try_from(100).expect("100 is the ceiling"),
        };
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &Decision::Liquidate(plan), tick, None)
            .await
            .expect("act");
        assert!(outcome.is_none(), "nowhere higher to try: skipped");
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            1,
            "and no second attempt at a percent that cannot exist"
        );
        Ok(())
    }

    /// And a percent of 1 the contract calls too large has nowhere lower:
    /// zero is not a `FillPercent` either, and `checked_sub` is what keeps
    /// the arithmetic from wrapping to `u32::MAX` on the way to finding
    /// that out.
    #[sqlx::test(migrations = "./migrations")]
    async fn too_large_at_one_percent_degrades_to_a_skip(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let signer = auctioneer_signer();
        let network = Network::testnet();
        let account = synthetic_account(110);

        script_simulate_prelude(&rpc, &signer, 10, 100);
        script_simulate_refused(&rpc, INVALID_LIQ_TOO_LARGE, 100);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()), Some(submitter));
        let plan = LiquidationPlan {
            bid: vec![synthetic_account(111)],
            lot: vec![synthetic_account(112)],
            percent: FillPercent::try_from(1).expect("1 is the floor"),
        };
        let tick = harness::fixture_tick();

        let outcome = auctioneer
            .act(POOL, &account, &Decision::Liquidate(plan), tick, None)
            .await
            .expect("act");
        assert!(outcome.is_none(), "nowhere lower to try: skipped");
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            1,
            "and no second attempt at a percent of zero"
        );
        Ok(())
    }
}
