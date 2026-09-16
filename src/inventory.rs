//! The filler's wallet inventory: balances per asset, a fee reserve
//! withheld from the native asset, and the [`Reservation`] tokens a plan
//! takes for what it will spend.
//!
//! Spec §5: "A plan takes a must-use `Reservation` for the wallet amounts
//! it will spend; the token is consumed or released by value exactly once,
//! carries the manager it was issued by, and saturates rather than errors
//! so the ledger only protects callers that honour it." Spec §8: settlement
//! "happens on every non-panicking path, including early returns and task
//! cancellation during shutdown, through a drop guard that releases an
//! unsettled token and logs a warning."
//!
//! This inventory tracks wallet balances only (ruling 5): a plan's
//! positions come from its own chain snapshot, never from here.
//!
//! Its arithmetic **saturates by design**, unlike the rest of this crate,
//! where arithmetic on chain values is checked, never silently saturated.
//! This ledger is not money — it is a bookkeeping claim against a wallet
//! whose real balance only [`Inventory::record_balances`] ever reports.
//! Saturating a subtraction here cannot lose or fabricate a stroop on
//! chain; it can only let this claim briefly disagree with reality, and
//! the next read corrects that drift. Checked arithmetic would instead
//! mean a bug in this bookkeeping — or a balance that simply arrived
//! smaller than a stale reservation expected — panics or errors a filler
//! that has nothing wrong with the chain state it is about to act on.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::chain::{ChainError, PoolReader};

/// The wallet state behind an [`Inventory`]: the last balances read from
/// chain, what is currently held by open reservations, when the balances
/// were last read, and the fee asset withheld from spending.
#[derive(Debug)]
struct Ledger {
    /// The last balance read per asset, from [`Inventory::record_balances`].
    balances: BTreeMap<String, i128>,
    /// What open [`Reservation`]s currently hold, per asset.
    reserved: BTreeMap<String, i128>,
    /// When `balances` was last replaced; `None` before the first read.
    read_at: Option<Instant>,
    /// The asset `fee_reserve` is withheld from — the native asset, in
    /// practice, since that is what pays transaction fees.
    fee_asset: String,
    /// Held back from `fee_asset` so a plan never spends the wallet down
    /// to the point it cannot pay its own fees.
    fee_reserve: i128,
}

impl Ledger {
    fn available(&self, asset: &str) -> i128 {
        let balance = self.balances.get(asset).copied().unwrap_or(0);
        let reserved = self.reserved.get(asset).copied().unwrap_or(0);
        let withheld = if asset == self.fee_asset {
            self.fee_reserve
        } else {
            0
        };
        balance
            .saturating_sub(reserved)
            .saturating_sub(withheld)
            .max(0)
    }
}

/// Locks the ledger. A poisoned lock is recovered rather than propagated:
/// the ledger holds no invariant a panic mid-update could break that the
/// next read does not repair, and a filler that stops for good over one is
/// worse than one that re-reads its wallet.
fn lock(ledger: &Mutex<Ledger>) -> MutexGuard<'_, Ledger> {
    ledger.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Subtracts `amount` from `map[asset]`, saturating at zero. A missing key
/// is left absent rather than inserted at a negative value: nothing here
/// should ever debit an asset it never held a claim on.
fn subtract(map: &mut BTreeMap<String, i128>, asset: &str, amount: i128) {
    if let Some(value) = map.get_mut(asset) {
        *value = value.saturating_sub(amount).max(0);
    }
}

/// The filler's view of its own wallet: balances read from chain, and the
/// [`Reservation`]s plans currently hold against them. Cloning the handle
/// is not offered; every reader shares the one `Arc` a `Reservation` also
/// holds, so a plan's claim and a fresh read always agree on which ledger
/// they describe.
#[derive(Debug)]
pub struct Inventory {
    ledger: Arc<Mutex<Ledger>>,
}

impl Inventory {
    /// A wallet with no balances yet — [`Inventory::stale`] is `true`
    /// until the first [`Inventory::record_balances`] — withholding
    /// `fee_reserve` of `fee_asset` from what any plan may spend.
    #[must_use]
    pub fn new(fee_asset: String, fee_reserve: i128) -> Self {
        Self {
            ledger: Arc::new(Mutex::new(Ledger {
                balances: BTreeMap::new(),
                reserved: BTreeMap::new(),
                read_at: None,
                fee_asset,
                fee_reserve,
            })),
        }
    }

    /// Replaces the tracked balances with a fresh chain read taken at
    /// `at`. Reservations already open survive: a plan in flight still
    /// owns what it reserved regardless of what the wallet now shows — the
    /// balances are what moved, not the claims against them.
    pub fn record_balances(&self, balances: BTreeMap<String, i128>, at: Instant) {
        let mut ledger = lock(&self.ledger);
        ledger.balances = balances;
        ledger.read_at = Some(at);
    }

