//! The unwinder's arithmetic: which of the filler's own debts to repay from
//! its wallet, and which of its collateral to withdraw, once a fill has left
//! it holding the auction's lot as collateral and owing the auction's bid.
//! Pure: no I/O, nothing panics, and every step is checked arithmetic that
//! answers with a [`MathError`] rather than a plausible-looking number.
//!
//! Spec §5's "Unwind" subsection, in the three steps [`plan_unwind`] carries:
//!
//! 1. repay each liability asset the wallet holds, noting which liabilities
//!    remain;
//! 2. with no liabilities left the contract health-checks nothing, so every
//!    collateral but the primary goes entirely and the primary goes down to
//!    its floor;
//! 3. with liabilities left, withdraw only while the *projected* health
//!    factor stays at or above `min_health_factor`.
//!
//! The rulings the phase took, which the code is written to rather than
//! around:
//!
//! - **5.** A repay is the debt in underlying plus a one-basis-point
//!   allowance plus one unit, capped at what the wallet can spend — the same
//!   rule [`super::fill`] uses, shared with it rather than restated. The
//!   contract refunds the excess, but the wallet must hold all of it.
//! - **6.** The unwind floor is the pool's `min_health_factor` alone;
//!   `HF_SAFETY_MULTIPLIER` is the *fill's* margin, not this one.
//! - **7.** [`DUST_FLOOR_BPS`] applies to every partial withdrawal of the
//!   primary asset, in step 2 as well as step 3, and a `WithdrawAll` of a
//!   non-primary asset is never dust. [`HEALTH_MARGIN_BPS`] ends step 3's
//!   walk: once the projection is within 0.5% of the minimum, nothing
//!   further is withdrawn.
//! - **8.** Withdrawal amounts are found by formula and *verified by exact
//!   projection*, backing off in bounded steps when the projection
//!   disagrees. No amount reaches a plan unprojected.
//!
//! Values are in the pool oracle's units (`OraclePrices::scalar`), the units
//! `PositionData` reports; amounts in an [`UnwindAction`] are underlying, in
//! the asset's own decimals, and a [`UnwindAction::WithdrawAll`] carries none
//! at all because the executor sends the contract's `WITHDRAW_ALL`.

use std::collections::BTreeMap;

use super::fill::{add_to, BPS, REPAY_ALLOWANCE_BPS};
use super::fixed::{div_ceil, mul_ceil, mul_floor, MathError, SCALAR_7};
use super::position::{calculate_position_data, OraclePrices, PositionData, Positions};
use super::reserve::Reserve;

/// How close to `min_health_factor` step 3 withdraws before it stops, in
/// basis points: at 0.5% of the minimum the next ledger's interest could
/// take the position under it, and a withdrawal the contract refuses costs
/// a fee and a pass.
pub const HEALTH_MARGIN_BPS: i128 = 50;

/// The smallest withdrawal of the primary asset worth a transaction, as a
/// fraction of `min_primary_collateral` in basis points. Below it the fee
/// outweighs the amount moved, and the next pass will find the same dust.
pub const DUST_FLOOR_BPS: i128 = 100;

/// How far a partial withdrawal is cut when its projection disagrees with
/// the formula, as a fraction of itself in basis points. Rounded *up*, so
/// every round makes progress however small the amount.
const BACKOFF_BPS: i128 = 1;

/// How many times a partial withdrawal is backed off before the candidate is
/// given up on. Bounded because the formula is exact up to the rounding of
/// one conversion chain: a projection that still disagrees after this many
/// rounds means the estimate was wrong about something else, and guessing
/// further is not free.
const BACKOFF_ROUNDS: u32 = 8;

/// What the operator holds one unwind to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwindTerms {
    /// The asset the filler keeps as collateral, and the only one an unwind
    /// leaves any of behind.
    pub primary_asset: String,
    /// The least underlying of `primary_asset` the unwind leaves as
    /// collateral. Zero means "withdraw all of it".
    pub min_primary_collateral: i128,
    /// The pool's `min_health_factor`, 7 decimals: the floor every
    /// projection in step 3 is held to (ruling 6).
    pub min_health_factor: i128,
}

/// The chain state one plan is made against, all read at one ledger.
#[derive(Debug, Clone, Copy)]
pub struct UnwindInputs<'a> {
    /// The pool's reserves, accrued to the valuation time and keyed by
    /// `ReserveConfig::index` — the key `positions` uses.
    pub reserves: &'a BTreeMap<u32, Reserve>,
    /// Asset address to reserve index.
    pub asset_index: &'a BTreeMap<String, u32>,
    /// The pool oracle's prices.
    pub prices: &'a OraclePrices,
    /// The filler's own positions in this pool, as the fill left them.
    pub positions: &'a Positions,
    /// What the filler's wallet may spend, per asset: its balance less the
    /// fee reserve and every live reservation (ruling 5).
    pub wallet: &'a BTreeMap<String, i128>,
}

/// One request an unwind sends, in the order sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnwindAction {
    /// Repay `amount` of `asset` from the wallet. The contract refunds
    /// whatever exceeds the debt, so the allowance costs nothing but must
    /// be held.
    Repay {
        /// The liability asset.
        asset: String,
        /// Underlying, in the asset's decimals.
        amount: i128,
    },
    /// Withdraw `amount` of `asset` from collateral.
    Withdraw {
        /// The collateral asset.
        asset: String,
        /// Underlying, in the asset's decimals.
        amount: i128,
    },
    /// Withdraw every b-token of `asset`. Carries no amount: the executor
    /// sends the contract's `WITHDRAW_ALL`, which is exact whatever the
    /// position has accrued to by the time the request lands.
    WithdrawAll {
        /// The collateral asset.
        asset: String,
    },
}

/// What one unwind pass moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwindPlan {
    /// The requests, in the order sent. Empty is an idle pass.
    pub actions: Vec<UnwindAction>,
    /// The wallet amounts the repays take, per asset: what a live pass
    /// reserves.
    pub spend: BTreeMap<String, i128>,
    /// The liability assets the wallet could not fully repay, in
    /// reserve-index order. What an idle pass notifies about.
    pub remaining_liabilities: Vec<String>,
    /// The health factor the plan's own projection ends on, in the oracle's
    /// scale; `None` when no liabilities remain and the contract checks
    /// nothing. For a repay the wallet capped below its allowance this is
    /// the *conservative* figure the plan decided against — that repay is
    /// projected to clear nothing, so the number is the one the floor was
    /// held to and not what the contract will find once it lands (see
    /// `repay`). `remaining_liabilities` names every such asset.
    pub projected_health: Option<i128>,
}

