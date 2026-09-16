//! A per-key ordered submission queue.
//!
//! A Soroban transaction is built against its source account's sequence
//! number, read when it is prepared. Two tasks preparing for one account
//! concurrently build two transactions carrying the same sequence, and the
//! network applies at most one of them: the other comes back as
//! [`ChainError::BadSequence`], whose only correct recovery is to re-plan
//! from fresh state, never to resend. One queue per signing key makes that
//! race unreachable rather than merely recoverable — Phase 5's filler holds
//! a second queue for the filler's own key, and nothing here may assume
//! there is only one.
//!
//! The queue owns two things: the order submissions go out in, and the rule
//! that nothing is prepared for a key while an earlier transaction's outcome
//! is still unknown — an in-flight transaction may yet consume the sequence
//! number the next `prepare` would read. What to submit, at what priority,
//! and what a failure means stay the caller's.
//!
//! Only a failure that provably sent nothing is retried here, and only
//! within the budget its [`Submission`] carries: a `prepare` that failed
//! before any envelope left, or a send the RPC refused outright. A send
//! whose *answer* was lost proves nothing — the RPC may have forwarded the
//! envelope — so that one is resolved by the hash the queue already holds
//! and never sent again.

use std::num::NonZeroUsize;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::chain::tx::{Prepared, Priority, Submitter, TxOutcome};
use crate::chain::ChainError;
use stellar_xdr::Operation;

/// What a caller asks the queue to send.
///
/// `Debug` is written by hand, and prints the label and the priority only.
/// The derive would print `operation`'s contents — an `InvokeHostFunction`
/// carrying the pool, the borrower and the amounts — which is exactly what
/// this module states it does not log, and a derived impl makes breaking
/// that a one-liner for the next caller that puts a `Submission` on a
/// tracing field. `Signer`, `Secret` and `ChainConfig` are hand-written for
/// the same reason.
pub struct Submission {
    /// The operation to invoke.
    pub operation: Operation,
    /// The fee tier.
    pub priority: Priority,
    /// What this submission is, for the log line. Never a secret: this
    /// module logs it verbatim, and nothing in this module logs
    /// `operation`'s contents.
    pub label: String,
    /// Further attempts after a failure that provably sent nothing; zero
    /// for none. What each role gets is spec §8's: [`CREATION_RETRIES`],
    /// [`FILL_RETRIES`].
    pub retries: u32,
}

/// A submission paired with the channel its answer goes back on.
///
/// `Debug` is hand-written for the reason [`Submission`]'s is, and prints
/// the same two fields.
pub struct QueuedSubmission {
    /// The work.
    pub submission: Submission,
    /// Where the outcome goes. A dropped receiver means the caller gave up;
    /// the send fails and the queue moves on.
    pub respond: oneshot::Sender<Result<TxOutcome, QueueError>>,
}

impl std::fmt::Debug for Submission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Submission")
            .field("label", &self.label)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for QueuedSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueuedSubmission")
            .field("label", &self.submission.label)
            .field("priority", &self.submission.priority)
            .finish_non_exhaustive()
    }
}

/// Why a submission did not produce an outcome.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// The queue's worker is gone, so nothing will ever be sent.
    #[error("the submission queue is closed")]
    Closed,
    /// The bot is shutting down: this submission was dequeued after the
    /// flag was set and was never attempted. Its own variant, rather than
    /// a `ChainError` dressed up as one, because a caller must be able to
    /// tell "the bot is stopping" from "this operation can never be built"
    /// — the first is nothing to act on, the second is a bug.
    #[error("the submission queue is shutting down; this submission was not attempted")]
    ShuttingDown,
    /// The chain layer's own failure, passed through.
    ///
    /// This variant is answered **only for a failure that provably sent
    /// nothing of this submission**: a `prepare` that failed before any
    /// envelope of it left (including [`ChainError::RestoreUnknown`],
    /// where the restore transaction of `prepare`'s own is what is
    /// unresolved — it spends fees, never this submission's operation), a
    /// send the RPC refused outright ([`ChainError::Rejected`]), or a
    /// sequence number another transaction took first
    /// ([`ChainError::BadSequence`]). A transaction that may be in flight
    /// is never reported here: a send whose *answer* was lost is resolved
    /// by the hash this queue already holds, and
    /// [`Submitter::wait_for`] answers [`TxOutcome::Unknown`] rather than
    /// an error while an outcome could still become terminal.
    ///
    /// A caller may therefore treat this as "nothing was spent" — the
    /// executor releases its wallet reservation on it — and that stays
    /// true only while this contract does.
    #[error(transparent)]
    Chain(#[from] ChainError),
}

/// Retries an auction creation gets after a failure that sent nothing
/// (spec §8).
pub const CREATION_RETRIES: u32 = 3;

/// Retries a fill gets (spec §8): more than a creation, because a fill that
/// lapses is money another bot takes.
pub const FILL_RETRIES: u32 = 10;

