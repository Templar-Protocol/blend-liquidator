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
//! The queue owns ordering, not policy: what to submit, at what priority,
//! and what a failure means are the caller's.

use tokio::sync::{mpsc, oneshot, watch};

use crate::chain::tx::{Priority, Submitter, TxOutcome};
use crate::chain::ChainError;
use stellar_xdr::Operation;

/// What a caller asks the queue to send.
#[derive(Debug)]
pub struct Submission {
    /// The operation to invoke.
    pub operation: Operation,
    /// The fee tier.
    pub priority: Priority,
    /// What this submission is, for the log line. Never a secret: this
    /// module logs it verbatim, and nothing in this module logs
    /// `operation`'s contents.
    pub label: String,
}

/// A submission paired with the channel its answer goes back on.
#[derive(Debug)]
pub struct QueuedSubmission {
    /// The work.
    pub submission: Submission,
    /// Where the outcome goes. A dropped receiver means the caller gave up;
    /// the send fails and the queue moves on.
    pub respond: oneshot::Sender<Result<TxOutcome, ChainError>>,
}

/// Why a submission did not produce an outcome.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// The queue's worker is gone, so nothing will ever be sent.
    #[error("the submission queue is closed")]
    Closed,
    /// The chain layer's own failure, passed through.
    #[error(transparent)]
    Chain(#[from] ChainError),
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
    /// unbounded backlog of stale plans.
    #[must_use]
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<QueuedSubmission>) {
        let (sender, receiver) = mpsc::channel(capacity);
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
        answer
            .await
            .map_err(|_| QueueError::Closed)?
            .map_err(QueueError::from)
    }
}

/// Drains `receiver` until every sender is dropped, submitting one at a
/// time through `submitter`. Once `shutdown` is set, no further submission
/// is attempted: each is answered with an error instead of being sent, so a
/// caller blocked on `enqueue` — the auctioneer awaiting a submission that
/// will never be made — is released rather than left waiting on a tick that
/// will never come.
///
/// A submission's failure is returned to its caller and never ends the
/// queue: one borrower's contract error is not the bot's. The shutdown flag
/// is checked between submissions, never during one — a transaction already
/// sent is waited for, because abandoning it would leave the account's
/// sequence consumed by something the bot never saw the outcome of.
///
/// The answer for a submission dequeued after shutdown is
/// [`ChainError::Config`]: no other existing variant fits without being
/// actively misleading. `Rejected` and `BadSequence` both carry specific
/// recovery meaning elsewhere (an on-chain refusal, and "re-plan and
/// resend") that a caller might reasonably act on; answering a submission
/// that was never sent with either would be a false claim about what the
/// chain did. `Config` already serves this crate as the catch-all for "an
/// operational precondition this code needs is not met" — see its other
/// call sites for `LedgerWindow` and the system clock — so labelling
/// shutdown that way is consistent with, not a stretch of, its existing use.
pub async fn run_queue(
    submitter: &Submitter<'_>,
    mut receiver: mpsc::Receiver<QueuedSubmission>,
    shutdown: &watch::Receiver<bool>,
) {
    while let Some(queued) = receiver.recv().await {
        if *shutdown.borrow() {
            tracing::debug!(
                label = queued.submission.label,
                "queue is shutting down; refusing this submission"
            );
            let _ = queued
                .respond
                .send(Err(ChainError::Config("shutting down")));
            continue;
        }
        let QueuedSubmission {
            submission,
            respond,
        } = queued;
        tracing::info!(label = submission.label, "submitting");
        let outcome = submitter
            .submit(submission.operation, submission.priority)
            .await;
        if let Err(error) = &outcome {
            tracing::warn!(label = submission.label, %error, "submission failed");
        }
        // A caller that gave up is not an error: the answer simply has
        // nowhere to go.
        let _ = respond.send(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::TxHash;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";

    fn operation() -> Operation {
        crate::chain::xdr::encode::invoke_contract_op(POOL, "bad_debt", vec![])
            .expect("a fixed operation encodes")
    }

    fn submission(label: &str) -> Submission {
        Submission {
            operation: operation(),
            priority: Priority::Normal,
            label: label.to_string(),
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
        let (queue, receiver) = SubmissionQueue::new(8);
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
        let (queue, receiver) = SubmissionQueue::new(8);
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
        let (queue, receiver) = SubmissionQueue::new(1);
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
        let (queue, receiver) = SubmissionQueue::new(8);
        let worker = tokio::spawn(async move {
            let mut receiver = receiver;
            let mut seen = 0_u32;
            while let Some(queued) = receiver.recv().await {
                seen += 1;
                let answer = if seen == 1 {
                    Err(ChainError::Rejected("contract refused".to_string()))
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
        let (queue, receiver) = SubmissionQueue::new(1);

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
            matches!(answer, Err(QueueError::Chain(ChainError::Config(_)))),
            "{answer:?}"
        );
        assert!(
            rpc.calls("getLedgerEntries").is_empty(),
            "a shutting-down queue must never attempt a submission"
        );
    }
}