impl UnwindPlan {
    /// Whether the pass moves nothing. The filler stops repeating on the
    /// first idle pass, so this is what ends an unwind.
    pub fn is_idle(&self) -> bool {
        self.actions.is_empty()
    }
}

/// Builds one unwind pass (spec §5, "Unwind").
///
/// 1. **Repay** each liability asset the wallet holds a positive balance of:
///    the debt in underlying (rounded up, as the borrower owes it) plus
///    `REPAY_ALLOWANCE_BPS` of it and one unit, capped at the balance. The
///    burn is projected as the contract's — `to_d_token_down(amount)`,
///    capped at the debt — and every asset still owing afterwards goes in
///    `remaining_liabilities`, in reserve-index order.
/// 2. **No liabilities remain:** `WithdrawAll` every collateral asset but
///    the primary, in reserve-index order, then the primary down to
///    `min_primary_collateral`. No projection is consulted, because a
///    position with no liabilities is not health-checked at all.
/// 3. **Liabilities remain:** withdraw while the projected health factor
///    stays at or above `min_health_factor`. Candidates, in an order fixed
///    once from the post-repay positions: collateral assets that are also
///    liabilities (reserve-index order), then the remaining non-primary
///    collateral by ascending effective value (ties by index), then the
///    primary. Before each candidate the position is re-projected, and a
///    projection already within [`HEALTH_MARGIN_BPS`] of the minimum ends
///    the walk.
///
/// Every amount `spend` records is debited from a copy of the wallet that no
/// debit takes below zero, so a plan never spends more of an asset than the
/// wallet holds; and no withdrawal ever leaves the primary asset under
/// `min_primary_collateral`, whatever the health factor would allow.
///
/// # Errors
///
/// `MathError::MissingReserve` or `MissingPrice` when the caller's view of
/// the pool does not cover a position it holds, `InvalidInput` when the
/// primary asset is not one of the pool's reserves, and `Overflow` only for
/// values no pool holds.
pub fn plan_unwind(
    terms: &UnwindTerms,
    inputs: &UnwindInputs<'_>,
) -> Result<UnwindPlan, MathError> {
    let mut positions = inputs.positions.clone();
    let mut wallet = inputs.wallet.clone();
    let mut actions = Vec::new();
    let mut spend = BTreeMap::new();
    repay(
        inputs,
        &mut positions,
        &mut wallet,
        &mut actions,
        &mut spend,
    )?;

    let mut remaining_liabilities = Vec::new();
    for index in positions.liabilities.keys() {
        remaining_liabilities.push(reserve_at(inputs, *index)?.asset.clone());
    }

    let projected_health = if positions.liabilities.is_empty() {
        withdraw_free(terms, inputs, &mut positions, &mut actions)?;
        None
    } else {
        withdraw_within_floor(terms, inputs, &mut positions, &mut actions)?
    };

    Ok(UnwindPlan {
        actions,
        spend,
        remaining_liabilities,
        projected_health,
    })
}

/// Step 1, over `positions.liabilities` in reserve-index order: the fill
/// planner's repay rule, with the contract's own burn applied to
/// `positions` so the later steps decide against the debt the repay
/// actually leaves.
///
/// An entry already at zero is no debt — `calculate_position_data` values it
/// at nothing — so it is removed rather than carried: leaving it would make
/// step 2's "no liabilities remain" read false for a position the contract
/// health-checks not at all.
fn repay(
    inputs: &UnwindInputs<'_>,
    positions: &mut Positions,
    wallet: &mut BTreeMap<String, i128>,
    actions: &mut Vec<UnwindAction>,
    spend: &mut BTreeMap<String, i128>,
) -> Result<(), MathError> {
    let owed_on: Vec<u32> = positions.liabilities.keys().copied().collect();
    for index in owed_on {
        let reserve = reserve_at(inputs, index)?;
        let owing = positions.liabilities.get(&index).copied().unwrap_or(0);
        if owing <= 0 {
            positions.liabilities.remove(&index);
            continue;
        }
        let held = wallet.get(&reserve.asset).copied().unwrap_or(0);
        if held <= 0 {
            continue;
        }
        let owed = reserve.to_asset_from_d_token(owing)?;
        let wanted = owed
            .checked_add(mul_floor(owed, REPAY_ALLOWANCE_BPS, BPS)?)
            .and_then(|amount| amount.checked_add(1))
            .ok_or(MathError::Overflow)?;
        let amount = wanted.min(held);
        let burnt = reserve.to_d_token_down(amount)?.min(owing);
        let left = owing.checked_sub(burnt).ok_or(MathError::Overflow)?;
        // A repay the wallet capped below `wanted` clears the debt only at
        // the `d_rate` this plan was built at: with the allowance trimmed
        // away, any accrual between here and the ledger the repay lands in
        // leaves a dust debt behind. Projecting that as cleared takes step
        // 2, which withdraws every collateral without a health check at
        // all — and the contract then refuses the whole transaction, a
        // dust debt standing against nothing. So a capped repay is
        // projected to clear nothing, step 3 keeps the health floor, and
        // the next pass — planning against the debt the repay actually
        // left — finishes.
        let left = if left == 0 && amount < wanted {
            owing
        } else {
            left
        };
        if left > 0 {
            positions.liabilities.insert(index, left);
        } else {
            positions.liabilities.remove(&index);
        }
        let rest = held.checked_sub(amount).ok_or(MathError::Overflow)?;
        wallet.insert(reserve.asset.clone(), rest);
        add_to(spend, reserve.asset.clone(), amount)?;
        actions.push(UnwindAction::Repay {
            asset: reserve.asset.clone(),
            amount,
        });
    }
    Ok(())
}

/// Step 2, for a position the contract will not health-check: every
/// collateral but the primary entirely, in reserve-index order, then the
/// primary's excess over `min_primary_collateral` — as a `WithdrawAll` when
/// the floor is zero, and not at all when the excess is dust (ruling 7).
fn withdraw_free(
    terms: &UnwindTerms,
    inputs: &UnwindInputs<'_>,
    positions: &mut Positions,
    actions: &mut Vec<UnwindAction>,
) -> Result<(), MathError> {
    let primary = primary_index(terms, inputs)?;
    let held_in: Vec<u32> = positions.collateral.keys().copied().collect();
    for index in held_in {
        if index == primary {
            continue;
        }
        let empty = positions.collateral.get(&index).copied().unwrap_or(0) <= 0;
        let reserve = reserve_at(inputs, index)?;
        positions.collateral.remove(&index);
        // A zero position is nothing to withdraw: the contract's
        // `WithdrawCollateral` has nothing to burn and would refuse the
        // whole call.
        if !empty {
            actions.push(UnwindAction::WithdrawAll {
                asset: reserve.asset.clone(),
            });
        }
    }

    let held = positions.collateral.get(&primary).copied().unwrap_or(0);
    if held <= 0 {
        return Ok(());
    }
    let reserve = reserve_at(inputs, primary)?;
    if terms.min_primary_collateral <= 0 {
        positions.collateral.remove(&primary);
        actions.push(UnwindAction::WithdrawAll {
            asset: reserve.asset.clone(),
        });
        return Ok(());
    }
    let excess = primary_excess(terms, inputs, positions)?;
    if excess <= 0 || dust(terms, excess)? {
        return Ok(());
    }
    *positions = after_withdrawal(inputs, positions, primary, excess)?;
    actions.push(UnwindAction::Withdraw {
        asset: reserve.asset.clone(),
        amount: excess,
    });
    Ok(())
}

