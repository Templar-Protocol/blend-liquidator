//! The filler's arithmetic: what an auction is worth, the ledger to fill
//! it at, and — in `plan_fill` — the requests that keep the filler's own
//! position at or above its floor while it takes the auction over. Pure:
//! no I/O, and nothing panics.
//!
//! Values are in the pool oracle's units (`OraclePrices::scalar`), the
//! units `PositionData` reports. *Raw* values are what the filler is paid
//! and pays; *effective* values, after collateral and liability factors,
//! are what the contract's health check reads.

use std::collections::BTreeMap;

use ethnum::I256;

use super::auction::{bid_modifier, lot_modifier, AuctionData, RAMP_BLOCKS, RAMP_END_BLOCKS};
use super::fixed::{mul_ceil, mul_floor, MathError, SCALAR_7};
use super::position::Positions;

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
