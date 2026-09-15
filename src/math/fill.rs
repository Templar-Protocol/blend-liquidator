//! The filler's arithmetic: what an auction is worth, the ledger to fill
//! it at, and — in [`plan_fill`] — the requests that keep the filler's own
//! position at or above its floor while it takes the auction over. Pure:
//! no I/O, and nothing panics.
//!
//! Values are in the pool oracle's units (`OraclePrices::scalar`), the
//! units `PositionData` reports. *Raw* values are what the filler is paid
//! and pays; *effective* values, after collateral and liability factors,
//! are what the contract's health check reads.

use std::collections::BTreeMap;

use ethnum::I256;

use super::auction::{
    bid_modifier, lot_modifier, scale_auction, AuctionData, RAMP_BLOCKS, RAMP_END_BLOCKS,
};
use super::fixed::{div_ceil, mul_ceil, mul_floor, MathError, SCALAR_7};
use super::position::{calculate_position_data, OraclePrices, PositionData, Positions};
use super::reserve::Reserve;
use crate::chain::xdr::encode::FillPercent;

/// The latest delay, in ledgers from an auction's start, a `force_fill`
/// pool waits before filling, however little the lot then covers (spec
/// §5). Also the latest the health escalation may delay such a fill to.
pub const FORCE_FILL_MAX_DELAY: u32 = 350;

/// Basis points in one.
const BPS: i128 = 10_000;

/// An auction's two sides as a position: the lot's b-tokens as collateral
/// and the bid's d-tokens as liabilities, keyed by reserve index. That is
/// the shape `calculate_position_data` values, so an auction and a
/// borrower are valued by one function and cannot disagree about rounding.
///
/// # Errors
///
/// `MathError::InvalidInput` when the auction names an asset that is not
/// one of this pool's reserves.
pub fn auction_positions(
    auction: &AuctionData,
    asset_index: &BTreeMap<String, u32>,
) -> Result<Positions, MathError> {
    let index = |asset: &String| {
        asset_index
            .get(asset)
            .copied()
            .ok_or(MathError::InvalidInput(
                "an auction names an asset that is not a reserve of this pool",
            ))
    };
    let mut positions = Positions::default();
    for (asset, amount) in &auction.lot {
        positions.collateral.insert(index(asset)?, *amount);
    }
    for (asset, amount) in &auction.bid {
        positions.liabilities.insert(index(asset)?, *amount);
    }
    Ok(positions)
}

/// The fewest ledgers after an auction's start at which its lot covers its
/// bid plus `profit_bps`: the smallest `d` in `0..=400` with
/// `lot × lot_modifier(d) ≥ bid × bid_modifier(d) × (1 + p)`.
///
/// Spec §5's closed form. When the whole lot covers the bid plus margin,
/// the answer is on the lot ramp, `d = ⌈200 · bid · (1 + p) / lot⌉`;
/// otherwise it is on the bid ramp, `d = 400 − ⌊200 · lot / (bid · (1 +
/// p))⌋`. Both are exact in integers, because the contract's modifiers
/// move in steps of exactly 1/200 — [`meets_margin`] is the check it is
/// proved against. `force_fill` caps the answer at
/// [`FORCE_FILL_MAX_DELAY`].
///
/// # Errors
///
/// `MathError::InvalidInput` for a negative value; `Overflow` only for
/// values no auction holds.
pub fn fill_delay(
    lot_raw: i128,
    bid_raw: i128,
    profit_bps: u32,
    force_fill: bool,
) -> Result<u32, MathError> {
    if lot_raw < 0 || bid_raw < 0 {
        return Err(MathError::InvalidInput(
            "an auction's value is never negative",
        ));
    }
    let ramp = i128::from(RAMP_BLOCKS);
    let margin = BPS
        .checked_add(i128::from(profit_bps))
        .ok_or(MathError::Overflow)?;
    let delay = if bid_raw == 0 {
        0
    } else if lot_raw == 0 {
        RAMP_END_BLOCKS
    // Unchecked in I256: lot_raw, bid_raw <= i128::MAX (~1.7e38) and margin
    // <= BPS + u32::MAX (~4.3e9), so the larger product here is at most
    // ~7.3e47 — far short of I256::MAX (~5.8e76).
    } else if I256::from(lot_raw) * I256::from(BPS) >= I256::from(bid_raw) * I256::from(margin) {
        // Lot ramp: the smallest d with lot · d / 200 ≥ bid · margin / BPS.
        let denominator = lot_raw.checked_mul(BPS).ok_or(MathError::Overflow)?;
        let factor = ramp.checked_mul(margin).ok_or(MathError::Overflow)?;
        let delay = mul_ceil(bid_raw, factor, denominator)?;
        u32::try_from(delay).map_err(|_| MathError::Overflow)?
    } else {
        // Bid ramp: the largest k = 400 − d with bid · k / 200 · margin ≤ lot · BPS.
        let denominator = bid_raw.checked_mul(margin).ok_or(MathError::Overflow)?;
        let factor = ramp.checked_mul(BPS).ok_or(MathError::Overflow)?;
        let covered = mul_floor(lot_raw, factor, denominator)?;
        // covered < 200 on this branch: lot · BPS < bid · margin.
        let covered = u32::try_from(covered).map_err(|_| MathError::Overflow)?;
        RAMP_END_BLOCKS
            .checked_sub(covered)
            .ok_or(MathError::Overflow)?
    };
    Ok(if force_fill {
        delay.min(FORCE_FILL_MAX_DELAY)
    } else {
        delay
    })
}

/// Whether an auction worth `lot_raw` against `bid_raw` meets `profit_bps`
/// when filled `delay` ledgers after its start, by the contract's own
/// modifiers and with no rounding anywhere: both sides are cross-multiplied
/// in 256 bits. What [`fill_delay`]'s closed form is proved against.
#[must_use]
pub fn meets_margin(delay: u32, lot_raw: i128, bid_raw: i128, profit_bps: u32) -> bool {
    let margin = I256::from(BPS) + I256::from(profit_bps);
    // Unchecked in I256: lot_raw, bid_raw <= i128::MAX (~1.7e38), the
    // modifiers <= SCALAR_7 (1e7), and margin <= BPS + u32::MAX (~4.3e9),
    // so the larger product here is at most ~7.3e54 — far short of
    // I256::MAX (~5.8e76).
    I256::from(lot_raw) * I256::from(lot_modifier(delay)) * I256::from(BPS)
        >= I256::from(bid_raw) * I256::from(bid_modifier(delay)) * margin
}

/// The health factor the filler keeps itself at or above after a fill:
/// the pool's `min_health_factor` times `HF_SAFETY_MULTIPLIER`, both 7
/// decimals, rounded up so the floor errs toward safety.
///
/// # Errors
///
/// `Overflow` only for inputs no configuration holds.
pub fn health_floor(min_health_factor: i128, multiplier: i128) -> Result<i128, MathError> {
    mul_ceil(min_health_factor, multiplier, SCALAR_7)
}

/// A 7-decimal configuration value in the pool oracle's own units, rounded
/// down.
///
/// # Errors
///
/// `Overflow` only for inputs no configuration holds.
pub fn to_oracle_units(value: i128, oracle_scalar: i128) -> Result<i128, MathError> {
    mul_floor(value, oracle_scalar, SCALAR_7)
}