/// Step 3, for a position that keeps liabilities: each candidate in turn,
/// stopping at the first projection within [`HEALTH_MARGIN_BPS`] of
/// `min_health_factor`. Answers the health factor the plan's own end state
/// projects to.
///
/// The candidate order is built once, before any withdrawal: re-ordering
/// after each one would let an asset whose partial withdrawal shrank it slip
/// ahead of assets the spec puts before it.
fn withdraw_within_floor(
    terms: &UnwindTerms,
    inputs: &UnwindInputs<'_>,
    positions: &mut Positions,
    actions: &mut Vec<UnwindAction>,
) -> Result<Option<i128>, MathError> {
    let margin = mul_ceil(
        terms.min_health_factor,
        BPS.checked_add(HEALTH_MARGIN_BPS)
            .ok_or(MathError::Overflow)?,
        BPS,
    )?;
    for index in candidates(terms, inputs, positions)? {
        let data = project(inputs, positions)?;
        if data.is_hf_under(margin)? {
            break;
        }
        withdraw_candidate(terms, inputs, positions, actions, &data, index)?;
    }
    project(inputs, positions)?.health_factor()
}

/// Step 3's candidate order, from the post-repay positions: collateral
/// assets that are also liabilities in reserve-index order, then the
/// remaining non-primary collateral by ascending effective value with ties
/// broken by index, then the primary. A position of nothing is no candidate.
fn candidates(
    terms: &UnwindTerms,
    inputs: &UnwindInputs<'_>,
    positions: &Positions,
) -> Result<Vec<u32>, MathError> {
    let primary = primary_index(terms, inputs)?;
    let mut order = Vec::new();
    let mut rest = Vec::new();
    let mut holds_primary = false;
    for (index, b_tokens) in &positions.collateral {
        if *b_tokens <= 0 {
            continue;
        }
        if *index == primary {
            holds_primary = true;
        } else if positions.liabilities.contains_key(index) {
            // `collateral` is a `BTreeMap`, so this is already in
            // reserve-index order.
            order.push(*index);
        } else {
            let reserve = reserve_at(inputs, *index)?;
            let price = inputs.prices.price(&reserve.asset)?;
            let effective = reserve.to_effective_asset_from_b_token(*b_tokens)?;
            rest.push((mul_floor(price, effective, reserve.scalar)?, *index));
        }
    }
    rest.sort_unstable();
    order.extend(rest.into_iter().map(|(_, index)| index));
    if holds_primary {
        order.push(primary);
    }
    Ok(order)
}

/// One candidate: its whole position first — the primary's "all" being its
/// excess over the floor — and, when the projection refuses that, the
/// largest partial the formula and the projection agree on.
fn withdraw_candidate(
    terms: &UnwindTerms,
    inputs: &UnwindInputs<'_>,
    positions: &mut Positions,
    actions: &mut Vec<UnwindAction>,
    data: &PositionData,
    index: u32,
) -> Result<(), MathError> {
    if index == primary_index(terms, inputs)? {
        let excess = primary_excess(terms, inputs, positions)?;
        // Nothing smaller than a dust excess is worth sending either, so
        // the candidate ends here rather than falling through to a partial.
        if excess <= 0 || dust(terms, excess)? {
            return Ok(());
        }
        if take_withdrawal(terms, inputs, positions, actions, index, excess)? {
            return Ok(());
        }
    } else {
        let reserve = reserve_at(inputs, index)?;
        let mut whole = positions.clone();
        whole.collateral.remove(&index);
        if !project(inputs, &whole)?.is_hf_under(terms.min_health_factor)? {
            *positions = whole;
            actions.push(UnwindAction::WithdrawAll {
                asset: reserve.asset.clone(),
            });
            return Ok(());
        }
    }
    partial(terms, inputs, positions, actions, data, index)
}