/// How the queue paces what a submission's budget allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The pause before the first retry; each later one doubles it.
    pub initial: Duration,
    /// The longest pause.
    pub max: Duration,
    /// The pause between two polls of an outcome still unknown.
    pub resolve_pause: Duration,
}

impl RetryPolicy {
    /// Spec §8's backoff: one second, doubling, to thirty.
    pub const DEFAULT: Self = Self {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(30),
        resolve_pause: Duration::from_secs(1),
    };
}

/// The caller's handle. Cloneable: several tasks may enqueue for one key,
/// which is the point.
#[derive(Debug, Clone)]
pub struct SubmissionQueue {
    sender: mpsc::Sender<QueuedSubmission>,
}

impl SubmissionQueue {
    /// A queue and the receiver its worker consumes. `capacity` bounds how
    /// many submissions may wait; a full queue makes `enqueue` wait, which
    /// is the backpressure that keeps a stalled chain from growing an
    /// unbounded backlog of stale plans. It is `NonZeroUsize` because
    /// tokio's bounded channel panics on a capacity of zero, and a type
    /// that cannot hold zero is cheaper than a runtime assertion nobody
    /// remembers.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> (Self, mpsc::Receiver<QueuedSubmission>) {
        let (sender, receiver) = mpsc::channel(capacity.get());
        (Self { sender }, receiver)
    }

    /// Enqueues `submission` and waits for its outcome.
    pub async fn enqueue(&self, submission: Submission) -> Result<TxOutcome, QueueError> {
        let (respond, answer) = oneshot::channel();
        self.sender
            .send(QueuedSubmission {
                submission,
                respond,
            })
            .await
            .map_err(|_| QueueError::Closed)?;
        answer.await.map_err(|_| QueueError::Closed)?
    }
}

/// [`run_queue_with`] under [`RetryPolicy::DEFAULT`].
pub async fn run_queue(
    submitter: &Submitter<'_>,
    receiver: mpsc::Receiver<QueuedSubmission>,
    shutdown: &watch::Receiver<bool>,
) {
    run_queue_with(submitter, receiver, shutdown, RetryPolicy::DEFAULT).await;
}

/// Drains `receiver` until every sender is dropped, submitting one at a
/// time through `submitter`. Once `shutdown` is set, no further submission
/// is attempted: each is answered with an error instead of being sent, so a
/// caller blocked on `enqueue` — the auctioneer awaiting a submission that
/// will never be made — is released rather than left waiting on a tick that
/// will never come.
///
/// Every submission is answered only once its outcome is terminal
/// ([`TxOutcome::Succeeded`], [`TxOutcome::Failed`], [`TxOutcome::Expired`]),
/// or with [`TxOutcome::Unknown`] once shutdown interrupts the resolution —
/// never while the next submission for this key could be prepared against a
/// sequence number it may still consume.
///
/// A submission's failure is returned to its caller and never ends the
/// queue: one borrower's contract error is not the bot's. The shutdown flag
/// is checked between submissions, never during one — a transaction already
/// sent is waited for, because abandoning it would leave the account's
/// sequence consumed by something the bot never saw the outcome of — and
/// while a retry's backoff is waiting, which it cuts short.
///
/// A submission dequeued after shutdown is answered
/// [`QueueError::ShuttingDown`], which no chain outcome can be mistaken
/// for: nothing was sent, so no `ChainError` — every one of which describes
/// something the chain or the client did — would be true of it.
pub async fn run_queue_with(
    submitter: &Submitter<'_>,
    mut receiver: mpsc::Receiver<QueuedSubmission>,
    shutdown: &watch::Receiver<bool>,
    policy: RetryPolicy,
) {
    while let Some(queued) = receiver.recv().await {
        if *shutdown.borrow() {
            tracing::debug!(
                label = queued.submission.label,
                "queue is shutting down; refusing this submission"
            );
            let _ = queued.respond.send(Err(QueueError::ShuttingDown));
            continue;
        }
        let QueuedSubmission {
            submission,
            respond,
        } = queued;
        tracing::info!(label = submission.label, "submitting");
        let outcome = submit_until_settled(submitter, &submission, shutdown, policy).await;
        if let Err(error) = &outcome {
            tracing::warn!(label = submission.label, %error, "submission failed");
        }
        // A caller that gave up is not an error: the answer simply has
        // nowhere to go.
        let _ = respond.send(outcome);
    }
}