/// What the pool and the operator hold one fill to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillTerms {
    /// The pool's `min_collateral`, in oracle units: the least effective
    /// collateral the contract lets a position with liabilities keep.
    pub min_collateral: i128,
    /// The pool's `max_positions`.
    pub max_positions: u32,
    /// Whether `SupplyCollateral` of the primary asset is allowed now: the
    /// pool's status is 3 or below and the primary reserve is enabled.
    pub supply_allowed: bool,
    /// The asset the filler keeps as collateral, and supplies to cover a
    /// shortfall.
    pub primary_asset: String,
    /// [`health_floor`] of the pool's `min_health_factor` and
    /// `HF_SAFETY_MULTIPLIER`, 7 decimals.
    pub health_floor: i128,
    /// The margin [`fill_delay`] waits for, in basis points.
    pub profit_bps: u32,
    /// Fill by [`FORCE_FILL_MAX_DELAY`], and past the auction's end at all.
    pub force_fill: bool,
    /// How many rounds of supply → percent → delay the plan may take.
    ///
    /// Never zero: the rounds run `0..plan_iterations`, so a zero would
    /// project nothing at all and answer `Health` for every auction while
    /// reporting that it had exhausted its rounds — a bot that looks busy
    /// and does nothing. `PLAN_ITERATIONS=0` is refused at parse (see
    /// `Args`), so the filler never builds these terms with it.
    pub plan_iterations: u32,
}

/// The chain state one plan is made against, all read at one ledger.
#[derive(Debug, Clone, Copy)]
pub struct FillInputs<'a> {
    /// The pool's reserves, accrued to the valuation time.
    pub reserves: &'a BTreeMap<u32, Reserve>,
    /// Asset address to reserve index.
    pub asset_index: &'a BTreeMap<String, u32>,
    /// The pool oracle's prices.
    pub prices: &'a OraclePrices,
    /// The filler's own positions in this pool before the fill.
    pub filler: &'a Positions,
    /// What the filler's wallet may spend, per asset: its balance less the
    /// fee reserve and every live reservation.
    pub wallet: &'a BTreeMap<String, i128>,
    /// The auction as the chain holds it now.
    pub auction: &'a AuctionData,
    /// The first ledger a transaction sent now could land in.
    pub earliest_ledger: u32,
    /// The largest percent the plan may name: 100, except on the
    /// executor's one re-plan after the contract refused a health check.
    pub max_percent: FillPercent,
}

/// One request a plan adds after the fill itself, in the order sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillAction {
    /// Repay `amount` of `asset` from the wallet. The contract refunds
    /// whatever exceeds the debt, so the allowance above the scaled bid
    /// costs nothing but must be held.
    Repay {
        /// The bid asset.
        asset: String,
        /// Underlying, in the asset's decimals.
        amount: i128,
    },
    /// Withdraw every b-token of `asset`, a reserve with no collateral
    /// factor: it adds nothing to health and costs a position slot.
    WithdrawAll {
        /// The lot asset.
        asset: String,
    },
    /// Supply `amount` of the primary asset as collateral.
    SupplyCollateral {
        /// The primary asset.
        asset: String,
        /// Underlying, in the asset's decimals.
        amount: i128,
    },
}

/// A fill the filler's own position can carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillDraft {
    /// The ledger to fill at.
    pub fill_ledger: u32,
    /// The percent of the auction to fill.
    pub percent: FillPercent,
    /// The requests after the fill, in the order sent.
    pub actions: Vec<FillAction>,
    /// What the fill hands over at `fill_ledger` and `percent`.
    pub to_fill: AuctionData,
    /// `to_fill.lot`'s raw value, oracle units.
    pub lot_value: i128,
    /// `to_fill.bid`'s raw value, oracle units.
    pub bid_value: i128,
    /// `lot_value − bid_value`.
    pub est_profit: i128,
    /// The wallet amounts `actions` spend, per asset: what a live plan
    /// reserves.
    pub spend: BTreeMap<String, i128>,
    /// The filler's projected health factor, in the oracle's scale; `None`
    /// when the fill leaves it no liabilities and the contract checks
    /// nothing.
    pub projected_health: Option<i128>,
}

/// Why no fill was planned. A closed set: each is a metric label in Phase 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillSkip {
    /// The lot is worth nothing at the oracle's prices.
    Unprofitable,
    /// Past its 400th ledger, in a pool that is not `force_fill`.
    PastAuctionEnd,
    /// The fill would take the filler past the pool's `max_positions`.
    TooManyPositions,
    /// More of the primary asset would have closed the shortfall, and the
    /// wallet does not hold it.
    Unfunded,
    /// Nothing within `plan_iterations` holds the filler's floor.
    Health,
}

/// What [`plan_fill`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedFill {
    /// Fill as drafted.
    Fill(FillDraft),
    /// Not this auction, for this reason.
    Skip(FillSkip),
}

/// The allowance a repay adds above the scaled bid, as a fraction of it:
/// one basis point, which covers a few hours of interest at any rate the
/// pool allows. The contract refunds the excess.
const REPAY_ALLOWANCE_BPS: i128 = 1;

/// Units added to a supply for the b-token round trip's two floors; the
/// next projection verifies the result either way.
const SUPPLY_ROUNDING_ALLOWANCE: i128 = 2;