/// The largest partial withdrawal of `index` the floor allows, by formula
/// and then by projection.
///
/// The floor is charged to the *whole* position, never to this candidate
/// alone: the collateral base it requires is `mul_ceil(liability_base,
/// min_health_factor, SCALAR_7)`, what may go is `collateral_base` less
/// that, and what must stay of this asset is its own contribution — valued
/// exactly as `calculate_position_data` values it — less what may go,
/// floored at zero. Charging the whole floor to one candidate throws away
/// every other remaining collateral's base, which leaves the position
/// untouched whenever no single asset could carry the floor by itself.
///
/// That base then converts back, every step rounded so what stays is never
/// a unit short of what the floor asks for: effective underlying through
/// the price (`mul_ceil(base, reserve.scalar, price)`), underlying through
/// the collateral factor (`div_ceil(effective, c_factor, SCALAR_7)`), and
/// b-tokens through `b_rate` (`to_b_token_up`). What goes is the position
/// less that, never taking the primary below `to_b_token_up`'s image of its
/// own floor, converted back to the request's underlying with
/// `to_asset_from_b_token` — which rounds down, so the contract's
/// `to_b_token_up(amount)` burns no more than the formula allowed.
///
/// The result is then *verified* by projection and cut by [`BACKOFF_BPS`]
/// of itself up to [`BACKOFF_ROUNDS`] times before the candidate is given up
/// on (ruling 8). Every step of the chain above rounds the kept side up, so
/// the projection holds by construction and the back-off is the guard rather
/// than the mechanism: it is what keeps the plan safe — never the floor
/// broken, only a smaller withdrawal — if a contract upgrade ever moves one
/// of those roundings.
fn partial(
    terms: &UnwindTerms,
    inputs: &UnwindInputs<'_>,
    positions: &mut Positions,
    actions: &mut Vec<UnwindAction>,
    data: &PositionData,
    index: u32,
) -> Result<(), MathError> {
    let reserve = reserve_at(inputs, index)?;
    let c_factor = i128::from(reserve.config.c_factor);
    // A reserve with no collateral factor adds nothing to the health factor,
    // so no part of it can hold a floor its whole position did not — and
    // dividing by the factor below would be a division by zero.
    if c_factor == 0 {
        return Ok(());
    }
    let held = positions.collateral.get(&index).copied().unwrap_or(0);
    if held <= 0 {
        return Ok(());
    }
    let price = inputs.prices.price(&reserve.asset)?;
    let required = mul_ceil(data.liability_base, terms.min_health_factor, SCALAR_7)?;
    let may_go = data
        .collateral_base
        .checked_sub(required)
        .ok_or(MathError::Overflow)?;
    // This candidate's own contribution to `collateral_base`, by
    // `calculate_position_data`'s rounding, so the two cannot disagree
    // about what removing it would cost.
    let carries = mul_floor(
        price,
        reserve.to_effective_asset_from_b_token(held)?,
        reserve.scalar,
    )?;
    let stays_base = carries
        .checked_sub(may_go)
        .ok_or(MathError::Overflow)?
        .max(0);
    let stays = reserve.to_b_token_up(div_ceil(
        mul_ceil(stays_base, reserve.scalar, price)?,
        c_factor,
        SCALAR_7,
    )?)?;
    let mut goes = held.checked_sub(stays).ok_or(MathError::Overflow)?;
    let is_primary = index == primary_index(terms, inputs)?;
    if is_primary {
        let floor = reserve.to_b_token_up(terms.min_primary_collateral.max(0))?;
        goes = goes.min(held.checked_sub(floor).ok_or(MathError::Overflow)?);
    }
    if goes <= 0 {
        return Ok(());
    }

    let mut amount = reserve.to_asset_from_b_token(goes)?;
    for _ in 0..=BACKOFF_ROUNDS {
        if amount <= 0 || (is_primary && dust(terms, amount)?) {
            return Ok(());
        }
        if take_withdrawal(terms, inputs, positions, actions, index, amount)? {
            return Ok(());
        }
        amount = amount
            .checked_sub(mul_ceil(amount, BACKOFF_BPS, BPS)?)
            .ok_or(MathError::Overflow)?;
    }
    Ok(())
}

/// Projects a `Withdraw` of `amount` underlying from `index` and takes it
/// when the projection holds `min_health_factor`. A `false` leaves
/// `positions` and `actions` exactly as they were.
fn take_withdrawal(
    terms: &UnwindTerms,
    inputs: &UnwindInputs<'_>,
    positions: &mut Positions,
    actions: &mut Vec<UnwindAction>,
    index: u32,
    amount: i128,
) -> Result<bool, MathError> {
    if amount <= 0 {
        return Ok(false);
    }
    let next = after_withdrawal(inputs, positions, index, amount)?;
    if project(inputs, &next)?.is_hf_under(terms.min_health_factor)? {
        return Ok(false);
    }
    actions.push(UnwindAction::Withdraw {
        asset: reserve_at(inputs, index)?.asset.clone(),
        amount,
    });
    *positions = next;
    Ok(true)
}

/// The positions a `WithdrawCollateral` of `amount` underlying from `index`
/// leaves, exactly as the contract computes it: `to_b_token_up(amount)`
/// burnt, capped at the position.
fn after_withdrawal(
    inputs: &UnwindInputs<'_>,
    positions: &Positions,
    index: u32,
    amount: i128,
) -> Result<Positions, MathError> {
    let reserve = reserve_at(inputs, index)?;
    let held = positions.collateral.get(&index).copied().unwrap_or(0);
    let burnt = reserve.to_b_token_up(amount)?.min(held);
    let left = held.checked_sub(burnt).ok_or(MathError::Overflow)?;
    let mut next = positions.clone();
    if left > 0 {
        next.collateral.insert(index, left);
    } else {
        next.collateral.remove(&index);
    }
    Ok(next)
}

/// The primary asset's underlying above `min_primary_collateral`, or zero
/// when the position is already at or under it.
///
/// Computed in b-tokens — the floor converted up, taken from the position,
/// and the remainder converted back down — rather than as the difference of
/// two underlying amounts. `to_asset_from_b_token` rounds down and the
/// contract burns `to_b_token_up(amount)`, so at any `b_rate` above one the
/// difference of the two underlying numbers can leave the position a unit
/// under the floor; this route cannot, because the b-tokens that stay are
/// `to_b_token_up(floor)` by construction.
fn primary_excess(
    terms: &UnwindTerms,
    inputs: &UnwindInputs<'_>,
    positions: &Positions,
) -> Result<i128, MathError> {
    let primary = primary_index(terms, inputs)?;
    let reserve = reserve_at(inputs, primary)?;
    let held = positions.collateral.get(&primary).copied().unwrap_or(0);
    if held <= 0 {
        return Ok(0);
    }
    let floor = reserve.to_b_token_up(terms.min_primary_collateral.max(0))?;
    let goes = held.checked_sub(floor).ok_or(MathError::Overflow)?;
    if goes <= 0 {
        return Ok(0);
    }
    reserve.to_asset_from_b_token(goes)
}

/// Whether a withdrawal of the primary asset is too small to send: under
/// [`DUST_FLOOR_BPS`] of `min_primary_collateral` (ruling 7). A zero floor
/// makes nothing dust.
fn dust(terms: &UnwindTerms, amount: i128) -> Result<bool, MathError> {
    Ok(amount < mul_floor(terms.min_primary_collateral.max(0), DUST_FLOOR_BPS, BPS)?)
}

/// The working positions, valued. One function for every projection in this
/// module, so no two of them can disagree about rounding.
fn project(inputs: &UnwindInputs<'_>, positions: &Positions) -> Result<PositionData, MathError> {
    calculate_position_data(inputs.reserves, inputs.prices, positions)
}

/// The reserve index of the primary asset. An asset the pool does not list
/// is `InvalidInput`: the caller's terms and its view of the pool disagree,
/// and reading it as "there is no primary here" would withdraw the very
/// collateral the floor exists to keep.
fn primary_index(terms: &UnwindTerms, inputs: &UnwindInputs<'_>) -> Result<u32, MathError> {
    inputs
        .asset_index
        .get(&terms.primary_asset)
        .copied()
        .ok_or(MathError::InvalidInput(
            "the primary asset is not a reserve of this pool",
        ))
}