    /// `true` when the balances were never read, or were last read more
    /// than `max_age` before `now`.
    #[must_use]
    pub fn stale(&self, now: Instant, max_age: Duration) -> bool {
        match lock(&self.ledger).read_at {
            None => true,
            Some(read_at) => now.saturating_duration_since(read_at) > max_age,
        }
    }

    /// What every tracked asset currently has free to spend: the last read
    /// balance, less any open reservations and, for the fee asset, the fee
    /// reserve — never below zero.
    #[must_use]
    pub fn available(&self) -> BTreeMap<String, i128> {
        let ledger = lock(&self.ledger);
        ledger
            .balances
            .keys()
            .chain(ledger.reserved.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|asset| (asset.clone(), ledger.available(asset)))
            .collect()
    }

    /// What is currently held by open reservations, per asset.
    #[must_use]
    pub fn reserved(&self) -> BTreeMap<String, i128> {
        lock(&self.ledger).reserved.clone()
    }

    /// Takes a [`Reservation`] for `amounts`, or refuses with the first
    /// asset that asks for more than [`Inventory::available`] currently
    /// shows. Every asset is checked before any is reserved, so a refusal
    /// claims nothing.
    ///
    /// # Errors
    ///
    /// [`InventoryError::InvalidAmount`] when some asset in `amounts` asks
    /// for zero or less — a negative claim would *raise* what is available
    /// and, on settlement, fabricate balance, so the ledger refuses it at
    /// the boundary rather than trusting every caller to size a spend
    /// positive — and [`InventoryError::Insufficient`] when some asset asks
    /// for more than is currently available.
    pub fn reserve(&self, amounts: &BTreeMap<String, i128>) -> Result<Reservation, InventoryError> {
        let mut ledger = lock(&self.ledger);
        for (asset, needed) in amounts {
            if *needed <= 0 {
                return Err(InventoryError::InvalidAmount {
                    asset: asset.clone(),
                    amount: *needed,
                });
            }
            let available = ledger.available(asset);
            if *needed > available {
                return Err(InventoryError::Insufficient {
                    asset: asset.clone(),
                    needed: *needed,
                    available,
                });
            }
        }
        for (asset, amount) in amounts {
            let held = ledger.reserved.entry(asset.clone()).or_insert(0);
            *held = held.saturating_add(*amount);
        }
        drop(ledger);
        Ok(Reservation {
            amounts: amounts.clone(),
            ledger: Arc::clone(&self.ledger),
            settled: false,
        })
    }
}

/// A plan's claim on the wallet for `amounts`: out of what
/// [`Inventory::available`] shows until it is [`Reservation::consume`]d,
/// [`Reservation::release`]d, or dropped unsettled, which releases it and
/// logs a warning (spec §8). Settlement is by value and exactly once —
/// there is no other way to close one out.
#[must_use]
#[derive(Debug)]
pub struct Reservation {
    amounts: BTreeMap<String, i128>,
    ledger: Arc<Mutex<Ledger>>,
    settled: bool,
}

impl Reservation {
    /// What this reservation holds, by asset.
    #[must_use]
    pub fn amounts(&self) -> &BTreeMap<String, i128> {
        &self.amounts
    }

    /// Marks these amounts spent: debited from both the open reservation
    /// and the tracked balance, so the wallet reflects the spend before
    /// the next chain read confirms it.
    pub fn consume(mut self) {
        self.settled = true;
        let mut ledger = lock(&self.ledger);
        for (asset, amount) in &self.amounts {
            subtract(&mut ledger.reserved, asset, *amount);
            subtract(&mut ledger.balances, asset, *amount);
        }
    }

    /// Returns these amounts to what later plans may spend, unspent.
    pub fn release(mut self) {
        self.settled = true;
        let mut ledger = lock(&self.ledger);
        for (asset, amount) in &self.amounts {
            subtract(&mut ledger.reserved, asset, *amount);
        }
    }
}

impl Drop for Reservation {
    /// An unsettled reservation — an early return, a cancelled task — is
    /// released rather than leaked, exactly as [`Reservation::release`]
    /// would, and logs a warning: this path means a caller did not settle
    /// its own token (spec §8).
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        tracing::warn!(
            amounts = ?self.amounts,
            "a reservation was dropped unsettled; releasing it"
        );
        let mut ledger = lock(&self.ledger);
        for (asset, amount) in &self.amounts {
            subtract(&mut ledger.reserved, asset, *amount);
        }
    }
}