/// Plans one fill (spec §5, "Health-bounded plan").
///
/// 1. Value the whole auction. A lot worth nothing is `Unprofitable`. An
///    auction whose earliest ledger is more than 400 past its start is
///    `PastAuctionEnd` unless the pool is `force_fill` (spec §1).
/// 2. The first candidate is `start + fill_delay(...)`, moved to the
///    earliest ledger if it has already passed, at `max_percent`. The
///    latest a candidate may be is `start + 350` under `force_fill`, else
///    `start + 400` — or the earliest ledger, when that is already later.
/// 3. Each round (at most `plan_iterations`) projects the candidate
///    exactly: the fill's scaled lot and bid added to the filler's
///    positions; `Repay` of each bid asset the wallet holds, the scaled
///    d-tokens in underlying plus a 1 bp allowance, capped at the balance;
///    `WithdrawAll` of each lot asset whose collateral factor is zero;
///    `SupplyCollateral` of the primary asset sized so far, capped at what
///    the wallet holds after the repays. A projection that raises the
///    position count past `max_positions` is `TooManyPositions`. One with
///    no liabilities, or at or above the floor with at least
///    `min_collateral`, is the plan.
/// 4. Short, in the spec's order: supply more of the primary for the
///    shortfall, when the pool permits it (remember when the wallet capped
///    it); else the largest lower percent that projects healthy; else the
///    first later ledger at which `max_percent` projects healthy (ruling
///    10). Candidates are searched exactly (ruling 11), and a candidate
///    the search finds is the plan: it is drafted from the very projection
///    it was found with, never re-projected by a later round that
///    `plan_iterations` may not reach.
/// 5. Out of rounds or candidates: `Unfunded` if the wallet capped a
///    supply along the way, else `Health`.
///
/// Whatever a round or a search produces is still refused as
/// `Unprofitable` when its own lot no longer covers its own bid: step 1
/// judges the margin on the whole auction, and a lowered percent or a
/// rounding step can take the fill itself under it. Only `force_fill`
/// accepts a loss.
///
/// The request order a plan produces — fill, repays, withdrawals, supply —
/// is the order the executor sends them in; the contract checks health
/// once, after all of them, so order changes nothing but readability.
///
/// # Errors
///
/// `MathError` for an auction naming an asset the pool does not list, a
/// reserve or price missing, or arithmetic no real auction reaches.
pub fn plan_fill(terms: &FillTerms, inputs: &FillInputs<'_>) -> Result<PlannedFill, MathError> {
    let whole = calculate_position_data(
        inputs.reserves,
        inputs.prices,
        &auction_positions(inputs.auction, inputs.asset_index)?,
    )?;
    if whole.collateral_raw == 0 {
        return Ok(PlannedFill::Skip(FillSkip::Unprofitable));
    }
    let start = inputs.auction.block;
    let earliest = inputs.earliest_ledger.max(start);
    // `earliest >= start` by the line above.
    if earliest - start > RAMP_END_BLOCKS && !terms.force_fill {
        return Ok(PlannedFill::Skip(FillSkip::PastAuctionEnd));
    }
    let delay = fill_delay(
        whole.collateral_raw,
        whole.liability_raw,
        terms.profit_bps,
        terms.force_fill,
    )?;
    let ledger = start
        .checked_add(delay)
        .ok_or(MathError::Overflow)?
        .max(earliest);
    let reach = if terms.force_fill {
        FORCE_FILL_MAX_DELAY
    } else {
        RAMP_END_BLOCKS
    };
    let last = start
        .checked_add(reach)
        .ok_or(MathError::Overflow)?
        .max(ledger);
    let percent = inputs.max_percent;
    let mut supply = 0_i128;
    let mut unfunded = false;

    for _ in 0..terms.plan_iterations {
        let projection = project(terms, inputs, ledger, percent, supply)?;
        if over_positions(terms, inputs, &projection) {
            return Ok(PlannedFill::Skip(FillSkip::TooManyPositions));
        }
        if healthy(terms, &projection)? {
            return settle(terms, inputs, ledger, percent, projection);
        }
        if terms.supply_allowed {
            let wanted = supply
                .checked_add(supply_for(terms, inputs, &projection.data)?)
                .ok_or(MathError::Overflow)?;
            let next = wanted.min(projection.primary_available);
            unfunded |= wanted > projection.primary_available;
            if next > supply {
                supply = next;
                continue;
            }
        }
        if let Some((lower, projection)) =
            largest_healthy_percent(terms, inputs, ledger, percent, supply)?
        {
            return settle(terms, inputs, ledger, lower, projection);
        }
        if let Some((later, projection)) =
            first_healthy_ledger(terms, inputs, ledger, last, supply)?
        {
            return settle(terms, inputs, later, inputs.max_percent, projection);
        }
        break;
    }
    Ok(PlannedFill::Skip(if unfunded {
        FillSkip::Unfunded
    } else {
        FillSkip::Health
    }))
}

/// One candidate fill, projected exactly by [`project`].
struct Projection {
    /// What the fill hands over at the candidate's ledger and percent.
    to_fill: AuctionData,
    /// The filler's positions after the fill and every request after it.
    positions: Positions,
    /// The requests after the fill, in the order sent.
    actions: Vec<FillAction>,
    /// The wallet amounts `actions` spend, per asset.
    spend: BTreeMap<String, i128>,
    /// The primary asset the wallet holds after the repays and before the
    /// supply, never negative: what caps the supply.
    primary_available: i128,
    /// `positions`, valued.
    data: PositionData,
}

/// The reserve index and reserve of `asset`. An asset the pool does not
/// list is `InvalidInput` and an index with no reserve is `MissingReserve`:
/// both mean the caller's view of the pool is incomplete, and neither is
/// ever read as a zero.
fn reserve_for<'a>(inputs: &FillInputs<'a>, asset: &str) -> Result<(u32, &'a Reserve), MathError> {
    let index = inputs
        .asset_index
        .get(asset)
        .copied()
        .ok_or(MathError::InvalidInput(
            "a fill names an asset that is not a reserve of this pool",
        ))?;
    let reserve = inputs
        .reserves
        .get(&index)
        .ok_or(MathError::MissingReserve(index))?;
    Ok((index, reserve))
}

/// Adds `amount` to `key`'s entry, checked. A zero amount changes nothing
/// and creates no entry, so it never counts against `max_positions`.
fn add_to<K: Ord>(map: &mut BTreeMap<K, i128>, key: K, amount: i128) -> Result<(), MathError> {
    if amount == 0 {
        return Ok(());
    }
    let entry = map.entry(key).or_insert(0);
    *entry = entry.checked_add(amount).ok_or(MathError::Overflow)?;
    Ok(())
}

/// Projects a fill of `percent` at `ledger` exactly, with the requests
/// after it in the order the executor sends them:
///
/// 1. the fill's scaled lot added to the filler's collateral and its
///    scaled bid to its liabilities, by reserve index;
/// 2. a `Repay` of each bid asset the wallet holds a positive balance of:
///    the scaled d-tokens in underlying, plus `REPAY_ALLOWANCE_BPS` of it
///    and one unit, capped at the balance — burning at most what the
///    filler then owes in that reserve, as the contract's repay does;
/// 3. a `WithdrawAll` of each lot asset whose reserve has no collateral
///    factor;
/// 4. a `SupplyCollateral` of `supply` of the primary asset, clamped to
///    what the wallet holds after the repays, and only when that mints at
///    least one b-token;
///
/// and the result valued by `calculate_position_data`. Every amount
/// `spend` records is first debited from a copy of the wallet that no
/// debit takes below zero, so a projection never spends more of an asset
/// than the wallet holds.
fn project(
    terms: &FillTerms,
    inputs: &FillInputs<'_>,
    ledger: u32,
    percent: FillPercent,
    supply: i128,
) -> Result<Projection, MathError> {
    let to_fill = scale_auction(inputs.auction, ledger, percent.get())?.to_fill;
    let mut positions = inputs.filler.clone();
    for (asset, b_tokens) in &to_fill.lot {
        let (index, _) = reserve_for(inputs, asset)?;
        add_to(&mut positions.collateral, index, *b_tokens)?;
    }
    for (asset, d_tokens) in &to_fill.bid {
        let (index, _) = reserve_for(inputs, asset)?;
        add_to(&mut positions.liabilities, index, *d_tokens)?;
    }

    let mut wallet = inputs.wallet.clone();
    let mut actions = Vec::new();
    let mut spend = BTreeMap::new();
    for (asset, d_tokens) in &to_fill.bid {
        let held = wallet.get(asset).copied().unwrap_or(0);
        if held <= 0 {
            continue;
        }
        let (index, reserve) = reserve_for(inputs, asset)?;
        let owed = reserve.to_asset_from_d_token(*d_tokens)?;
        let amount = owed
            .checked_add(mul_floor(owed, REPAY_ALLOWANCE_BPS, BPS)?)
            .and_then(|amount| amount.checked_add(1))
            .ok_or(MathError::Overflow)?
            .min(held);
        let owing = positions.liabilities.get(&index).copied().unwrap_or(0);
        let burnt = reserve.to_d_token_down(amount)?.min(owing);
        let left = owing.checked_sub(burnt).ok_or(MathError::Overflow)?;
        if left > 0 {
            positions.liabilities.insert(index, left);
        } else {
            positions.liabilities.remove(&index);
        }
        let rest = held.checked_sub(amount).ok_or(MathError::Overflow)?;
        wallet.insert(asset.clone(), rest);
        add_to(&mut spend, asset.clone(), amount)?;
        actions.push(FillAction::Repay {
            asset: asset.clone(),
            amount,
        });
    }

    for asset in to_fill.lot.keys() {
        let (index, reserve) = reserve_for(inputs, asset)?;
        if reserve.config.c_factor == 0 {
            positions.collateral.remove(&index);
            actions.push(FillAction::WithdrawAll {
                asset: asset.clone(),
            });
        }
    }

    let primary_available = wallet
        .get(&terms.primary_asset)
        .copied()
        .unwrap_or(0)
        .max(0);
    let supply = supply.min(primary_available);
    if supply > 0 {
        let (index, reserve) = reserve_for(inputs, &terms.primary_asset)?;
        let b_tokens = reserve.to_b_token_down(supply)?;
        // A supply too small to mint a b-token is no supply at all: the
        // contract's `add_collateral` refuses a zero mint
        // (`InvalidBTokenMintAmount`), so sending it would cost the whole
        // fill for as long as the dust sat in the wallet.
        if b_tokens > 0 {
            add_to(&mut positions.collateral, index, b_tokens)?;
            add_to(&mut spend, terms.primary_asset.clone(), supply)?;
            actions.push(FillAction::SupplyCollateral {
                asset: terms.primary_asset.clone(),
                amount: supply,
            });
        }
    }

    let data = calculate_position_data(inputs.reserves, inputs.prices, &positions)?;
    Ok(Projection {
        to_fill,
        positions,
        actions,
        spend,
        primary_available,
        data,
    })
}