/// The reserve at `index`. A position in an index the pool does not have is
/// `MissingReserve` and never a zero: it means the caller's view of the pool
/// is incomplete, and a zero would silently understate the position.
fn reserve_at<'a>(inputs: &UnwindInputs<'a>, index: u32) -> Result<&'a Reserve, MathError> {
    inputs
        .reserves
        .get(&index)
        .ok_or(MathError::MissingReserve(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{ReserveConfig, ReserveData, SCALAR_12};

    const XLM: &str = "XLM";
    const USDC: &str = "USDC";
    const NO_CF: &str = "NOCF";

    /// A reserve with both rates at `rate` (12 decimals). Nothing accrues:
    /// the supplies are zero, so the reserve a plan is built against is the
    /// reserve every projection in these tests values.
    fn reserve(asset: &str, index: u32, c_factor: u32, l_factor: u32, rate: i128) -> Reserve {
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
                d_rate: rate,
                b_rate: rate,
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
    /// $1 reserve with no collateral factor at all — `math::fill`'s pool,
    /// copied rather than shared, because the two modules' fixtures may
    /// diverge. `rate` is both rates of every reserve.
    fn pool_at(rate: i128) -> Pool {
        Pool {
            reserves: BTreeMap::from([
                (0, reserve(XLM, 0, 7_500_000, 7_500_000, rate)),
                (1, reserve(USDC, 1, 9_500_000, 9_500_000, rate)),
                (2, reserve(NO_CF, 2, 0, 10_000_000, rate)),
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

    /// Every rate exactly one, so b-tokens, d-tokens and underlying are the
    /// same number and each figure below is the contract's own rounding of
    /// the factors and the price alone.
    fn pool() -> Pool {
        pool_at(SCALAR_12)
    }

    /// XLM as the primary asset, a floor of 10,000 XLM, and the pool's
    /// minimum health factor of 1.5.
    fn terms() -> UnwindTerms {
        UnwindTerms {
            primary_asset: XLM.to_string(),
            min_primary_collateral: 100_000_000_000,
            min_health_factor: 15_000_000,
        }
    }

    fn positions(collateral: &[(u32, i128)], liabilities: &[(u32, i128)]) -> Positions {
        Positions {
            collateral: collateral.iter().copied().collect(),
            liabilities: liabilities.iter().copied().collect(),
            ..Positions::default()
        }
    }

    fn wallet(entries: &[(&str, i128)]) -> BTreeMap<String, i128> {
        entries
            .iter()
            .map(|(asset, amount)| ((*asset).to_string(), *amount))
            .collect()
    }

    fn inputs<'a>(
        pool: &'a Pool,
        positions: &'a Positions,
        wallet: &'a BTreeMap<String, i128>,
    ) -> UnwindInputs<'a> {
        UnwindInputs {
            reserves: &pool.reserves,
            asset_index: &pool.asset_index,
            prices: &pool.prices,
            positions,
            wallet,
        }
    }

    fn plan(
        terms: &UnwindTerms,
        pool: &Pool,
        positions: &Positions,
        wallet: &BTreeMap<String, i128>,
    ) -> UnwindPlan {
        plan_unwind(terms, &inputs(pool, positions, wallet)).expect("no arithmetic failure")
    }

    /// The health factor step 3 stops at, and the smallest withdrawal of
    /// the primary worth sending — the two bounds the maximality clause
    /// below is judged against, from the module's own constants.
    fn margin(terms: &UnwindTerms) -> i128 {
        mul_ceil(terms.min_health_factor, BPS + HEALTH_MARGIN_BPS, BPS).expect("a margin")
    }

    fn dust_floor(terms: &UnwindTerms) -> i128 {
        mul_floor(terms.min_primary_collateral, DUST_FLOOR_BPS, BPS).expect("a dust floor")
    }

    /// The largest `Withdraw` of `index` from `positions` that holds both
    /// floors — the health factor at or above `min_health_factor` and the
    /// primary at or above `min_primary_collateral` — found by binary
    /// search over the builder's own [`after_withdrawal`] and [`project`],
    /// so the search and the plan cannot disagree about the contract's
    /// rounding. Monotone, and so searchable: a larger amount burns no
    /// fewer b-tokens, and the health factor never rises with it.
    fn largest_withdrawal(
        terms: &UnwindTerms,
        inputs: &UnwindInputs<'_>,
        positions: &Positions,
        index: u32,
    ) -> i128 {
        let primary = inputs
            .asset_index
            .get(&terms.primary_asset)
            .copied()
            .expect("the primary asset");
        let holds = |amount: i128| {
            let next = after_withdrawal(inputs, positions, index, amount).expect("a projection");
            if project(inputs, &next)
                .expect("values")
                .is_hf_under(terms.min_health_factor)
                .expect("hf")
            {
                return false;
            }
            inputs
                .reserves
                .get(&primary)
                .expect("the primary reserve")
                .to_asset_from_b_token(next.collateral.get(&primary).copied().unwrap_or(0))
                .expect("underlying")
                >= terms.min_primary_collateral
        };
        let held = positions.collateral.get(&index).copied().unwrap_or(0);
        let mut low = 0;
        let mut high = inputs
            .reserves
            .get(&index)
            .expect("a listed reserve")
            .to_asset_from_b_token(held)
            .expect("underlying");
        while low < high {
            let mid = low + (high - low + 1) / 2;
            if holds(mid) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        low
    }

    fn repay(asset: &str, amount: i128) -> UnwindAction {
        UnwindAction::Repay {
            asset: asset.to_string(),
            amount,
        }
    }

    fn withdraw(asset: &str, amount: i128) -> UnwindAction {
        UnwindAction::Withdraw {
            asset: asset.to_string(),
            amount,
        }
    }

    fn withdraw_all(asset: &str) -> UnwindAction {
        UnwindAction::WithdrawAll {
            asset: asset.to_string(),
        }
    }

    /// Everything repaid, everything but the floor withdrawn. Debt of 1,000
    /// USDC (1e10) against a wallet of 2,000: the repay is 1e10 + ⌊1e10 /
    /// 10_000⌋ + 1 = 10_001_000_001, which burns the whole debt; with no
    /// liabilities left the USDC collateral goes entirely and the XLM's
    /// excess over the floor, 2e11 − 1e11 = 1e11 (≥ 1% of the floor), goes
    /// as a plain withdrawal.
    #[test]
    fn a_wallet_that_covers_the_debt_unwinds_to_the_floor() {
        let pool = pool();
        let unwound = plan(
            &terms(),
            &pool,
            &positions(
                &[(0, 200_000_000_000), (1, 5_000_000_000)],
                &[(1, 10_000_000_000)],
            ),
            &wallet(&[(USDC, 20_000_000_000)]),
        );
        assert_eq!(
            unwound.actions,
            vec![
                repay(USDC, 10_001_000_001),
                withdraw_all(USDC),
                withdraw(XLM, 100_000_000_000),
            ]
        );
        assert_eq!(unwound.spend, wallet(&[(USDC, 10_001_000_001)]));
        assert!(unwound.remaining_liabilities.is_empty());
        assert_eq!(unwound.projected_health, None);
        assert!(!unwound.is_idle());
    }

    /// The wallet is short: 400 USDC repays 4e9 of the 1e10 debt, leaving
    /// 6e9 d-tokens = 6_315_789_474 of liability base. The floor 1.5 needs
    /// 9_473_684_211 of collateral base. The USDC collateral is also a
    /// liability, so it goes first and entirely (1.5e10 of XLM base
    /// remains, enough); then the primary, last: keeping 9_473_684_211 of
    /// base means keeping ⌈94_736_842_110 / 0.75⌉ = 126_315_789_480
    /// b-tokens, so 73_684_210_520 may go — above the floor, above the dust
    /// rule — and the projection lands on exactly 15_000_000.
    #[test]
    fn a_wallet_short_of_the_debt_withdraws_within_the_floor() {
        let pool = pool();
        let unwound = plan(
            &terms(),
            &pool,
            &positions(
                &[(0, 200_000_000_000), (1, 5_000_000_000)],
                &[(1, 10_000_000_000)],
            ),
            &wallet(&[(USDC, 4_000_000_000)]),
        );
        assert_eq!(
            unwound.actions,
            vec![
                repay(USDC, 4_000_000_000),
                withdraw_all(USDC),
                withdraw(XLM, 73_684_210_520),
            ]
        );
        assert_eq!(unwound.remaining_liabilities, vec![USDC.to_string()]);
        assert_eq!(unwound.projected_health, Some(15_000_000));
    }

    /// The primary never goes below its floor: with a floor of 15,000 XLM
    /// the health floor would allow 73_684_210_520 out but the primary
    /// floor allows only 2e11 − 1.5e11 = 5e10.
    #[test]
    fn the_primary_never_goes_below_its_floor() {
        let pool = pool();
        let terms = UnwindTerms {
            min_primary_collateral: 150_000_000_000,
            ..terms()
        };
        let unwound = plan(
            &terms,
            &pool,
            &positions(&[(0, 200_000_000_000)], &[(1, 6_000_000_000)]),
            &BTreeMap::new(),
        );
        assert_eq!(unwound.actions, vec![withdraw(XLM, 50_000_000_000)]);
        assert_eq!(unwound.remaining_liabilities, vec![USDC.to_string()]);
    }

    /// Also-liability assets first, then the smallest, the primary last. A
    /// no-collateral-factor lot (worth nothing) is the smallest position
    /// and goes second; the primary goes third, down to what the floor
    /// allows.
    #[test]
    fn withdrawals_take_also_liabilities_then_the_smallest_then_the_primary() {
        let pool = pool();
        let unwound = plan(
            &terms(),
            &pool,
            &positions(
                &[(0, 200_000_000_000), (1, 5_000_000_000), (2, 1_000_000_000)],
                &[(1, 6_000_000_000)],
            ),
            &BTreeMap::new(),
        );
        assert_eq!(
            unwound.actions,
            vec![
                withdraw_all(USDC),
                withdraw_all(NO_CF),
                withdraw(XLM, 73_684_210_520),
            ]
        );
    }

    /// Within 0.5% of the minimum nothing more is withdrawn: 2e11 of XLM
    /// against 9_481_000_000 of USDC debt (liability base 9_980_000_000) is
    /// a health factor of ⌊1.5e10 × 1e7 / 9.98e9⌋ = 15_030_060, under the
    /// 15_075_000 the margin sets — an idle pass with the debt remaining.
    #[test]
    fn a_position_within_the_margin_of_the_floor_is_left_alone() {
        let pool = pool();
        let unwound = plan(
            &terms(),
            &pool,
            &positions(&[(0, 200_000_000_000)], &[(1, 9_481_000_000)]),
            &BTreeMap::new(),
        );
        assert!(unwound.is_idle());
        assert_eq!(unwound.remaining_liabilities, vec![USDC.to_string()]);
        assert_eq!(unwound.projected_health, Some(15_030_060));
    }

    /// A primary excess under 1% of the floor is not worth a transaction:
    /// 100_500_000_000 against a floor of 1e11 is 5e8 of excess, under 1e9.
    #[test]
    fn a_dust_excess_over_the_floor_is_not_withdrawn() {
        let pool = pool();
        let unwound = plan(
            &terms(),
            &pool,
            &positions(&[(0, 100_500_000_000)], &[]),
            &BTreeMap::new(),
        );
        assert!(unwound.is_idle());
        assert_eq!(unwound.projected_health, None);
    }

    /// The floor is carried by the whole position, not by one asset. 15,000
    /// XLM (11_250_000_000 of base) and 500 USDC (4_750_000_000) against
    /// 1,000 USDC of debt (liability base 10_526_315_790) is a health factor
    /// of ⌊16e9 × 1e7 / 10_526_315_790⌋ = 15_199_999, clear of the
    /// 15_075_000 margin, and the floor 1.5 requires ⌈10_526_315_790 ×
    /// 1.5⌉ = 15_789_473_685 — so 210_526_315 of base may go. Charging the
    /// whole 15_789_473_685 to a single candidate asks USDC to keep
    /// ⌈15_789_473_685 / 0.95⌉ and XLM to keep ⌈157_894_736_850 / 0.75⌉,
    /// both more than either position holds, and withdraws nothing at all.
    ///
    /// Charged correctly, the also-liability goes first (spec §5 step 3's
    /// order) and spends the whole 210_526_315: USDC must keep
    /// 4_750_000_000 − 210_526_315 = 4_539_473_685 of base, which is
    /// ⌈4_539_473_685 / 0.95⌉ = 4_778_393_353 b-tokens, so 221_606_647 goes
    /// and the projection lands on exactly 15_000_000. The primary is
    /// then inside the margin and nothing more is sent — the same
    /// 210_526_315 of base is worth ⌈110_394_736_850 / 0.75⌉ =
    /// 147_192_982_467 b-tokens kept and 2_807_017_533 XLM out, which a
    /// binary search over exact projections confirms is that candidate's
    /// maximum to the unit, but the order spends the headroom before the
    /// primary is reached.
    #[test]
    fn the_floor_is_charged_to_the_whole_position_not_to_one_asset() {
        let pool = pool();
        let terms = terms();
        let held = positions(
            &[(0, 150_000_000_000), (1, 5_000_000_000)],
            &[(1, 10_000_000_000)],
        );
        let empty = BTreeMap::new();
        let unwound = plan(&terms, &pool, &held, &empty);
        assert_eq!(unwound.actions, vec![withdraw(USDC, 221_606_647)]);
        assert_eq!(unwound.remaining_liabilities, vec![USDC.to_string()]);
        assert_eq!(unwound.projected_health, Some(15_000_000));

        // The same headroom, priced in the primary: what the reviewer's
        // figure names, and what step 3's order spends before it gets there.
        assert_eq!(
            largest_withdrawal(&terms, &inputs(&pool, &held, &empty), &held, 0),
            2_807_017_533
        );
        // And once the plan has spent it, nothing of the primary is left to
        // take.
        let end = apply(&pool, &held, unwound.actions.iter());
        assert_eq!(
            largest_withdrawal(&terms, &inputs(&pool, &end, &empty), &end, 0),
            0
        );
    }

    /// A wallet holding exactly the debt buys no headroom for `d_rate` to
    /// grow in, so the debt is not projected as cleared: step 3 runs, not
    /// step 2. The 1e10 repay leaves the position planned against its whole
    /// 1e10 of debt (liability base 10_526_315_790) with 19_750_000_000 of
    /// collateral base — a health factor of 18_762_499, clear of the margin
    /// — and the floor wants 15_789_473_685, so 3_960_526_315 of base may
    /// go. It comes from the also-liability: USDC keeps 4_750_000_000 −
    /// 3_960_526_315 = 789_473_685 of base, or ⌈789_473_685 / 0.95⌉ =
    /// 831_024_932 b-tokens, and 4_168_975_068 goes. The primary is left
    /// untouched at 20,000 XLM rather than withdrawn entirely.
    #[test]
    fn a_repay_with_no_allowance_left_does_not_clear_the_debt() {
        let pool = pool();
        let unwound = plan(
            &terms(),
            &pool,
            &positions(
                &[(0, 200_000_000_000), (1, 5_000_000_000)],
                &[(1, 10_000_000_000)],
            ),
            &wallet(&[(USDC, 10_000_000_000)]),
        );
        assert_eq!(
            unwound.actions,
            vec![repay(USDC, 10_000_000_000), withdraw(USDC, 4_168_975_068),]
        );
        assert_eq!(unwound.remaining_liabilities, vec![USDC.to_string()]);
        assert_eq!(unwound.projected_health, Some(15_000_000));

        // One stroop more in the wallet buys the allowance, and with it
        // step 2: the debt is cleared and everything but the floor goes.
        let funded = plan(
            &terms(),
            &pool,
            &positions(
                &[(0, 200_000_000_000), (1, 5_000_000_000)],
                &[(1, 10_000_000_000)],
            ),
            &wallet(&[(USDC, 10_001_000_001)]),
        );
        assert_eq!(
            funded.actions,
            vec![
                repay(USDC, 10_001_000_001),
                withdraw_all(USDC),
                withdraw(XLM, 100_000_000_000),
            ]
        );
        assert!(funded.remaining_liabilities.is_empty());
    }

    /// The dust rule binds the partial too, not only the excess. 12,070 XLM
    /// against 570 USDC of debt (liability base exactly 6e9) is a health
    /// factor of ⌊9_052_500_000 × 1e7 / 6e9⌋ = 15_087_500, just clear of the
    /// 15_075_000 margin, so the primary is a candidate. Its whole excess
    /// over the floor — 2.07e10, far above the dust rule — leaves 1e11 and a
    /// health factor of 1.25, which the floor refuses; the formula's answer
    /// is what the floor allows instead, ⌈9e9 × 10 / 0.75⌉ = 1.2e11 of
    /// b-tokens kept and 7e8 out, and that is under 1% of the floor. Nothing
    /// is sent.
    #[test]
    fn a_partial_of_the_primary_under_the_dust_rule_is_not_withdrawn() {
        let pool = pool();
        let unwound = plan(
            &terms(),
            &pool,
            &positions(&[(0, 120_700_000_000)], &[(1, 5_700_000_000)]),
            &BTreeMap::new(),
        );
        assert!(unwound.is_idle());
        assert_eq!(unwound.remaining_liabilities, vec![USDC.to_string()]);
        assert_eq!(unwound.projected_health, Some(15_087_500));
    }

    /// A zero floor is "withdraw everything": no liabilities, the primary
    /// entirely, as a `WithdrawAll`.
    #[test]
    fn a_zero_floor_withdraws_the_primary_entirely() {
        let pool = pool();
        let terms = UnwindTerms {
            min_primary_collateral: 0,
            ..terms()
        };
        let unwound = plan(
            &terms,
            &pool,
            &positions(&[(0, 200_000_000_000)], &[]),
            &BTreeMap::new(),
        );
        assert_eq!(unwound.actions, vec![withdraw_all(XLM)]);
    }

    /// Nothing to do: no positions, or only the primary at its floor.
    #[test]
    fn a_clean_position_is_an_idle_pass() {
        let pool = pool();
        assert!(plan(&terms(), &pool, &Positions::default(), &BTreeMap::new()).is_idle());
        assert!(plan(
            &terms(),
            &pool,
            &positions(&[(0, 100_000_000_000)], &[]),
            &BTreeMap::new()
        )
        .is_idle());
    }

    /// The repay never spends more than the wallet holds, and never repays
    /// an asset the wallet does not hold.
    #[test]
    fn a_repay_is_capped_at_the_wallet() {
        let pool = pool();
        let owing = positions(&[], &[(1, 10_000_000_000)]);
        let unwound = plan(&terms(), &pool, &owing, &wallet(&[(USDC, 1)]));
        assert_eq!(unwound.actions, vec![repay(USDC, 1)]);
        assert_eq!(unwound.spend, wallet(&[(USDC, 1)]));
        assert_eq!(unwound.remaining_liabilities, vec![USDC.to_string()]);

        let empty = plan(&terms(), &pool, &owing, &BTreeMap::new());
        assert!(empty.is_idle());
        assert!(empty.spend.is_empty());
        assert_eq!(empty.remaining_liabilities, vec![USDC.to_string()]);
    }

    /// The positions the contract is left with after `actions`: a repay
    /// burns `to_d_token_down(amount)` capped at the debt, a `Withdraw`
    /// burns `to_b_token_up(amount)` capped at the position, and a
    /// `WithdrawAll` removes the entry.
    fn apply<'a>(
        pool: &Pool,
        start: &Positions,
        actions: impl Iterator<Item = &'a UnwindAction>,
    ) -> Positions {
        let mut end = start.clone();
        for action in actions {
            let asset = match action {
                UnwindAction::Repay { asset, .. }
                | UnwindAction::Withdraw { asset, .. }
                | UnwindAction::WithdrawAll { asset } => asset,
            };
            let index = pool
                .asset_index
                .get(asset)
                .copied()
                .expect("a listed asset");
            let reserve = pool.reserves.get(&index).expect("a listed reserve");
            match action {
                UnwindAction::Repay { amount, .. } => {
                    let owing = end.liabilities.get(&index).copied().unwrap_or(0);
                    let left = owing
                        - reserve
                            .to_d_token_down(*amount)
                            .expect("d-tokens")
                            .min(owing);
                    if left > 0 {
                        end.liabilities.insert(index, left);
                    } else {
                        end.liabilities.remove(&index);
                    }
                }
                UnwindAction::Withdraw { amount, .. } => {
                    let held = end.collateral.get(&index).copied().unwrap_or(0);
                    let left = held - reserve.to_b_token_up(*amount).expect("b-tokens").min(held);
                    if left > 0 {
                        end.collateral.insert(index, left);
                    } else {
                        end.collateral.remove(&index);
                    }
                }
                UnwindAction::WithdrawAll { .. } => {
                    end.collateral.remove(&index);
                }
            }
        }
        end
    }

    /// One grid point of [`every_plan_holds_the_floors`]: plan it, apply
    /// the plan the way the contract would, and assert everything the
    /// builder promises of the result.
    fn holds_the_floors(
        terms: &UnwindTerms,
        pool: &Pool,
        start: &Positions,
        purse: &BTreeMap<String, i128>,
        case: &str,
    ) {
        let floor = terms.min_primary_collateral;
        let primary = pool.reserves.get(&0).expect("the primary reserve");
        let unwound = plan(terms, pool, start, purse);
        for (asset, spent) in &unwound.spend {
            assert!(
                *spent <= purse.get(asset).copied().unwrap_or(0),
                "{case}: spends {spent} of {asset}"
            );
        }
        let repaid = apply(
            pool,
            start,
            unwound
                .actions
                .iter()
                .filter(|action| matches!(action, UnwindAction::Repay { .. })),
        );
        let end = apply(
            pool,
            &repaid,
            unwound
                .actions
                .iter()
                .filter(|action| !matches!(action, UnwindAction::Repay { .. })),
        );
        let data = calculate_position_data(&pool.reserves, &pool.prices, &end).expect("values");
        assert!(
            end.liabilities.is_empty()
                || !data.is_hf_under(terms.min_health_factor).expect("hf")
                || end == repaid,
            "{case}: withdrew under the floor"
        );
        let before = primary
            .to_asset_from_b_token(start.collateral.get(&0).copied().unwrap_or(0))
            .expect("underlying");
        let after = primary
            .to_asset_from_b_token(end.collateral.get(&0).copied().unwrap_or(0))
            .expect("underlying");
        assert!(
            after >= floor || before < floor,
            "{case}: primary {before} → {after}, floor {floor}"
        );

        // A repay the wallet capped below its allowance is projected to
        // clear nothing, so in that one window the plan's model and the
        // contract's application above part company by construction: the
        // plan names a liability the contract cleared. The floors are
        // asserted on the contract's state either way, and it is strictly
        // the healthier of the two — the two clauses below compare
        // *models*, and
        // `a_repay_with_no_allowance_left_does_not_clear_the_debt` pins
        // that window exactly.
        if !unwound.remaining_liabilities.is_empty() && end.liabilities.is_empty() {
            return;
        }
        assert_eq!(
            unwound.projected_health,
            data.health_factor().expect("hf"),
            "{case}: the plan's projection is the end state's"
        );

        // Maximality. Without it "withdrew nothing" and "stopped at the
        // margin, correctly" are the same assertion, and a builder that
        // moves nothing passes every grid point. When the pass began with
        // room to move — a post-repay health factor at or above the margin
        // — it must have moved all of it: no further withdrawal of the
        // primary worth sending can still hold both floors.
        let started =
            calculate_position_data(&pool.reserves, &pool.prices, &repaid).expect("values");
        if !started.is_hf_under(margin(terms)).expect("hf") {
            let more = largest_withdrawal(terms, &inputs(pool, &end, purse), &end, 0);
            assert!(
                more < dust_floor(terms),
                "{case}: {more} more of the primary would still hold the floors"
            );
        }
    }

    /// Whatever the inputs, a plan holds the floors: re-projecting the
    /// plan's end state gives a health factor at or above the minimum (or
    /// no liabilities, or no withdrawal at all — a position already under
    /// the floor when the pass began is left exactly as the repays left
    /// it), and the primary at or above its floor whenever it began there;
    /// and where the pass had room to move, it moved all of it. A grid over
    /// positions and wallets, including rates that are not one.
    #[test]
    fn every_plan_holds_the_floors() {
        let terms = terms();
        for rate in [SCALAR_12, 1_000_022_300_000, 1_228_700_000_000] {
            let pool = pool_at(rate);
            for collateral in [
                100_000_000_000_i128,
                150_000_000_000,
                200_000_000_000,
                500_000_000_000,
            ] {
                for debt in [0_i128, 6_000_000_000, 10_000_000_000, 20_000_000_000] {
                    for supplied in [0_i128, 5_000_000_000] {
                        for held in [0_i128, 4_000_000_000, 20_000_000_000] {
                            let mut start = positions(&[(0, collateral)], &[]);
                            if supplied > 0 {
                                start.collateral.insert(1, supplied);
                            }
                            if debt > 0 {
                                start.liabilities.insert(1, debt);
                            }
                            let purse = if held > 0 {
                                wallet(&[(USDC, held)])
                            } else {
                                BTreeMap::new()
                            };
                            let case = format!(
                                "rate {rate}, collateral {collateral}, debt {debt}, supplied {supplied}, wallet {held}"
                            );
                            holds_the_floors(&terms, &pool, &start, &purse, &case);
                        }
                    }
                }
            }
        }
    }

    /// An asset the pool does not list is a bug upstream, not a plan.
    #[test]
    fn an_unknown_asset_is_refused() {
        let pool = pool();
        let empty = BTreeMap::new();
        let refused = |held: &Positions| plan_unwind(&terms(), &inputs(&pool, held, &empty));
        assert_eq!(
            refused(&positions(&[], &[(9, 1_000_000)])),
            Err(MathError::MissingReserve(9))
        );
        assert_eq!(
            refused(&positions(&[(9, 1_000_000)], &[])),
            Err(MathError::MissingReserve(9))
        );
        assert_eq!(
            refused(&positions(
                &[(0, 200_000_000_000), (9, 1_000_000)],
                &[(1, 6_000_000_000)]
            )),
            Err(MathError::MissingReserve(9))
        );
    }
}