/// One submission, to an answer the caller can act on. Retries only what
/// provably never reached the network, pausing between attempts.
async fn submit_until_settled(
    submitter: &Submitter<'_>,
    submission: &Submission,
    shutdown: &watch::Receiver<bool>,
    policy: RetryPolicy,
) -> Result<TxOutcome, QueueError> {
    let mut retries_left = submission.retries;
    // `max` is the longest pause, the first one included: a policy whose
    // `initial` is longer than its `max` is a misconfiguration, not a
    // licence to wait past the ceiling.
    let mut pause = policy.initial.min(policy.max);
    loop {
        match attempt(submitter, submission, shutdown, policy).await {
            Err(error) if sent_nothing(&error) && retries_left > 0 => {
                retries_left -= 1;
                tracing::warn!(
                    label = submission.label,
                    %error,
                    retries_left,
                    "nothing was sent; preparing again from fresh state"
                );
                if !pause_unless_shutdown(pause, shutdown).await {
                    return Err(QueueError::Chain(error));
                }
                pause = pause.saturating_mul(2).min(policy.max);
            }
            outcome => return outcome.map_err(QueueError::Chain),
        }
    }
}

/// Prepare, send, and resolve. A send whose answer is lost is resolved by
/// hash like any other — the RPC may have forwarded it.
async fn attempt(
    submitter: &Submitter<'_>,
    submission: &Submission,
    shutdown: &watch::Receiver<bool>,
    policy: RetryPolicy,
) -> Result<TxOutcome, ChainError> {
    let prepared = submitter
        .prepare(submission.operation.clone(), submission.priority)
        .await?;
    tracing::info!(
        label = submission.label,
        hash = %prepared.hash,
        sequence = prepared.sequence,
        max_ledger = prepared.window.max_ledger(),
        fee = prepared.fee,
        "sending transaction"
    );
    match submitter.send(&prepared).await {
        Ok(()) => {}
        Err(error @ (ChainError::BadSequence | ChainError::Rejected(_))) => return Err(error),
        Err(error) => tracing::warn!(
            label = submission.label,
            hash = %prepared.hash,
            %error,
            "the send's answer was lost; resolving by hash rather than resending"
        ),
    }
    resolve(submitter, &prepared, shutdown, policy).await
}

/// `wait_for` until the outcome is terminal. `Unknown` comes back only when
/// shutdown has been requested.
async fn resolve(
    submitter: &Submitter<'_>,
    prepared: &Prepared,
    shutdown: &watch::Receiver<bool>,
    policy: RetryPolicy,
) -> Result<TxOutcome, ChainError> {
    loop {
        let outcome = submitter
            .wait_for(prepared.hash, prepared.sequence, prepared.window)
            .await?;
        if !matches!(outcome, TxOutcome::Unknown { .. }) || *shutdown.borrow() {
            return Ok(outcome);
        }
        tracing::warn!(
            hash = %prepared.hash,
            "the outcome is still unknown; nothing else is sent for this key until it is known"
        );
        // `pause_unless_shutdown` answers the flag itself, so an unknown
        // outcome goes back only for a shutdown that was actually
        // requested. Anything looser — a dropped sender read as one —
        // would free this key while the transaction is still in flight,
        // and the next submission would be prepared against a sequence
        // number it may yet consume.
        if !pause_unless_shutdown(policy.resolve_pause, shutdown).await {
            return Ok(outcome);
        }
    }
}

/// Whether `error` proves nothing of this submission reached the network: a
/// `prepare` that failed before sending, or a send the RPC refused.
/// Everything a send can fail with *after* the envelope left is handled in
/// `attempt`, by hash, and never reaches this.
fn sent_nothing(error: &ChainError) -> bool {
    matches!(
        error,
        ChainError::Rejected(_)
            | ChainError::Transport(_)
            | ChainError::Http(_)
            | ChainError::Rpc { .. }
            | ChainError::LedgerMoved { .. }
    )
}