/// Whether a projection is one the contract accepts and the operator
/// wants: no liabilities left, when the contract checks nothing; or a
/// health factor at or above the floor with at least `min_collateral` of
/// effective collateral — the contract's `InvalidHf` and
/// `MinCollateralNotMet` checks, with the floor in place of the pool's
/// minimum.
fn healthy(terms: &FillTerms, projection: &Projection) -> Result<bool, MathError> {
    if projection.positions.liabilities.is_empty() {
        return Ok(true);
    }
    Ok(!projection.data.is_hf_under(terms.health_floor)?
        && projection.data.collateral_base >= terms.min_collateral)
}

/// Whether a projection breaks the pool's `max_positions` as the
/// contract's `require_under_max` does: only a count the fill raises, and
/// raises past the cap, is refused. A count that does not fit `u32` is
/// over.
fn over_positions(terms: &FillTerms, inputs: &FillInputs<'_>, projection: &Projection) -> bool {
    let before = inputs.filler.effective_count();
    let after = projection.positions.effective_count();
    after > before && u32::try_from(after).map_or(true, |after| after > terms.max_positions)
}

/// The primary asset, in its underlying, that would close a projection's
/// shortfall: the effective collateral the floor and `min_collateral` ask
/// for beyond what the projection has, divided back through the primary's
/// price and collateral factor — both rounded up — plus
/// `SUPPLY_ROUNDING_ALLOWANCE`. An estimate the next projection verifies.
/// Zero when nothing is short, and when the primary reserve has no
/// collateral factor, since no supply of it can help.
fn supply_for(
    terms: &FillTerms,
    inputs: &FillInputs<'_>,
    data: &PositionData,
) -> Result<i128, MathError> {
    let wanted =
        mul_ceil(data.liability_base, terms.health_floor, SCALAR_7)?.max(terms.min_collateral);
    let shortfall = wanted
        .checked_sub(data.collateral_base)
        .ok_or(MathError::Overflow)?
        .max(0);
    if shortfall == 0 {
        return Ok(0);
    }
    let (_, reserve) = reserve_for(inputs, &terms.primary_asset)?;
    let c_factor = i128::from(reserve.config.c_factor);
    if c_factor == 0 {
        return Ok(0);
    }
    let price = inputs.prices.price(&reserve.asset)?;
    div_ceil(
        mul_ceil(shortfall, reserve.scalar, price)?,
        c_factor,
        SCALAR_7,
    )?
    .checked_add(SUPPLY_ROUNDING_ALLOWANCE)
    .ok_or(MathError::Overflow)
}

/// The largest percent below `below` whose exact projection at `ledger`
/// with `supply` is [`healthy`], searched from `below − 1` down to 1
/// (ruling 11); `None` when none is.
///
/// The projection it was found with travels back with it, so the caller
/// drafts the candidate it proved rather than projecting the same triple
/// again in a round `plan_iterations` may not have left.
fn largest_healthy_percent(
    terms: &FillTerms,
    inputs: &FillInputs<'_>,
    ledger: u32,
    below: FillPercent,
    supply: i128,
) -> Result<Option<(FillPercent, Projection)>, MathError> {
    for value in (1..below.get()).rev() {
        let percent = FillPercent::try_from(value)
            .map_err(|_| MathError::InvalidInput("a fill percent is 1 to 100"))?;
        let projection = project(terms, inputs, ledger, percent, supply)?;
        if healthy(terms, &projection)? {
            return Ok(Some((percent, projection)));
        }
    }
    Ok(None)
}

/// The first ledger after `after`, up to and including `last`, whose exact
/// projection at `inputs.max_percent` with `supply` is [`healthy`]
/// (rulings 10 and 11); `None` when none is, and when `after` is the last
/// ledger a `u32` holds.
///
/// The projection it was found with travels back with it, for
/// [`largest_healthy_percent`]'s reason.
fn first_healthy_ledger(
    terms: &FillTerms,
    inputs: &FillInputs<'_>,
    after: u32,
    last: u32,
    supply: i128,
) -> Result<Option<(u32, Projection)>, MathError> {
    let Some(first) = after.checked_add(1) else {
        return Ok(None);
    };
    for ledger in first..=last {
        let projection = project(terms, inputs, ledger, inputs.max_percent, supply)?;
        if healthy(terms, &projection)? {
            return Ok(Some((ledger, projection)));
        }
    }
    Ok(None)
}

/// What a healthy projection answers with: the draft it becomes, or the
/// skip it turns out to be.
///
/// Every plan leaves through here, wherever its projection came from — a
/// round or one of the two searches — so both of the refusals that are
/// judged on the finished fill are judged on all of them: a fill that
/// raises the filler's position count past `max_positions`, and one whose
/// own lot no longer covers its own bid, which only a `force_fill` pool
/// accepts.
fn settle(
    terms: &FillTerms,
    inputs: &FillInputs<'_>,
    ledger: u32,
    percent: FillPercent,
    projection: Projection,
) -> Result<PlannedFill, MathError> {
    if over_positions(terms, inputs, &projection) {
        return Ok(PlannedFill::Skip(FillSkip::TooManyPositions));
    }
    let draft = draft(inputs, ledger, percent, projection)?;
    if !terms.force_fill && draft.est_profit <= 0 {
        return Ok(PlannedFill::Skip(FillSkip::Unprofitable));
    }
    Ok(PlannedFill::Fill(draft))
}