/// What travels with a plan to the executor: a live reservation, or the
/// statement that this is a dry run and nothing was reserved. The executor
/// refuses the one that does not match its mode, so the two cannot be
/// mixed up (spec §5).
#[derive(Debug)]
pub enum Settlement {
    /// A live plan's claim on the wallet.
    Live(Reservation),
    /// A dry run: nothing was reserved, and nothing may be spent.
    DryRun,
}

/// A reservation this inventory refuses.
#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    /// `asset` was asked for zero or less, which is not a claim at all: a
    /// negative one would invert the ledger. No plan produces one — every
    /// spend is a positive repay or supply — so this is a bug upstream,
    /// refused here so it cannot become a phantom balance.
    #[error("invalid reservation of {amount} {asset}: a claim is positive")]
    InvalidAmount {
        /// The asset.
        asset: String,
        /// What was asked for.
        amount: i128,
    },
    /// `asset` was asked for more than [`Inventory::available`] showed at
    /// the time.
    #[error("insufficient {asset}: needed {needed}, available {available}")]
    Insufficient {
        /// The asset that fell short.
        asset: String,
        /// What the reservation asked for.
        needed: i128,
        /// What was actually free when it was asked.
        available: i128,
    },
}

/// Reads `account`'s balance of every asset in `assets`, one `balance`
/// simulation per asset, in order, into one map. Any failure is returned
/// as-is; the caller's previous [`Inventory`] balances are left exactly as
/// they were, since this function never writes to an `Inventory` itself.
///
/// # Errors
///
/// Whatever [`PoolReader::balance`] returns for the failing asset.
pub async fn read_balances(
    reader: &PoolReader<'_>,
    account: &str,
    assets: &BTreeSet<String>,
) -> Result<BTreeMap<String, i128>, ChainError> {
    let mut balances = BTreeMap::new();
    for asset in assets {
        let (_ledger, amount) = reader.balance(asset, account).await?;
        balances.insert(asset.clone(), amount);
    }
    Ok(balances)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::{scval_b64, transaction_data_b64, ScriptedRpc};
    use crate::chain::xdr::encode::i128_val;
    use serde_json::{json, Value};

    const XLM: &str = "CXLM";
    const USDC: &str = "CUSDC";

    fn inventory() -> Inventory {
        let inventory = Inventory::new(XLM.to_string(), 500_000_000);
        inventory.record_balances(
            BTreeMap::from([(XLM.to_string(), 2_000_000_000), (USDC.to_string(), 700)]),
            Instant::now(),
        );
        inventory
    }

    /// The fee reserve is held back from XLM and from nothing else, and
    /// nothing is ever less than zero available.
    #[test]
    fn the_fee_reserve_is_withheld_from_the_native_asset_only() {
        let inventory = inventory();
        assert_eq!(inventory.available()[XLM], 1_500_000_000);
        assert_eq!(inventory.available()[USDC], 700);
        let poor = Inventory::new(XLM.to_string(), 500_000_000);
        poor.record_balances(BTreeMap::from([(XLM.to_string(), 100)]), Instant::now());
        assert_eq!(poor.available()[XLM], 0);
    }

    /// A reservation is out of what later plans may spend until it is
    /// settled; consumed, it has been spent; released, it is back.
    #[test]
    fn a_reservation_holds_until_it_is_settled() {
        let inventory = inventory();
        let held = inventory
            .reserve(&BTreeMap::from([(USDC.to_string(), 300)]))
            .unwrap();
        assert_eq!(inventory.available()[USDC], 400);
        held.release();
        assert_eq!(inventory.available()[USDC], 700);
        let spent = inventory
            .reserve(&BTreeMap::from([(USDC.to_string(), 300)]))
            .unwrap();
        spent.consume();
        assert_eq!(
            inventory.available()[USDC],
            400,
            "debited until the next read"
        );
        assert!(inventory.reserved().values().all(|amount| *amount == 0));
    }

    /// A plan sized against a view that has since changed is refused, and
    /// the refusal names the asset.
    #[test]
    fn more_than_is_available_is_refused() {
        let inventory = inventory();
        let _first = inventory
            .reserve(&BTreeMap::from([(USDC.to_string(), 600)]))
            .unwrap();
        let error = inventory
            .reserve(&BTreeMap::from([(USDC.to_string(), 200)]))
            .expect_err("only 100 is left");
        assert!(
            matches!(&error, InventoryError::Insufficient { asset, needed: 200, available: 100 } if asset == USDC)
        );
    }

    /// A claim is positive: zero and negative amounts are refused at the
    /// boundary, and a refused reservation claims nothing — a negative one
    /// let through would raise `available` now and fabricate balance on
    /// settlement.
    #[test]
    fn a_non_positive_amount_is_refused() {
        let inventory = inventory();
        for amount in [0, -1, -700] {
            let error = inventory
                .reserve(&BTreeMap::from([
                    (XLM.to_string(), 1),
                    (USDC.to_string(), amount),
                ]))
                .expect_err("not a claim");
            assert!(
                matches!(&error, InventoryError::InvalidAmount { asset, amount: got } if asset == USDC && *got == amount),
                "{error}"
            );
        }
        assert_eq!(
            inventory.available()[XLM],
            1_500_000_000,
            "nothing was claimed"
        );
        assert!(inventory.reserved().values().all(|amount| *amount == 0));
    }

    /// The drop guard: a reservation nobody settled — an early return, a
    /// cancelled task — is released, not leaked.
    #[test]
    fn an_unsettled_reservation_is_released_when_dropped() {
        let inventory = inventory();
        {
            let _forgotten = inventory
                .reserve(&BTreeMap::from([(USDC.to_string(), 300)]))
                .unwrap();
        }
        assert_eq!(inventory.available()[USDC], 700);
    }

    /// A reservation settles against the inventory that issued it, by
    /// construction.
    #[test]
    fn a_reservation_never_touches_another_inventory() {
        let first = inventory();
        let second = inventory();
        first
            .reserve(&BTreeMap::from([(USDC.to_string(), 300)]))
            .unwrap()
            .consume();
        assert_eq!(second.available()[USDC], 700);
    }

    /// A fresh read replaces the balances and keeps live reservations: a
    /// plan in flight still owns what it reserved.
    #[test]
    fn a_fresh_read_keeps_live_reservations() {
        let inventory = inventory();
        let held = inventory
            .reserve(&BTreeMap::from([(USDC.to_string(), 300)]))
            .unwrap();
        inventory.record_balances(BTreeMap::from([(USDC.to_string(), 1_000)]), Instant::now());
        assert_eq!(inventory.available()[USDC], 700);
        held.release();
    }

    /// Saturation: consuming after a read that already shows the spend
    /// leaves zero, never a negative balance.
    #[test]
    fn settling_saturates_rather_than_going_negative() {
        let inventory = inventory();
        let held = inventory
            .reserve(&BTreeMap::from([(USDC.to_string(), 700)]))
            .unwrap();
        inventory.record_balances(BTreeMap::from([(USDC.to_string(), 0)]), Instant::now());
        held.consume();
        assert_eq!(inventory.available()[USDC], 0);
    }

    /// Never read is stale; read now is not; read long enough ago is.
    #[test]
    fn balances_go_stale() {
        let never = Inventory::new(XLM.to_string(), 0);
        assert!(never.stale(Instant::now(), Duration::from_secs(30)));
        let read = Instant::now();
        let fresh = Inventory::new(XLM.to_string(), 0);
        fresh.record_balances(BTreeMap::new(), read);
        assert!(!fresh.stale(read, Duration::from_secs(30)));
        assert!(fresh.stale(read + Duration::from_secs(31), Duration::from_secs(30)));
    }

    /// A bare `simulateTransaction` answer for a `balance` view call, as
    /// `chain::pool`'s own `simulation` test helper builds it: a return
    /// value and the ledger it was read at, with no footprint to restore.
    fn simulation(return_xdr: &str, ledger: u32) -> Value {
        json!({"transactionData": transaction_data_b64(1), "events": [],
               "minResourceFee": "1", "results": [{"auth": [], "xdr": return_xdr}],
               "latestLedger": ledger})
    }

    /// One `balance` simulation per asset, read into one map.
    #[tokio::test]
    async fn balances_are_read_per_asset() {
        // Real strkeys, unlike the module's `XLM`/`USDC` placeholders
        // above: these round-trip through `sc_address` on the way into
        // the simulated envelope, so they must actually decode.
        const NATIVE: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
        const TOKEN: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
        const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
        const FILLER: &str = "GCIH7OYRDHJ3IOPFEM7DMUX3SXTVHOO2XSWLGBMSVQ3EIHPHYUTNJID3";

        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "simulateTransaction",
            simulation(&scval_b64(&i128_val(2_000_000_000)), 10),
        );
        rpc.expect(
            "simulateTransaction",
            simulation(&scval_b64(&i128_val(700)), 11),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let reader = PoolReader::new(&client, POOL);
        let assets = BTreeSet::from([NATIVE.to_string(), TOKEN.to_string()]);

        let balances = read_balances(&reader, FILLER, &assets).await.unwrap();

        assert_eq!(
            balances,
            BTreeMap::from([
                (NATIVE.to_string(), 2_000_000_000),
                (TOKEN.to_string(), 700)
            ])
        );
        assert_eq!(rpc.calls("simulateTransaction").len(), 2);
    }
}