/// Sleeps `pause`, cut short by a shutdown request. `false` when the bot is
/// shutting down — the answer is the flag itself, read once the pause has
/// ended, whichever arm ended it.
///
/// A dropped `watch::Sender` is **not** a shutdown: `wait_for` answers
/// `RecvError` for it, and taking that for a request would end every pause
/// the instant the sender went away — reporting "stopping" while the flag
/// still reads `false`, and, for a caller pausing between polls, spinning
/// instead of pausing at all. The `Ok(_)` pattern disables that arm rather
/// than matching it, so the sleep runs its course and the flag decides.
async fn pause_unless_shutdown(pause: Duration, shutdown: &watch::Receiver<bool>) -> bool {
    let mut requested = shutdown.clone();
    tokio::select! {
        () = tokio::time::sleep(pause) => {}
        Ok(_) = requested.wait_for(|stopping| *stopping) => {}
    }
    !*shutdown.borrow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::{
        script_prepare_prelude, script_send, script_simulate_accepted, script_simulate_refused,
        script_transaction_not_found, script_transaction_success, ScriptedRpc,
    };
    use crate::chain::signer::{Network, Signer};
    use crate::chain::tx::TxConfig;
    use crate::chain::TxHash;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use stellar_xdr::TransactionResultResult;

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";

    fn capacity(slots: usize) -> NonZeroUsize {
        NonZeroUsize::new(slots).expect("a test capacity is never zero")
    }

    fn operation() -> Operation {
        crate::chain::xdr::encode::invoke_contract_op(POOL, "bad_debt", vec![])
            .expect("a fixed operation encodes")
    }

    fn submission(label: &str) -> Submission {
        Submission {
            operation: operation(),
            priority: Priority::Normal,
            label: label.to_string(),
            retries: 0,
        }
    }

    fn fake_outcome() -> TxOutcome {
        TxOutcome::Succeeded {
            hash: TxHash([0_u8; 32]),
            ledger: 1,
            return_value: None,
        }
    }

    /// A hash derived from `label`'s own bytes, so an outcome can be traced
    /// back to the submission it answers without any shared counter.
    fn hash_for(label: &str) -> TxHash {
        let mut bytes = [0_u8; 32];
        let text = label.as_bytes();
        let len = text.len().min(bytes.len());
        bytes[..len].copy_from_slice(&text[..len]);
        TxHash(bytes)
    }

    fn outcome_for(label: &str) -> TxOutcome {
        TxOutcome::Succeeded {
            hash: hash_for(label),
            ledger: 1,
            return_value: None,
        }
    }

    /// Submissions are prepared and sent one at a time, in the order they
    /// were enqueued. Two at once would build two transactions against one
    /// sequence number, and the chain would reject the second.
    ///
    /// Recording only the label on arrival — as a naive version of this
    /// test would — proves nothing about overlap: a worker that processed
    /// two submissions concurrently could still record their labels in
    /// enqueue order. Recording entry *and* exit does pin it: two
    /// overlapping submissions would show `enter:second` before
    /// `exit:first`, which this assertion would catch and a
    /// labels-only assertion could not.
    #[tokio::test]
    async fn submissions_are_serialised_in_order() {
        let (queue, receiver) = SubmissionQueue::new(capacity(8));
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&events);
        // A worker that records entry and exit and answers, standing in for
        // the real `Submitter`.
        let worker = tokio::spawn(async move {
            let mut receiver = receiver;
            while let Some(queued) = receiver.recv().await {
                let label = queued.submission.label.clone();
                recorder
                    .lock()
                    .expect("lock")
                    .push(format!("enter:{label}"));
                tokio::time::sleep(Duration::from_millis(10)).await;
                recorder.lock().expect("lock").push(format!("exit:{label}"));
                let _ = queued.respond.send(Ok(fake_outcome()));
            }
        });

        let first = queue.enqueue(submission("first"));
        let second = queue.enqueue(submission("second"));
        let third = queue.enqueue(submission("third"));
        let (a, b, c) = tokio::join!(first, second, third);
        assert!(a.is_ok() && b.is_ok() && c.is_ok());
        drop(queue);
        worker.await.expect("worker");
        assert_eq!(
            *events.lock().expect("lock"),
            vec![
                "enter:first".to_string(),
                "exit:first".to_string(),
                "enter:second".to_string(),
                "exit:second".to_string(),
                "enter:third".to_string(),
                "exit:third".to_string(),
            ],
            "a submission's exit must precede the next one's entry"
        );
    }

    /// Each caller gets its own answer back, not another caller's.
    #[tokio::test]
    async fn each_caller_receives_its_own_outcome() {
        // The worker answers with an outcome carrying a hash derived from
        // the label it received, so a crossed wire is visible — a wrong
        // hash — rather than merely possible.
        let (queue, receiver) = SubmissionQueue::new(capacity(8));
        let worker = tokio::spawn(async move {
            let mut receiver = receiver;
            while let Some(queued) = receiver.recv().await {
                let outcome = outcome_for(&queued.submission.label);
                let _ = queued.respond.send(Ok(outcome));
            }
        });

        let first = queue.enqueue(submission("first"));
        let second = queue.enqueue(submission("second"));
        let third = queue.enqueue(submission("third"));
        let (a, b, c) = tokio::join!(first, second, third);
        drop(queue);
        worker.await.expect("worker");

        for (result, label) in [(a, "first"), (b, "second"), (c, "third")] {
            let TxOutcome::Succeeded { hash, .. } = result.expect("submission succeeds") else {
                panic!("expected a succeeded outcome");
            };
            assert_eq!(
                hash,
                hash_for(label),
                "{label}'s caller received another submission's outcome"
            );
        }
    }

    /// A queue whose worker is gone answers `Closed` rather than hanging:
    /// a caller awaiting a submission that can never be made would hold the
    /// auctioneer's tick forever.
    #[tokio::test]
    async fn a_closed_queue_does_not_hang_its_callers() {
        let (queue, receiver) = SubmissionQueue::new(capacity(1));
        drop(receiver);
        assert!(matches!(
            queue.enqueue(submission("orphan")).await,
            Err(QueueError::Closed)
        ));
    }

    /// A submission that fails does not stop the queue: the next one is
    /// still prepared and sent. One borrower's contract error is not the
    /// bot's.
    #[tokio::test]
    async fn a_failed_submission_does_not_stop_the_queue() {
        let (queue, receiver) = SubmissionQueue::new(capacity(8));
        let worker = tokio::spawn(async move {
            let mut receiver = receiver;
            let mut seen = 0_u32;
            while let Some(queued) = receiver.recv().await {
                seen += 1;
                let answer = if seen == 1 {
                    Err(QueueError::Chain(ChainError::Rejected(
                        "contract refused".to_string(),
                    )))
                } else {
                    Ok(fake_outcome())
                };
                let _ = queued.respond.send(answer);
            }
        });

        let first = queue.enqueue(submission("first")).await;
        let second = queue.enqueue(submission("second")).await;
        drop(queue);
        worker.await.expect("worker");

        assert!(
            matches!(first, Err(QueueError::Chain(ChainError::Rejected(_)))),
            "{first:?}"
        );
        assert!(second.is_ok(), "{second:?}");
    }

    /// `run_queue` itself — not a stand-in worker — never attempts a
    /// submission once `shutdown` is set, and it answers the caller
    /// promptly instead of leaving `enqueue` waiting: a caller blocked here
    /// would stall the auctioneer's whole tick. Proven against the real
    /// `Submitter` and an `RpcClient` with nothing scripted, so any attempt
    /// to submit would show up as an unscripted-call failure rather than
    /// silently succeeding.
    #[tokio::test]
    async fn run_queue_answers_rather_than_attempts_after_shutdown() {
        let rpc = crate::chain::script::ScriptedRpc::start().await;
        let client = crate::chain::rpc::RpcClient::new(&rpc.url(), None).expect("rpc client");
        let key = ed25519_dalek::SigningKey::from_bytes(&[3_u8; 32]);
        let secret = stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string();
        let signer = crate::chain::signer::Signer::from_secret(&secret).expect("signer");
        let network = crate::chain::signer::Network::testnet();
        let config = crate::chain::tx::TxConfig::new(100, 200, 3);
        let submitter = Submitter::new(&client, &network, &signer, config);

        let (_flag, shutdown) = watch::channel(true);
        let (queue, receiver) = SubmissionQueue::new(capacity(1));

        let respond = async {
            let answer =
                tokio::time::timeout(Duration::from_secs(1), queue.enqueue(submission("orphan")))
                    .await
                    .expect("enqueue must not hang while the queue is shutting down");
            drop(queue);
            answer
        };
        let (answer, ()) = tokio::join!(respond, run_queue(&submitter, receiver, &shutdown));

        assert!(
            matches!(answer, Err(QueueError::ShuttingDown)),
            "shutdown is its own answer, never a chain error: {answer:?}"
        );
        assert!(
            rpc.calls("getLedgerEntries").is_empty(),
            "a shutting-down queue must never attempt a submission"
        );
    }

    /// A signing key for the queue's own submission tests: no real funds,
    /// and no relation to any fixture account. Everything it signs is
    /// signed against what the scripted RPC hands back for it.
    fn signer() -> Signer {
        let key = ed25519_dalek::SigningKey::from_bytes(&[3_u8; 32]);
        let secret = stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string();
        Signer::from_secret(&secret).expect("signer")
    }

    /// Millisecond timings, and a wait cap of zero so every `wait_for`
    /// polls `getTransaction` exactly once before giving up as `Unknown`.
    /// The repeated polling these tests observe is then the queue's own
    /// resolution loop and never `wait_for`'s internal one, which is the
    /// thing under test.
    fn tx_config() -> TxConfig {
        TxConfig {
            poll_interval: Duration::from_millis(1),
            send_retry_pause: Duration::ZERO,
            wait_cap: Duration::ZERO,
            ..TxConfig::new(100, 200, 3)
        }
    }

    /// What a `Submitter` borrows, held together so a test can build one in
    /// a line: the submitter borrows all three for as long as it lives.
    struct Chain {
        client: RpcClient,
        network: Network,
        signer: Signer,
    }

    impl Chain {
        fn new(url: &str) -> Self {
            Self {
                client: RpcClient::new(url, None).expect("rpc client"),
                network: Network::testnet(),
                signer: signer(),
            }
        }

        fn submitter(&self) -> Submitter<'_> {
            Submitter::new(&self.client, &self.network, &self.signer, tx_config())
        }
    }

    /// A policy fast enough for a test; the shape of `RetryPolicy::DEFAULT`.
    fn quick() -> RetryPolicy {
        RetryPolicy {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(4),
            resolve_pause: Duration::from_millis(1),
        }
    }

    fn submission_with(label: &str, retries: u32) -> Submission {
        Submission {
            retries,
            ..submission(label)
        }
    }

    /// One submission through `run_queue_with`, bounded so a queue that
    /// never settles fails the test instead of hanging it. The queue is
    /// dropped the moment the answer arrives, which is what ends the
    /// worker.
    async fn run_one(
        submitter: &Submitter<'_>,
        shutdown: &watch::Receiver<bool>,
        policy: RetryPolicy,
        retries: u32,
    ) -> Result<TxOutcome, QueueError> {
        let (queue, receiver) = SubmissionQueue::new(capacity(1));
        let enqueue = async {
            let answer = queue.enqueue(submission_with("creation", retries)).await;
            drop(queue);
            answer
        };
        let worker = run_queue_with(submitter, receiver, shutdown, policy);
        tokio::time::timeout(Duration::from_secs(5), async {
            let (answer, ()) = tokio::join!(enqueue, worker);
            answer
        })
        .await
        .expect("the queue must settle a submission rather than hang on it")
    }

    /// The JSON-RPC method of every request the scripted server saw, in the
    /// order it saw them: `calls` groups by method and so cannot say which
    /// of two different methods came first.
    async fn methods_in_order(rpc: &ScriptedRpc) -> Vec<String> {
        rpc.received()
            .await
            .iter()
            .map(|request| {
                let body: Value = serde_json::from_slice(&request.body).expect("a JSON body");
                body["method"].as_str().expect("a method").to_string()
            })
            .collect()
    }

    /// Every index in `methods` holding `method`.
    fn positions_of(methods: &[String], method: &str) -> Vec<usize> {
        methods
            .iter()
            .enumerate()
            .filter(|(_, name)| name.as_str() == method)
            .map(|(index, _)| index)
            .collect()
    }

    /// A send whose answer was lost may still have been forwarded, so the
    /// queue asks the chain about the hash it holds — and finds it landed.
    /// Resending would have been a second transaction for one plan.
    #[tokio::test]
    async fn a_send_whose_answer_was_lost_is_resolved_by_hash_never_resent() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        // The RPC never answered this send. It may have forwarded the
        // envelope all the same, so the only safe question is what became
        // of the hash — not whether to send it again.
        rpc.expect_http("sendTransaction", 500);
        script_transaction_success(&rpc, 101);

        let (_flag, shutdown) = watch::channel(false);
        let answer = run_one(&chain.submitter(), &shutdown, quick(), 3).await;

        assert!(
            matches!(answer, Ok(TxOutcome::Succeeded { .. })),
            "the transaction landed under the hash the queue held: {answer:?}"
        );
        assert_eq!(
            rpc.calls("sendTransaction").len(),
            1,
            "a send whose answer was lost is never sent again"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            1,
            "and nothing is prepared again either"
        );
        assert_eq!(rpc.remaining(), 0);
    }

    /// An outcome the RPC cannot yet name is polled until it can be named.
    /// Moving on would let the next submission for this key be prepared
    /// against a sequence number this transaction may still consume.
    #[tokio::test]
    async fn an_unknown_outcome_is_polled_until_it_is_terminal() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(&rpc, "PENDING", None, 100);
        // The RPC's latest ledger stays far under the window's max ledger
        // of 104, so none of these proves the transaction expired: each
        // `wait_for` gives up as `Unknown` at the wait cap, and the queue
        // must ask again rather than call the submission done.
        for _ in 0..3 {
            script_transaction_not_found(&rpc, 100, 1);
        }
        script_transaction_success(&rpc, 101);

        let (_flag, shutdown) = watch::channel(false);
        let answer = run_one(&chain.submitter(), &shutdown, quick(), 3).await;

        assert!(
            matches!(answer, Ok(TxOutcome::Succeeded { .. })),
            "the queue waited for the terminal outcome: {answer:?}"
        );
        assert_eq!(
            rpc.calls("getTransaction").len(),
            4,
            "three unknown answers were polled past, not accepted"
        );
        assert_eq!(rpc.calls("sendTransaction").len(), 1);
        assert_eq!(rpc.remaining(), 0);
    }

    /// A second submission is not prepared while the first is unresolved.
    #[tokio::test]
    async fn nothing_else_is_sent_for_the_key_until_the_first_is_settled() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        // The first submission: prepared, sent, unknown twice, then landed.
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(&rpc, "PENDING", None, 100);
        script_transaction_not_found(&rpc, 100, 1);
        script_transaction_not_found(&rpc, 100, 1);
        script_transaction_success(&rpc, 101);
        // The second: a fresh account read — at the sequence the first one
        // consumed — and its own send.
        script_prepare_prelude(&rpc, &chain.signer, 11, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(&rpc, "PENDING", None, 100);
        script_transaction_success(&rpc, 101);

        let (_flag, shutdown) = watch::channel(false);
        let submitter = chain.submitter();
        let (queue, receiver) = SubmissionQueue::new(capacity(2));
        let enqueue_both = async {
            let first = queue.enqueue(submission_with("first", 0));
            let second = queue.enqueue(submission_with("second", 0));
            let answers = tokio::join!(first, second);
            drop(queue);
            answers
        };
        let ((first, second), ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                enqueue_both,
                run_queue_with(&submitter, receiver, &shutdown, quick())
            )
        })
        .await
        .expect("both submissions must settle");

        assert!(
            matches!(first, Ok(TxOutcome::Succeeded { .. })),
            "{first:?}"
        );
        assert!(
            matches!(second, Ok(TxOutcome::Succeeded { .. })),
            "{second:?}"
        );
        let methods = methods_in_order(&rpc).await;
        let account_reads = positions_of(&methods, "getLedgerEntries");
        let polls = positions_of(&methods, "getTransaction");
        assert_eq!(account_reads.len(), 2, "{methods:?}");
        assert_eq!(polls.len(), 4, "{methods:?}");
        assert!(
            account_reads[1] > polls[2],
            "the second submission read this key's sequence number before \
             the first submission's outcome was known: {methods:?}"
        );
        assert_eq!(rpc.remaining(), 0);
    }

    /// The RPC refusing the envelope proves nothing landed, so the queue
    /// prepares again from fresh state — a new sequence read and a new
    /// simulation — within the submission's budget.
    #[tokio::test]
    async fn a_refused_send_is_retried_with_a_fresh_prepare() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(
            &rpc,
            "ERROR",
            Some(TransactionResultResult::TxInsufficientFee),
            100,
        );
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(&rpc, "PENDING", None, 100);
        script_transaction_success(&rpc, 101);

        let (_flag, shutdown) = watch::channel(false);
        let answer = run_one(&chain.submitter(), &shutdown, quick(), 3).await;

        assert!(
            matches!(answer, Ok(TxOutcome::Succeeded { .. })),
            "the retry landed: {answer:?}"
        );
        assert_eq!(rpc.calls("sendTransaction").len(), 2);
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            2,
            "the retry is a fresh prepare, never the same envelope again"
        );
        assert_eq!(rpc.calls("getLedgerEntries").len(), 2);
        assert_eq!(rpc.remaining(), 0);
    }

    /// The budget is a bound: `retries: 2` is three attempts in all.
    #[tokio::test]
    async fn retries_stop_at_the_submissions_budget() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        for _ in 0..3 {
            script_prepare_prelude(&rpc, &chain.signer, 10, 100);
            script_simulate_accepted(&rpc, 100);
            script_send(
                &rpc,
                "ERROR",
                Some(TransactionResultResult::TxInsufficientFee),
                100,
            );
        }

        let (_flag, shutdown) = watch::channel(false);
        let answer = run_one(&chain.submitter(), &shutdown, quick(), 2).await;

        assert!(
            matches!(answer, Err(QueueError::Chain(ChainError::Rejected(_)))),
            "the budget ran out and the failure went back to the caller: {answer:?}"
        );
        assert_eq!(
            rpc.calls("sendTransaction").len(),
            3,
            "two retries after the first attempt, and no more"
        );
        assert_eq!(rpc.remaining(), 0);
    }

    /// A bad sequence means the plan is stale: it goes back to the caller to
    /// be rebuilt, however much budget is left.
    #[tokio::test]
    async fn a_bad_sequence_goes_back_to_the_caller() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(&rpc, "ERROR", Some(TransactionResultResult::TxBadSeq), 100);

        let (_flag, shutdown) = watch::channel(false);
        let answer = run_one(&chain.submitter(), &shutdown, quick(), 5).await;

        assert!(
            matches!(answer, Err(QueueError::Chain(ChainError::BadSequence))),
            "a stale plan is the caller's to rebuild: {answer:?}"
        );
        assert_eq!(
            rpc.calls("sendTransaction").len(),
            1,
            "five retries left, and none of them used on a stale plan"
        );
        assert_eq!(rpc.remaining(), 0);
    }

    /// A contract refusal at prepare cannot change on a retry.
    #[tokio::test]
    async fn a_contract_refusal_goes_back_to_the_caller() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        // The signing path's prelude: `prepare` reads the fee stats before
        // it simulates, so a simulate-only prelude would fail this on the
        // fee read rather than on the contract's answer.
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_refused(&rpc, 1_205, 100);

        let (_flag, shutdown) = watch::channel(false);
        let answer = run_one(&chain.submitter(), &shutdown, quick(), 5).await;

        assert!(
            matches!(
                answer,
                Err(QueueError::Chain(ChainError::Simulation {
                    contract_error: Some(1_205),
                    ..
                }))
            ),
            "a refusal is an answer, not a transient failure: {answer:?}"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            1,
            "nothing a retry could change, so nothing was retried"
        );
        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "a refused simulation is never sent"
        );
        assert_eq!(rpc.remaining(), 0);
    }

    /// An expired transaction never landed; whether to try again is the
    /// caller's decision, made against fresh state (spec §8).
    #[tokio::test]
    async fn an_expired_transaction_goes_back_to_the_caller() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(&rpc, "PENDING", None, 100);
        // 105 is past the window's max ledger of 104 and the retention
        // still reaches back to its min ledger of 100, so this `NOT_FOUND`
        // is proof the transaction never applied.
        script_transaction_not_found(&rpc, 105, 1);

        let (_flag, shutdown) = watch::channel(false);
        let answer = run_one(&chain.submitter(), &shutdown, quick(), 5).await;

        assert!(
            matches!(answer, Ok(TxOutcome::Expired { .. })),
            "expiry is a terminal outcome the caller re-plans from: {answer:?}"
        );
        assert_eq!(
            rpc.calls("sendTransaction").len(),
            1,
            "an expired transaction is not resent behind the caller's back"
        );
        assert_eq!(rpc.calls("getTransaction").len(), 1);
        assert_eq!(rpc.remaining(), 0);
    }

    /// Shutdown cuts a backoff short: the caller hears the failure at once.
    #[tokio::test]
    async fn shutdown_cuts_a_backoff_short() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(
            &rpc,
            "ERROR",
            Some(TransactionResultResult::TxInsufficientFee),
            100,
        );

        let (flag, shutdown) = watch::channel(false);
        // An hour before the retry, and a ceiling that allows it: nothing
        // but the shutdown request can end this wait inside a test.
        let policy = RetryPolicy {
            initial: Duration::from_hours(1),
            max: Duration::from_hours(1),
            ..quick()
        };
        let raise_once_sent = async {
            while rpc.calls("sendTransaction").is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            flag.send(true).expect("the queue still holds a receiver");
        };
        let submitter = chain.submitter();
        let (answer, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(run_one(&submitter, &shutdown, policy, 5), raise_once_sent)
        })
        .await
        .expect("shutdown must cut the backoff short, not wait it out");

        assert!(
            matches!(answer, Err(QueueError::Chain(ChainError::Rejected(_)))),
            "the caller hears the failure that was waiting to be retried: {answer:?}"
        );
        assert_eq!(
            rpc.calls("sendTransaction").len(),
            1,
            "the backoff was cut short, so the retry never happened"
        );
    }

    /// A dropped shutdown sender is not a shutdown. `watch`'s wait answers
    /// an error rather than a request once the last sender is gone, and
    /// reading that as "stopping" would hand an `Unknown` outcome back
    /// while the flag still says the bot is running — freeing this key for
    /// a submission prepared against a sequence number the transaction in
    /// flight may still consume, which is the race this queue exists to
    /// close.
    #[tokio::test]
    async fn a_dropped_shutdown_sender_does_not_end_the_resolution() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(&rpc, "PENDING", None, 100);
        script_transaction_not_found(&rpc, 100, 1);
        script_transaction_not_found(&rpc, 100, 1);
        script_transaction_success(&rpc, 101);

        let (flag, shutdown) = watch::channel(false);
        // Nothing can ever request a shutdown from here on.
        drop(flag);
        let answer = run_one(&chain.submitter(), &shutdown, quick(), 0).await;

        assert!(
            matches!(answer, Ok(TxOutcome::Succeeded { .. })),
            "the queue kept polling until the outcome was terminal: {answer:?}"
        );
        assert_eq!(
            rpc.calls("getTransaction").len(),
            3,
            "two unknown answers were polled past, not read as a shutdown"
        );
        assert_eq!(rpc.remaining(), 0);
    }

    /// `max` is the longest pause, the first one included: a policy whose
    /// `initial` sits above it waits `max`, not `initial`. Unclamped, this
    /// test would wait an hour for a retry and fail on `run_one`'s bound.
    #[tokio::test]
    async fn the_first_backoff_is_clamped_to_the_policys_maximum() {
        let rpc = ScriptedRpc::start().await;
        let chain = Chain::new(&rpc.url());
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(
            &rpc,
            "ERROR",
            Some(TransactionResultResult::TxInsufficientFee),
            100,
        );
        script_prepare_prelude(&rpc, &chain.signer, 10, 100);
        script_simulate_accepted(&rpc, 100);
        script_send(&rpc, "PENDING", None, 100);
        script_transaction_success(&rpc, 101);

        let (_flag, shutdown) = watch::channel(false);
        let policy = RetryPolicy {
            initial: Duration::from_hours(1),
            max: Duration::from_millis(1),
            ..quick()
        };
        let answer = run_one(&chain.submitter(), &shutdown, policy, 3).await;

        assert!(
            matches!(answer, Ok(TxOutcome::Succeeded { .. })),
            "the retry waited `max`, not `initial`: {answer:?}"
        );
        assert_eq!(rpc.calls("sendTransaction").len(), 2);
        assert_eq!(rpc.remaining(), 0);
    }
}