/// The draft a healthy projection becomes: its requests and spend, the
/// fill valued raw — what the filler is paid and pays — for the estimated
/// profit, and the projected health factor, `None` when the fill leaves no
/// liabilities.
fn draft(
    inputs: &FillInputs<'_>,
    ledger: u32,
    percent: FillPercent,
    projection: Projection,
) -> Result<FillDraft, MathError> {
    let value = calculate_position_data(
        inputs.reserves,
        inputs.prices,
        &auction_positions(&projection.to_fill, inputs.asset_index)?,
    )?;
    let est_profit = value
        .collateral_raw
        .checked_sub(value.liability_raw)
        .ok_or(MathError::Overflow)?;
    let projected_health = if projection.positions.liabilities.is_empty() {
        None
    } else {
        projection.data.health_factor()?
    };
    Ok(FillDraft {
        fill_ledger: ledger,
        percent,
        actions: projection.actions,
        to_fill: projection.to_fill,
        lot_value: value.collateral_raw,
        bid_value: value.liability_raw,
        est_profit,
        spend: projection.spend,
        projected_health,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The closed form is the smallest delay that meets the margin: it
    /// meets it, and the ledger before does not. `meets_margin` is
    /// monotonic in the delay — the lot's modifier never falls and the
    /// bid's never rises — so checking `d − 1` is enough.
    #[test]
    fn the_fill_delay_is_the_first_ledger_that_meets_the_margin() {
        let values = [1_i128, 7, 999, 1_000, 1_001, 1_000_000_007, 10_i128.pow(20)];
        for lot in values {
            for bid in values {
                for profit_bps in [0_u32, 1, 500, 1_000, 10_000, 20_000] {
                    let delay = fill_delay(lot, bid, profit_bps, false).unwrap();
                    assert!(delay <= 400, "{lot} {bid} {profit_bps}: {delay}");
                    assert!(
                        meets_margin(delay, lot, bid, profit_bps),
                        "{lot} {bid} {profit_bps}: {delay} does not meet it"
                    );
                    if delay > 0 {
                        assert!(
                            !meets_margin(delay - 1, lot, bid, profit_bps),
                            "{lot} {bid} {profit_bps}: {} already met it",
                            delay - 1
                        );
                    }
                }
            }
        }
    }

    /// On the lot ramp: $1,200 of lot against $1,000 of bid at 10% needs
    /// the lot at $1,100, which the ramp passes at ⌈200 × 1100 / 1200⌉ =
    /// 184 ledgers ($1,104); at 183 it is $1,098.
    #[test]
    fn a_lot_worth_more_than_the_margin_is_filled_on_the_lot_ramp() {
        assert_eq!(fill_delay(1_200, 1_000, 1_000, false).unwrap(), 184);
    }

    /// On the bid ramp: $900 of lot against $1,000 of bid at 10% needs the
    /// bid down to $818.18; 400 − ⌊200 × 900 / 1100⌋ = 400 − 163 = 237,
    /// where the bid is $815 and $815 × 1.1 = $896.50 ≤ $900. At 236 the
    /// bid is $820, and $902 is too much.
    #[test]
    fn a_lot_short_of_the_margin_waits_for_the_bid_ramp() {
        assert_eq!(fill_delay(900, 1_000, 1_000, false).unwrap(), 237);
    }

    /// Exactly the margin is met at 200: the whole lot against the whole
    /// bid.
    #[test]
    fn a_lot_exactly_at_the_margin_is_filled_at_200() {
        assert_eq!(fill_delay(1_100, 1_000, 1_000, false).unwrap(), 200);
    }

    /// The edges: nothing to pay is filled at once; nothing to receive is
    /// only "covered" once the bid has fallen to zero.
    #[test]
    fn a_zero_side_is_an_edge_not_an_error() {
        assert_eq!(fill_delay(1_000, 0, 1_000, false).unwrap(), 0);
        assert_eq!(fill_delay(0, 1_000, 1_000, false).unwrap(), 400);
    }

    /// `force_fill` never waits past 350, however little the lot covers.
    #[test]
    fn force_fill_caps_the_delay_at_350() {
        // ⌊200 × 100 / 1100⌋ = 18, so 382 without the cap.
        assert_eq!(fill_delay(100, 1_000, 1_000, false).unwrap(), 382);
        assert_eq!(fill_delay(100, 1_000, 1_000, true).unwrap(), 350);
        assert_eq!(
            fill_delay(1_200, 1_000, 1_000, true).unwrap(),
            184,
            "under the cap it changes nothing"
        );
    }

    /// A negative value is a bug upstream, not an auction.
    #[test]
    fn a_negative_value_is_refused() {
        assert!(fill_delay(-1, 1_000, 0, false).is_err());
        assert!(fill_delay(1_000, -1, 0, false).is_err());
    }

    /// The lot is collateral and the bid is liabilities, each by reserve
    /// index — the shape `calculate_position_data` values.
    #[test]
    fn an_auction_is_valued_as_a_position() {
        let auction = AuctionData {
            lot: BTreeMap::from([("L".to_string(), 5)]),
            bid: BTreeMap::from([("B".to_string(), 9)]),
            block: 1,
        };
        let index = BTreeMap::from([("L".to_string(), 3), ("B".to_string(), 0)]);
        let positions = auction_positions(&auction, &index).unwrap();
        assert_eq!(positions.collateral, BTreeMap::from([(3, 5)]));
        assert_eq!(positions.liabilities, BTreeMap::from([(0, 9)]));
        assert!(positions.supply.is_empty());
        let unknown = BTreeMap::from([("L".to_string(), 3)]);
        assert!(
            auction_positions(&auction, &unknown).is_err(),
            "an asset the pool does not list"
        );
    }

    /// 1.5 × 1.1 = 1.65, rounded up — the floor errs toward safety.
    #[test]
    fn the_health_floor_is_the_pools_minimum_times_the_multiplier() {
        assert_eq!(health_floor(15_000_000, 11_000_000).unwrap(), 16_500_000);
        assert_eq!(
            health_floor(10_000_001, 11_000_000).unwrap(),
            11_000_002,
            "⌈11_000_001.1⌉"
        );
    }

    /// A 7-decimal config value in a 6-decimal oracle's units.
    #[test]
    fn a_config_value_is_rescaled_to_the_oracle() {
        assert_eq!(
            to_oracle_units(100_000_000, 10_000_000).unwrap(),
            100_000_000
        );
        assert_eq!(to_oracle_units(100_000_000, 1_000_000).unwrap(), 10_000_000);
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use crate::math::{ReserveConfig, ReserveData, SCALAR_12};

    const XLM: &str = "XLM";
    const USDC: &str = "USDC";
    const NO_CF: &str = "NOCF";
    const START: u32 = 1_000;

    fn reserve(asset: &str, index: u32, c_factor: u32, l_factor: u32) -> Reserve {
        Reserve::new(
            asset.to_string(),
            ReserveConfig {
                index,
                decimals: 7,
                c_factor,
                l_factor,
                util: 0,
                max_util: 9_500_000,
                r_base: 0,
                r_one: 0,
                r_two: 0,
                r_three: 0,
                reactivity: 0,
                supply_cap: i128::MAX,
                enabled: true,
            },
            ReserveData {
                d_rate: SCALAR_12,
                b_rate: SCALAR_12,
                ir_mod: SCALAR_7,
                b_supply: 0,
                d_supply: 0,
                backstop_credit: 0,
                last_time: 0,
            },
        )
        .expect("a test reserve")
    }

    struct Pool {
        reserves: BTreeMap<u32, Reserve>,
        asset_index: BTreeMap<String, u32>,
        prices: OraclePrices,
    }

    /// XLM at $0.10 with factors 0.75; USDC at $1 with factors 0.95; and a
    /// $1 reserve with no collateral factor at all.
    fn pool() -> Pool {
        Pool {
            reserves: BTreeMap::from([
                (0, reserve(XLM, 0, 7_500_000, 7_500_000)),
                (1, reserve(USDC, 1, 9_500_000, 9_500_000)),
                (2, reserve(NO_CF, 2, 0, 10_000_000)),
            ]),
            asset_index: BTreeMap::from([
                (XLM.to_string(), 0),
                (USDC.to_string(), 1),
                (NO_CF.to_string(), 2),
            ]),
            prices: OraclePrices::new(
                7,
                BTreeMap::from([
                    (XLM.to_string(), 1_000_000),
                    (USDC.to_string(), 10_000_000),
                    (NO_CF.to_string(), 10_000_000),
                ]),
            )
            .expect("prices"),
        }
    }

    /// Floor 1.1, 10% margin, XLM as the primary asset, $100 of minimum
    /// collateral.
    fn terms() -> FillTerms {
        FillTerms {
            min_collateral: 1_000_000_000,
            max_positions: 6,
            supply_allowed: true,
            primary_asset: XLM.to_string(),
            health_floor: 11_000_000,
            profit_bps: 1_000,
            force_fill: false,
            plan_iterations: 5,
        }
    }

    /// 20,000 XLM of lot — $2,000 raw, $1,500 effective — against 1,000
    /// USDC of bid — $1,000 raw, $1,052.63 effective (⌈1e10 / 0.95⌉ =
    /// 10_526_315_790). At 10% the lot covers the bid on the lot ramp at
    /// ⌈200 × 1100 / 2000⌉ = 110 ledgers.
    fn auction() -> AuctionData {
        AuctionData {
            lot: BTreeMap::from([(XLM.to_string(), 200_000_000_000)]),
            bid: BTreeMap::from([(USDC.to_string(), 10_000_000_000)]),
            block: START,
        }
    }

    fn percent(value: u32) -> FillPercent {
        FillPercent::try_from(value).expect("1..=100")
    }

    fn plan(
        terms: &FillTerms,
        pool: &Pool,
        filler: &Positions,
        wallet: &BTreeMap<String, i128>,
        auction: &AuctionData,
        earliest_ledger: u32,
        max_percent: FillPercent,
    ) -> PlannedFill {
        plan_fill(
            terms,
            &FillInputs {
                reserves: &pool.reserves,
                asset_index: &pool.asset_index,
                prices: &pool.prices,
                filler,
                wallet,
                auction,
                earliest_ledger,
                max_percent,
            },
        )
        .expect("no arithmetic failure")
    }

    fn draft(planned: PlannedFill) -> FillDraft {
        match planned {
            PlannedFill::Fill(draft) => draft,
            PlannedFill::Skip(skip) => panic!("expected a fill, got {skip:?}"),
        }
    }

    /// A filler holding $10,000 of USDC collateral ($9,500 effective) and
    /// nothing else: headroom for anything these tests auction.
    fn well_collateralised() -> Positions {
        Positions {
            collateral: BTreeMap::from([(1, 100_000_000_000)]),
            ..Positions::default()
        }
    }

    /// At a 200% margin the fill waits for the bid ramp: 400 − ⌊200 ×
    /// 2000 / 3000⌋ = 267. The bid there is 1e10 × 0.665 = 6.65e9 d-tokens,
    /// $665 raw and 7e9 effective (6.65e9 / 0.95, exactly), against $1,500
    /// effective lot: a health factor of ⌊1.5e10 × 1e7 / 7e9⌋ = 21_428_571
    /// with nothing from the wallet at all.
    #[test]
    fn a_fill_the_lot_carries_alone_needs_nothing_from_the_wallet() {
        let pool = pool();
        let terms = FillTerms {
            profit_bps: 20_000,
            ..terms()
        };
        let draft = draft(plan(
            &terms,
            &pool,
            &Positions::default(),
            &BTreeMap::new(),
            &auction(),
            START + 1,
            percent(100),
        ));
        assert_eq!(draft.fill_ledger, START + 267);
        assert_eq!(draft.percent, percent(100));
        assert!(draft.actions.is_empty() && draft.spend.is_empty());
        assert_eq!(
            draft.to_fill.lot,
            BTreeMap::from([(XLM.to_string(), 200_000_000_000)])
        );
        assert_eq!(
            draft.to_fill.bid,
            BTreeMap::from([(USDC.to_string(), 6_650_000_000)])
        );
        assert_eq!(
            (draft.lot_value, draft.bid_value),
            (20_000_000_000, 6_650_000_000)
        );
        assert_eq!(draft.est_profit, 13_350_000_000);
        assert_eq!(draft.projected_health, Some(21_428_571));
    }

    /// At 110 ledgers the fill hands over 1.1e11 XLM ($825 effective)
    /// against the whole bid ($1,052.63 effective): 0.78, under the 1.1
    /// floor. The wallet's XLM closes it by supplying the primary asset:
    /// the floor wants ⌈1.1 × 10_526_315_790⌉ = 11_578_947_369 of
    /// effective collateral, 3_328_947_369 more than the lot's 8.25e9, so
    /// the supply is ⌈3_328_947_369 × 1e7 / 1e6⌉ × 1e7 / 7_500_000 =
    /// 44_385_964_920 stroops plus the two-unit rounding allowance. That
    /// lands the projection exactly on the floor: ⌊(1.1e11 +
    /// 44_385_964_922) × 0.75⌋ × 0.1 = 11_578_947_369 against the same
    /// 10_526_315_790, a health factor of 1.1 to the stroop.
    #[test]
    fn the_primary_asset_is_supplied_to_close_a_shortfall() {
        let pool = pool();
        let wallet = BTreeMap::from([(XLM.to_string(), 1_000_000_000_000)]);
        let draft = draft(plan(
            &terms(),
            &pool,
            &Positions::default(),
            &wallet,
            &auction(),
            START + 1,
            percent(100),
        ));
        assert_eq!(draft.fill_ledger, START + 110);
        assert_eq!(draft.percent, percent(100));
        let [FillAction::SupplyCollateral { asset, amount }] = draft.actions.as_slice() else {
            panic!("expected one supply, got {:?}", draft.actions);
        };
        assert_eq!(asset.as_str(), XLM);
        assert_eq!(*amount, 44_385_964_922);
        assert_eq!(draft.spend, BTreeMap::from([(XLM.to_string(), *amount)]));
        assert_eq!(draft.projected_health, Some(11_000_000));
    }

    /// 1,000 XLM ($75 effective) cannot close the gap at 100%, so the whole
    /// of it is supplied and the percent comes down. With C(P) the
    /// collateral and L(P) the liabilities at P percent: C(22) = (2.42e10 +
    /// 1e10) × 0.75 / 10 = 2.565e9 and 1.1 × L(22) = 1.1 × ⌈2.2e9 / 0.95⌉
    /// = 2_547_368_422, so 22 holds; C(23) = 2.6475e9 against 2_663_157_896
    /// does not.
    #[test]
    fn a_wallet_short_of_the_primary_lowers_the_percent() {
        let pool = pool();
        let wallet = BTreeMap::from([(XLM.to_string(), 10_000_000_000)]);
        let draft = draft(plan(
            &terms(),
            &pool,
            &Positions::default(),
            &wallet,
            &auction(),
            START + 1,
            percent(100),
        ));
        assert_eq!(draft.fill_ledger, START + 110);
        assert_eq!(draft.percent, percent(22));
        assert_eq!(
            draft.actions,
            vec![FillAction::SupplyCollateral {
                asset: XLM.to_string(),
                amount: 10_000_000_000
            }]
        );
    }

    /// No wallet and no position: no supply, and no percent helps — the
    /// fill's own ratio is the filler's whole ratio at any size. So it
    /// waits until the lot ramp has the lot at 1.1 × 10_526_315_790 =
    /// 11_578_947_369 effective: at 155 ledgers it is 1.1625e10, at 154
    /// 1.155e10.
    #[test]
    fn with_no_inventory_and_no_headroom_the_fill_waits() {
        let pool = pool();
        let draft = draft(plan(
            &terms(),
            &pool,
            &Positions::default(),
            &BTreeMap::new(),
            &auction(),
            START + 1,
            percent(100),
        ));
        assert_eq!(draft.fill_ledger, START + 155);
        assert_eq!(draft.percent, percent(100));
        assert!(draft.actions.is_empty());
    }

    /// A pool that does not permit supplying falls through to the same
    /// wait, whatever the wallet holds.
    #[test]
    fn nothing_is_supplied_where_the_pool_forbids_it() {
        let pool = pool();
        let terms = FillTerms {
            supply_allowed: false,
            ..terms()
        };
        let wallet = BTreeMap::from([(XLM.to_string(), 1_000_000_000_000)]);
        let draft = draft(plan(
            &terms,
            &pool,
            &Positions::default(),
            &wallet,
            &auction(),
            START + 1,
            percent(100),
        ));
        assert_eq!(draft.fill_ledger, START + 155);
        assert!(draft.actions.is_empty());
    }

    /// USDC in the wallet repays the bid it names: the scaled d-tokens in
    /// underlying (1e10) plus a 1 bp allowance (1e6 + 1), and the contract
    /// refunds what is not owed. Repaid in full, the fill leaves no
    /// liabilities, so the contract checks nothing.
    #[test]
    fn a_bid_asset_in_the_wallet_is_repaid() {
        let pool = pool();
        let wallet = BTreeMap::from([(USDC.to_string(), 20_000_000_000)]);
        let draft = draft(plan(
            &terms(),
            &pool,
            &Positions::default(),
            &wallet,
            &auction(),
            START + 1,
            percent(100),
        ));
        assert_eq!(draft.fill_ledger, START + 110);
        assert_eq!(
            draft.actions,
            vec![FillAction::Repay {
                asset: USDC.to_string(),
                amount: 10_001_000_001
            }]
        );
        assert_eq!(
            draft.spend,
            BTreeMap::from([(USDC.to_string(), 10_001_000_001)])
        );
        assert_eq!(draft.projected_health, None);
        assert_eq!(draft.est_profit, 11_000_000_000 - 10_000_000_000);
    }

    /// A repay is capped at the balance; what it leaves (6e9 d-tokens,
    /// 6_315_789_474 effective, needing 6_947_368_422) the $825 of lot
    /// covers: ⌊8.25e9 × 1e7 / 6_315_789_474⌋ = 13_062_499, a health
    /// factor of 1.306.
    #[test]
    fn a_repay_is_capped_at_what_the_wallet_holds() {
        let pool = pool();
        let wallet = BTreeMap::from([(USDC.to_string(), 4_000_000_000)]);
        let draft = draft(plan(
            &terms(),
            &pool,
            &Positions::default(),
            &wallet,
            &auction(),
            START + 1,
            percent(100),
        ));
        assert_eq!(
            draft.actions,
            vec![FillAction::Repay {
                asset: USDC.to_string(),
                amount: 4_000_000_000
            }]
        );
        assert_eq!(draft.projected_health, Some(13_062_499));
    }

    /// Lot in a reserve with no collateral factor adds nothing to health
    /// and costs a position slot, so it is withdrawn in the same call.
    #[test]
    fn a_zero_collateral_factor_lot_is_withdrawn() {
        let pool = pool();
        let auction = AuctionData {
            lot: BTreeMap::from([
                (XLM.to_string(), 200_000_000_000),
                (NO_CF.to_string(), 10_000_000_000),
            ]),
            ..auction()
        };
        let draft = draft(plan(
            &terms(),
            &pool,
            &well_collateralised(),
            &BTreeMap::new(),
            &auction,
            START + 1,
            percent(100),
        ));
        assert_eq!(
            draft.actions,
            vec![FillAction::WithdrawAll {
                asset: NO_CF.to_string()
            }]
        );
    }

    /// Spec §5's position cap: one position before, three after, and a cap
    /// of two.
    #[test]
    fn a_fill_past_the_pools_position_cap_is_skipped() {
        let pool = pool();
        let terms = FillTerms {
            max_positions: 2,
            ..terms()
        };
        let planned = plan(
            &terms,
            &pool,
            &well_collateralised(),
            &BTreeMap::new(),
            &auction(),
            START + 1,
            percent(100),
        );
        assert_eq!(planned, PlannedFill::Skip(FillSkip::TooManyPositions));
    }

    /// Spec §1: past its 400th ledger an auction is filled only under
    /// `force_fill` — and then at once, with the bid at zero.
    #[test]
    fn past_its_end_only_a_force_fill_pool_fills() {
        let pool = pool();
        let late = START + 401;
        let planned = plan(
            &terms(),
            &pool,
            &well_collateralised(),
            &BTreeMap::new(),
            &auction(),
            late,
            percent(100),
        );
        assert_eq!(planned, PlannedFill::Skip(FillSkip::PastAuctionEnd));
        let forced = FillTerms {
            force_fill: true,
            ..terms()
        };
        let draft = draft(plan(
            &forced,
            &pool,
            &well_collateralised(),
            &BTreeMap::new(),
            &auction(),
            late,
            percent(100),
        ));
        assert_eq!(draft.fill_ledger, late);
        assert!(
            draft.to_fill.bid.is_empty(),
            "the bid has ramped to nothing"
        );
    }

    /// $100 of lot against $1,000 of bid would wait 382 ledgers for its
    /// margin; `force_fill` fills at 350.
    #[test]
    fn force_fill_never_waits_past_350() {
        let pool = pool();
        let auction = AuctionData {
            lot: BTreeMap::from([(XLM.to_string(), 10_000_000_000)]),
            ..auction()
        };
        let patient = draft(plan(
            &terms(),
            &pool,
            &well_collateralised(),
            &BTreeMap::new(),
            &auction,
            START + 1,
            percent(100),
        ));
        assert_eq!(patient.fill_ledger, START + 382);
        let forced = FillTerms {
            force_fill: true,
            ..terms()
        };
        let draft = draft(plan(
            &forced,
            &pool,
            &well_collateralised(),
            &BTreeMap::new(),
            &auction,
            START + 1,
            percent(100),
        ));
        assert_eq!(draft.fill_ledger, START + 350);
    }

    /// One stroop of XLM is worth ⌊1e6 × 1 / 1e7⌋ = 0.
    #[test]
    fn a_lot_worth_nothing_is_unprofitable() {
        let pool = pool();
        let auction = AuctionData {
            lot: BTreeMap::from([(XLM.to_string(), 1)]),
            ..auction()
        };
        let planned = plan(
            &terms(),
            &pool,
            &well_collateralised(),
            &BTreeMap::new(),
            &auction,
            START + 1,
            percent(100),
        );
        assert_eq!(planned, PlannedFill::Skip(FillSkip::Unprofitable));
    }

    /// A filler already under water — $7,500 effective against $10,526
    /// effective of its own debt — and nothing to supply: even at 400,
    /// with the bid gone, $9,000 against $11,579 needed.
    #[test]
    fn nothing_that_closes_the_gap_is_a_health_skip() {
        let pool = pool();
        let terms = FillTerms {
            supply_allowed: false,
            ..terms()
        };
        let under_water = Positions {
            collateral: BTreeMap::from([(0, 1_000_000_000_000)]),
            liabilities: BTreeMap::from([(1, 100_000_000_000)]),
            ..Positions::default()
        };
        let planned = plan(
            &terms,
            &pool,
            &under_water,
            &BTreeMap::new(),
            &auction(),
            START + 1,
            percent(100),
        );
        assert_eq!(planned, PlannedFill::Skip(FillSkip::Health));
    }

    /// The same filler with a little XLM it may supply: more XLM would have
    /// closed it, so the skip says the wallet was short, not the plan.
    #[test]
    fn a_shortfall_only_more_primary_would_close_is_unfunded() {
        let pool = pool();
        let under_water = Positions {
            collateral: BTreeMap::from([(0, 1_000_000_000_000)]),
            liabilities: BTreeMap::from([(1, 100_000_000_000)]),
            ..Positions::default()
        };
        let wallet = BTreeMap::from([(XLM.to_string(), 1_000_000_000)]);
        let planned = plan(
            &terms(),
            &pool,
            &under_water,
            &wallet,
            &auction(),
            START + 1,
            percent(100),
        );
        assert_eq!(planned, PlannedFill::Skip(FillSkip::Unfunded));
    }

    /// A candidate a search proves is the plan, not one held over for a
    /// round that may never come. With a single round to spend, the ledger
    /// search's find is drafted where it was found; two rounds — one to
    /// fund the supply, one to search — prove the same of the percent
    /// search. Both answered a skip while the search's find waited for a
    /// verification round.
    #[test]
    fn a_search_that_runs_out_of_rounds_still_returns_what_it_found() {
        let pool = pool();
        let one_round = FillTerms {
            plan_iterations: 1,
            ..terms()
        };
        let waited = draft(plan(
            &one_round,
            &pool,
            &Positions::default(),
            &BTreeMap::new(),
            &auction(),
            START + 1,
            percent(100),
        ));
        assert_eq!(
            (waited.fill_ledger, waited.percent),
            (START + 155, percent(100))
        );

        let two_rounds = FillTerms {
            plan_iterations: 2,
            ..terms()
        };
        let wallet = BTreeMap::from([(XLM.to_string(), 10_000_000_000)]);
        let lowered = draft(plan(
            &two_rounds,
            &pool,
            &Positions::default(),
            &wallet,
            &auction(),
            START + 1,
            percent(100),
        ));
        assert_eq!(
            (lowered.fill_ledger, lowered.percent),
            (START + 110, percent(22))
        );
    }

    /// The margin is judged on the whole auction, so a fill whose own lot
    /// no longer covers its own bid can still reach a draft. At a zero
    /// margin $2,000 of lot against $2,000 of bid meets the margin on the
    /// lot ramp at ⌈200 × 2e10 / 2e10⌉ = 200 ledgers, where both modifiers
    /// are one: the whole lot for the whole bid, a profit of exactly
    /// nothing. Only `force_fill` takes it.
    #[test]
    fn a_fill_that_gains_nothing_is_unprofitable() {
        let pool = pool();
        let terms = FillTerms {
            profit_bps: 0,
            ..terms()
        };
        let auction = AuctionData {
            lot: BTreeMap::from([(XLM.to_string(), 200_000_000_000)]),
            bid: BTreeMap::from([(USDC.to_string(), 20_000_000_000)]),
            block: START,
        };
        let planned = plan(
            &terms,
            &pool,
            &well_collateralised(),
            &BTreeMap::new(),
            &auction,
            START + 1,
            percent(100),
        );
        assert_eq!(planned, PlannedFill::Skip(FillSkip::Unprofitable));
        let forced = FillTerms {
            force_fill: true,
            ..terms
        };
        let draft = draft(plan(
            &forced,
            &pool,
            &well_collateralised(),
            &BTreeMap::new(),
            &auction,
            START + 1,
            percent(100),
        ));
        assert_eq!(draft.fill_ledger, START + 200);
        assert_eq!(
            (draft.lot_value, draft.bid_value, draft.est_profit),
            (20_000_000_000, 20_000_000_000, 0)
        );
    }

    /// `min_collateral`, not the health factor, is what a plan can be short
    /// of: at 110 ledgers the fill is worth 8.25e9 of effective collateral
    /// against 10_526_315_790 of effective liability — a health factor of
    /// 0.78 — but against a $10,000 `min_collateral` the binding shortfall
    /// is 1e11 − 8.25e9 = 91_750_000_000, far more than the floor's
    /// 3_328_947_369. The supply is ⌈91_750_000_000 × 1e7 / 1e6⌉ × 1e7 /
    /// 7_500_000 = 1_223_333_333_334 stroops plus the two-unit allowance,
    /// which lands the projection exactly on 1e11 of effective collateral
    /// and a health factor of ⌊1e11 × 1e7 / 10_526_315_790⌋ = 94_999_999.
    #[test]
    fn a_plan_supplies_up_to_the_pools_minimum_collateral() {
        let pool = pool();
        let terms = FillTerms {
            min_collateral: 100_000_000_000,
            ..terms()
        };
        let wallet = BTreeMap::from([(XLM.to_string(), 10_000_000_000_000)]);
        let draft = draft(plan(
            &terms,
            &pool,
            &Positions::default(),
            &wallet,
            &auction(),
            START + 1,
            percent(100),
        ));
        assert_eq!(draft.fill_ledger, START + 110);
        assert_eq!(
            draft.actions,
            vec![FillAction::SupplyCollateral {
                asset: XLM.to_string(),
                amount: 1_223_333_333_336
            }]
        );
        assert_eq!(draft.projected_health, Some(94_999_999));
        // The projection the plan was drafted from, valued again from its
        // parts: the lot plus the supply as collateral, the bid as
        // liabilities. Its effective collateral is what `min_collateral`
        // asked for, to the stroop.
        let data = calculate_position_data(
            &pool.reserves,
            &pool.prices,
            &Positions {
                collateral: BTreeMap::from([(0, 110_000_000_000 + 1_223_333_333_336)]),
                liabilities: BTreeMap::from([(1, 10_000_000_000)]),
                ..Positions::default()
            },
        )
        .expect("values");
        assert_eq!(data.collateral_base, 100_000_000_000);
        assert!(data.collateral_base >= terms.min_collateral);
    }

    /// The executor's re-plan passes a lower ceiling, and the plan honours
    /// it.
    #[test]
    fn the_percent_never_exceeds_the_ceiling() {
        let pool = pool();
        let terms = FillTerms {
            profit_bps: 20_000,
            ..terms()
        };
        let draft = draft(plan(
            &terms,
            &pool,
            &Positions::default(),
            &BTreeMap::new(),
            &auction(),
            START + 1,
            percent(50),
        ));
        assert_eq!(draft.percent, percent(50));
    }

    /// Nothing a plan spends exceeds what the wallet holds, whichever way
    /// it closes the gap.
    #[test]
    fn a_plan_never_spends_more_than_the_wallet_holds() {
        let pool = pool();
        for wallet in [
            BTreeMap::new(),
            BTreeMap::from([(XLM.to_string(), 10_000_000_000)]),
            BTreeMap::from([(USDC.to_string(), 4_000_000_000), (XLM.to_string(), 7)]),
            BTreeMap::from([
                (USDC.to_string(), 20_000_000_000),
                (XLM.to_string(), 1_000_000_000_000),
            ]),
        ] {
            if let PlannedFill::Fill(draft) = plan(
                &terms(),
                &pool,
                &Positions::default(),
                &wallet,
                &auction(),
                START + 1,
                percent(100),
            ) {
                for (asset, spent) in &draft.spend {
                    assert!(
                        *spent <= wallet.get(asset).copied().unwrap_or(0),
                        "{asset}: {spent} of {wallet:?}"
                    );
                }
            }
        }
    }
}
