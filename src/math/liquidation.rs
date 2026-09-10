//! The auctioneer's arithmetic: what an auction should name, and how much
//! of the borrower's liability it should take.
//!
//! Pure, like the rest of `math`: no I/O, no panics, every operation
//! checked. The selection is the port of the reference auctioneer's, and
//! the spec's §4 is its statement; the numbers here are 7-decimal fixed
//! point except the per-position values, which are in the oracle's own
//! scale, as `PositionData` is.

use std::collections::BTreeMap;

use crate::chain::xdr::encode::FillPercent;
use crate::math::fixed::{div_floor, mul_floor, SCALAR_7};
use crate::math::position::{OraclePrices, PositionData, Positions};
use crate::math::reserve::Reserve;
use crate::math::MathError;

/// One of a borrower's positions, valued at a ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionValue {
    /// The reserve's index, the key `Positions` uses.
    pub index: u32,
    /// The reserve's asset contract, which is what an auction names.
    pub asset: String,
    /// Value before the collateral or liability factor, in oracle scale.
    pub raw: i128,
    /// Value after that factor, in oracle scale.
    pub effective: i128,
}

/// The auction to create: which liabilities it takes over, which collateral
/// it pays out, and what share of the liability it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidationPlan {
    /// Liability assets, the auction's bid.
    pub bid: Vec<String>,
    /// Collateral assets, the auction's lot.
    pub lot: Vec<String>,
    /// The share of the selected liabilities, 1 to 100.
    pub percent: FillPercent,
}

/// Values every non-zero position, collateral and liabilities separately,
/// each sorted by effective value, largest first — the order the selection
/// walks.
///
/// `reserves` must already be accrued to the ledger being valued;
/// [`PoolSnapshot::position_data`](crate::chain::pool::PoolSnapshot::position_data)
/// accrues a clone before it values, and a caller of this function must do
/// the same or the values disagree with the health factor they are
/// compared against.
pub fn position_values(
    reserves: &BTreeMap<u32, Reserve>,
    prices: &OraclePrices,
    positions: &Positions,
) -> Result<(Vec<PositionValue>, Vec<PositionValue>), MathError> {
    let mut collateral = Vec::new();
    for (index, b_tokens) in &positions.collateral {
        if *b_tokens == 0 {
            continue;
        }
        let reserve = reserves
            .get(index)
            .ok_or(MathError::MissingReserve(*index))?;
        let price = prices.price(&reserve.asset)?;
        collateral.push(PositionValue {
            index: *index,
            asset: reserve.asset.clone(),
            raw: mul_floor(
                price,
                reserve.to_asset_from_b_token(*b_tokens)?,
                reserve.scalar,
            )?,
            effective: mul_floor(
                price,
                reserve.to_effective_asset_from_b_token(*b_tokens)?,
                reserve.scalar,
            )?,
        });
    }
    let mut liabilities = Vec::new();
    for (index, d_tokens) in &positions.liabilities {
        if *d_tokens == 0 {
            continue;
        }
        let reserve = reserves
            .get(index)
            .ok_or(MathError::MissingReserve(*index))?;
        let price = prices.price(&reserve.asset)?;
        liabilities.push(PositionValue {
            index: *index,
            asset: reserve.asset.clone(),
            raw: mul_floor(
                price,
                reserve.to_asset_from_d_token(*d_tokens)?,
                reserve.scalar,
            )?,
            effective: mul_floor(
                price,
                reserve.to_effective_asset_from_d_token(*d_tokens)?,
                reserve.scalar,
            )?,
        });
    }
    // Largest first, with the reserve index breaking a tie so the order is
    // total: two positions of equal value must not reorder between calls,
    // or the same borrower yields a different auction on each recheck.
    collateral.sort_by(|left, right| {
        right
            .effective
            .cmp(&left.effective)
            .then(left.index.cmp(&right.index))
    });
    liabilities.sort_by(|left, right| {
        right
            .effective
            .cmp(&left.effective)
            .then(left.index.cmp(&right.index))
    });
    Ok((collateral, liabilities))
}

/// The auction for `data`, or `None` when there is nothing to close.
///
/// `target_health` is `TARGET_HF` in 7 decimals: the health factor the
/// liquidation aims to leave the borrower at, above the contract's own
/// `1_030_0000` floor and below its `1_150_0000` ceiling so the created
/// auction is one the contract accepts. `max_positions` is the pool's cap
/// on the assets one auction may name.
///
/// The selection walks both sides largest first: a percent over 100 means
/// the selected liabilities cannot close the excess, so the next liability
/// joins; a percent of zero means the selected collateral cannot cover what
/// the auction would withdraw, so the next collateral joins. When neither
/// side has anything left to add, the answer is the whole position.
pub fn plan_liquidation(
    data: &PositionData,
    collateral: &[PositionValue],
    liabilities: &[PositionValue],
    target_health: i128,
    max_positions: u32,
) -> Result<Option<LiquidationPlan>, MathError> {
    if liabilities.is_empty() || collateral.is_empty() {
        return Ok(None);
    }
    // excess = effective liabilities × TARGET_HF − effective collateral.
    let excess = mul_floor(data.liability_base, target_health, SCALAR_7)?
        .checked_sub(data.collateral_base)
        .ok_or(MathError::Overflow)?;
    if excess <= 0 {
        return Ok(None);
    }

    let mut lot_count = 1usize;
    let mut bid_count = 1usize;
    loop {
        let selected_collateral = &collateral[..lot_count];
        let selected_liabilities = &liabilities[..bid_count];
        let percent = selection_percent(
            excess,
            selected_collateral,
            selected_liabilities,
            target_health,
        )?;

        if percent > 100 {
            // The selected liabilities cannot close the excess.
            if bid_count < liabilities.len() {
                bid_count += 1;
                continue;
            }
            if lot_count < collateral.len() {
                lot_count += 1;
                continue;
            }
        } else if percent == 0 {
            // The selected collateral cannot cover the withdrawal.
            if lot_count < collateral.len() {
                lot_count += 1;
                continue;
            }
        } else {
            return Ok(Some(bounded_plan(
                selected_collateral,
                selected_liabilities,
                percent,
                max_positions,
            )?));
        }

        // Both sides are exhausted: take everything. This is also what the
        // contract does for a percent over 95 with every position included.
        return Ok(Some(bounded_plan(
            collateral,
            liabilities,
            100,
            max_positions,
        )?));
    }
}

/// The percent for one selection, or `0` when the selected collateral
/// cannot cover what the auction would withdraw. May exceed 100, which the
/// caller reads as "these liabilities are not enough".
fn selection_percent(
    excess: i128,
    collateral: &[PositionValue],
    liabilities: &[PositionValue],
    target_health: i128,
) -> Result<u32, MathError> {
    let (raw_collateral, effective_collateral) = totals(collateral)?;
    let (raw_liabilities, effective_liabilities) = totals(liabilities)?;
    if raw_collateral == 0 || raw_liabilities == 0 || effective_liabilities == 0 {
        return Ok(0);
    }
    // cf = effective collateral / raw collateral; lf = effective liability
    // / raw liability. Both 7-decimal ratios.
    let cf = div_floor(effective_collateral, raw_collateral, SCALAR_7)?;
    let lf = div_floor(effective_liabilities, raw_liabilities, SCALAR_7)?;
    if cf == 0 || lf == 0 {
        return Ok(0);
    }
    // incentive = 1 + (1 − cf / lf) / 2
    let ratio = div_floor(cf, lf, SCALAR_7)?;
    let incentive = SCALAR_7
        .checked_add(
            SCALAR_7
                .checked_sub(ratio)
                .ok_or(MathError::Overflow)?
                .checked_div(2)
                .ok_or(MathError::Overflow)?,
        )
        .ok_or(MathError::Overflow)?;
    // recovered = lf × TARGET_HF − incentive × cf, the borrow limit
    // regained per unit of raw liability taken over.
    let recovered = mul_floor(lf, target_health, SCALAR_7)?
        .checked_sub(mul_floor(incentive, cf, SCALAR_7)?)
        .ok_or(MathError::Overflow)?;
    if recovered <= 0 {
        // The incentive eats the whole recovery: this selection closes
        // nothing, so more collateral will not help and more liability
        // must. 101 is not a percent — it is the sentinel `plan_liquidation`
        // reads as "these liabilities cannot close the excess, add
        // another"; the caller never treats it as a fill share.
        return Ok(101);
    }
    // percent = excess / (recovered × raw_liabilities) × 100
    let denominator = mul_floor(recovered, raw_liabilities, SCALAR_7)?;
    if denominator <= 0 {
        return Ok(101);
    }
    let percent = div_floor(
        excess.checked_mul(100).ok_or(MathError::Overflow)?,
        denominator,
        1,
    )?;
    let percent = u32::try_from(percent).unwrap_or(u32::MAX);
    if percent > 100 {
        return Ok(percent);
    }
    // The collateral the auction would withdraw:
    // raw_liabilities × percent/100 × incentive.
    let taken = mul_floor(raw_liabilities, i128::from(percent), 100)?;
    let withdrawn = mul_floor(taken, incentive, SCALAR_7)?;
    if withdrawn > raw_collateral {
        return Ok(0);
    }
    Ok(percent.max(1))
}

/// Raw and effective totals of a selection.
fn totals(values: &[PositionValue]) -> Result<(i128, i128), MathError> {
    let mut raw = 0i128;
    let mut effective = 0i128;
    for value in values {
        raw = raw.checked_add(value.raw).ok_or(MathError::Overflow)?;
        effective = effective
            .checked_add(value.effective)
            .ok_or(MathError::Overflow)?;
    }
    Ok((raw, effective))
}

/// The plan, with the asset lists trimmed to the pool's cap. The largest
/// positions are kept: a submission naming more assets than the pool allows
/// is refused outright, and dropping the smallest costs the least.
///
/// Both sides keep at least one asset — an auction with an empty bid or lot
/// is `InvalidBid` or `InvalidLot` — so a cap below 2 cannot be honoured and
/// is treated as 2.
fn bounded_plan(
    collateral: &[PositionValue],
    liabilities: &[PositionValue],
    percent: u32,
    max_positions: u32,
) -> Result<LiquidationPlan, MathError> {
    let cap = usize::try_from(max_positions).unwrap_or(usize::MAX).max(2);
    let mut lot_count = collateral.len();
    let mut bid_count = liabilities.len();
    while lot_count + bid_count > cap {
        // Drop from whichever side has more to give, the smallest first,
        // never below one.
        if lot_count >= bid_count && lot_count > 1 {
            lot_count -= 1;
        } else if bid_count > 1 {
            bid_count -= 1;
        } else {
            break;
        }
    }
    Ok(LiquidationPlan {
        lot: collateral[..lot_count]
            .iter()
            .map(|value| value.asset.clone())
            .collect(),
        bid: liabilities[..bid_count]
            .iter()
            .map(|value| value.asset.clone())
            .collect(),
        percent: FillPercent::try_from(percent.clamp(1, 100))
            .map_err(|_| MathError::InvalidInput("a percent outside 1..=100"))?,
    })
}

#[cfg(test)]
mod tests_support {
    //! Fixtures shared by this module's tests, built the way
    //! `math::position`'s own tests build a `Reserve`
    //! (`math::position::tests::reserve`): 7 decimals, rate 1.0 on both
    //! sides so token amounts equal underlying exactly, with no rounding
    //! from the rate.

    use std::collections::BTreeMap;

    use crate::math::fixed::SCALAR_7;
    use crate::math::position::{OraclePrices, Positions};
    use crate::math::reserve::{Reserve, ReserveConfig, ReserveData};

    /// A reserve at 7 decimals, rate 1.0, with the given collateral and
    /// liability factors — the shape `math::position::tests::reserve` uses,
    /// parameterised so collateral and liability reserves can carry
    /// different factors.
    fn reserve(index: u32, asset: &str, c_factor: u32, l_factor: u32) -> Reserve {
        let config = ReserveConfig {
            index,
            decimals: 7,
            c_factor,
            l_factor,
            util: 4_000_000,
            max_util: 7_000_000,
            r_base: 100_000,
            r_one: 300_000,
            r_two: 3_000_000,
            r_three: 50_000_000,
            reactivity: 50,
            supply_cap: 100_000_000_000_000_000,
            enabled: true,
        };
        let data = ReserveData {
            d_rate: 1_000_000_000_000,
            b_rate: 1_000_000_000_000,
            ir_mod: SCALAR_7,
            b_supply: 1_000_000_000,
            d_supply: 500_000_000,
            backstop_credit: 0,
            last_time: 0,
        };
        Reserve::new(asset.to_string(), config, data).expect("7 decimals fit")
    }

    /// Two collateral reserves and two liability reserves, each pair with
    /// different factors and different prices, and one position on each —
    /// deliberately keyed so ascending reserve-index order disagrees with
    /// descending effective-value order, so a test against this fixture
    /// proves `position_values` actually sorts rather than passing through
    /// `BTreeMap`'s key order.
    pub(super) fn two_of_each() -> (BTreeMap<u32, Reserve>, OraclePrices, Positions) {
        let reserves = BTreeMap::from([
            (0, reserve(0, "XLM", 5_000_000, 7_500_000)),
            (1, reserve(1, "BTC", 9_000_000, 7_500_000)),
            (2, reserve(2, "USDC", 7_500_000, 8_000_000)),
            (3, reserve(3, "EURC", 7_500_000, 4_000_000)),
        ]);
        let mut price_map = BTreeMap::new();
        price_map.insert("XLM".to_string(), 10_000_000); // 1.0
        price_map.insert("BTC".to_string(), 20_000_000); // 2.0
        price_map.insert("USDC".to_string(), 10_000_000); // 1.0
        price_map.insert("EURC".to_string(), 15_000_000); // 1.5
        let prices = OraclePrices::new(7, price_map).expect("7 decimals fit");

        let positions = Positions {
            collateral: BTreeMap::from([
                (0, 2_000_000_000),  // 200 b-tokens, effective 100
                (1, 10_000_000_000), // 1000 b-tokens, effective 1800
            ]),
            liabilities: BTreeMap::from([
                (2, 4_000_000_000), // 400 d-tokens, effective 500
                (3, 2_000_000_000), // 200 d-tokens, effective 750
            ]),
            supply: BTreeMap::new(),
        };
        (reserves, prices, positions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One collateral, one liability, 7-decimal oracle. The values are
    /// chosen so the arithmetic is checkable by hand:
    /// collateral raw 1_000, effective 750 (c_factor 0.75);
    /// liability raw 800, effective 1_000 (l_factor 0.8).
    fn one_of_each() -> (PositionData, Vec<PositionValue>, Vec<PositionValue>) {
        let data = PositionData {
            collateral_base: 7_500_000_000,
            collateral_raw: 10_000_000_000,
            liability_base: 10_000_000_000,
            liability_raw: 8_000_000_000,
            scalar: SCALAR_7,
        };
        let collateral = vec![PositionValue {
            index: 0,
            asset: "XLM".to_string(),
            raw: 10_000_000_000,
            effective: 7_500_000_000,
        }];
        let liabilities = vec![PositionValue {
            index: 1,
            asset: "USDC".to_string(),
            raw: 8_000_000_000,
            effective: 10_000_000_000,
        }];
        (data, collateral, liabilities)
    }

    /// A healthy borrower has no excess to close, so there is no plan.
    /// `excess = effective_liabilities × TARGET_HF − effective_collateral`,
    /// and a borrower whose collateral already exceeds that is not one the
    /// auctioneer has anything to do about.
    #[test]
    fn a_position_with_no_excess_has_no_plan() {
        let (mut data, collateral, liabilities) = one_of_each();
        data.collateral_base = 20_000_000_000;
        let plan = plan_liquidation(&data, &collateral, &liabilities, 10_600_000, 8).expect("plan");
        assert!(
            plan.is_none(),
            "collateral above the target leaves nothing to close"
        );
    }

    /// The worked case. With one position of each:
    ///   excess    = 1_000 × 1.06 − 750 = 310
    ///   cf        = 750 / 1_000 = 0.75
    ///   lf        = 1_000 / 800 = 1.25
    ///   incentive = 1 + (1 − 0.75/1.25) / 2 = 1.2
    ///   recovered = 1.25 × 1.06 − 1.2 × 0.75 = 1.325 − 0.9 = 0.425
    ///   percent   = 310 / (0.425 × 800) × 100 = 91.17… → 91
    /// and the withdrawn collateral, 800 × 0.91 × 1.2 = 873.6, is under the
    /// 1_000 of collateral selected, so the percent stands.
    #[test]
    fn the_worked_example_selects_ninety_one_percent() {
        let (data, collateral, liabilities) = one_of_each();
        let plan = plan_liquidation(&data, &collateral, &liabilities, 10_600_000, 8)
            .expect("plan")
            .expect("a liquidatable position has a plan");
        assert_eq!(plan.percent.get(), 91);
        assert_eq!(
            plan.bid,
            vec!["USDC".to_string()],
            "the bid is the liability taken over"
        );
        assert_eq!(
            plan.lot,
            vec!["XLM".to_string()],
            "the lot is the collateral received"
        );
    }

    /// A percent over 100 means one liability is not enough to close the
    /// excess, so the next largest liability joins the bid. Two liabilities
    /// of 400 raw each cannot be closed by the first alone.
    #[test]
    fn a_percent_over_one_hundred_adds_the_next_liability() {
        let data = PositionData {
            collateral_base: 1_000_000_000,
            collateral_raw: 1_333_333_333,
            liability_base: 10_000_000_000,
            liability_raw: 8_000_000_000,
            scalar: SCALAR_7,
        };
        let collateral = vec![PositionValue {
            index: 0,
            asset: "XLM".to_string(),
            raw: 1_333_333_333,
            effective: 1_000_000_000,
        }];
        let liabilities = vec![
            PositionValue {
                index: 1,
                asset: "USDC".to_string(),
                raw: 4_000_000_000,
                effective: 5_000_000_000,
            },
            PositionValue {
                index: 2,
                asset: "EURC".to_string(),
                raw: 4_000_000_000,
                effective: 5_000_000_000,
            },
        ];
        let plan = plan_liquidation(&data, &collateral, &liabilities, 10_600_000, 8)
            .expect("plan")
            .expect("a plan");
        assert_eq!(
            plan.bid.len(),
            2,
            "one liability could not close the excess"
        );
        assert!(
            plan.percent.get() <= 100,
            "the answer is always a percent the contract accepts"
        );
    }

    /// When the selected collateral cannot cover what the auction would
    /// withdraw, the percent is zero and the next largest collateral joins
    /// the lot rather than the plan being abandoned.
    #[test]
    fn an_underweight_lot_adds_the_next_collateral() {
        let data = PositionData {
            collateral_base: 7_500_000_000,
            collateral_raw: 10_000_000_000,
            liability_base: 10_000_000_000,
            liability_raw: 8_000_000_000,
            scalar: SCALAR_7,
        };
        let collateral = vec![
            PositionValue {
                index: 0,
                asset: "XLM".to_string(),
                raw: 5_000_000_000,
                effective: 3_750_000_000,
            },
            PositionValue {
                index: 3,
                asset: "BTC".to_string(),
                raw: 5_000_000_000,
                effective: 3_750_000_000,
            },
        ];
        let liabilities = vec![PositionValue {
            index: 1,
            asset: "USDC".to_string(),
            raw: 8_000_000_000,
            effective: 10_000_000_000,
        }];
        let plan = plan_liquidation(&data, &collateral, &liabilities, 10_600_000, 8)
            .expect("plan")
            .expect("a plan");
        assert_eq!(
            plan.lot.len(),
            2,
            "one collateral could not cover the withdrawal"
        );
    }

    /// With both sides exhausted the answer is the whole position at 100%,
    /// which is what the contract itself does for `percent > 95` with every
    /// position included.
    #[test]
    fn exhausting_both_sides_takes_everything() {
        let data = PositionData {
            collateral_base: 10_000_000,
            collateral_raw: 13_333_333,
            liability_base: 10_000_000_000,
            liability_raw: 8_000_000_000,
            scalar: SCALAR_7,
        };
        let collateral = vec![PositionValue {
            index: 0,
            asset: "XLM".to_string(),
            raw: 13_333_333,
            effective: 10_000_000,
        }];
        let liabilities = vec![PositionValue {
            index: 1,
            asset: "USDC".to_string(),
            raw: 8_000_000_000,
            effective: 10_000_000_000,
        }];
        let plan = plan_liquidation(&data, &collateral, &liabilities, 10_600_000, 8)
            .expect("plan")
            .expect("a plan");
        assert_eq!(plan.percent.get(), 100);
        assert_eq!(plan.bid.len(), 1);
        assert_eq!(plan.lot.len(), 1);
    }

    /// The pool caps how many assets one auction may name. When the
    /// selection would exceed it the largest are kept: dropping a small
    /// position costs less than a submission the contract refuses with
    /// `MaxPositionsExceeded`.
    #[test]
    fn the_asset_lists_respect_max_positions() {
        let data = PositionData {
            collateral_base: 1_000_000_000,
            collateral_raw: 1_333_333_333,
            liability_base: 10_000_000_000,
            liability_raw: 8_000_000_000,
            scalar: SCALAR_7,
        };
        let collateral: Vec<PositionValue> = (0..4)
            .map(|index| PositionValue {
                index,
                asset: format!("C{index}"),
                raw: 333_333_333,
                effective: 250_000_000,
            })
            .collect();
        let liabilities: Vec<PositionValue> = (10..14)
            .map(|index| PositionValue {
                index,
                asset: format!("L{index}"),
                raw: 2_000_000_000,
                effective: 2_500_000_000,
            })
            .collect();
        let plan = plan_liquidation(&data, &collateral, &liabilities, 10_600_000, 3)
            .expect("plan")
            .expect("a plan");
        assert!(
            plan.bid.len() + plan.lot.len() <= 3,
            "the pool's cap is not exceeded"
        );
        assert!(
            !plan.bid.is_empty() && !plan.lot.is_empty(),
            "an auction needs both sides"
        );
    }

    /// A borrower with no liabilities is not liquidatable at all, and the
    /// division that would compute a percent has no meaning: the answer is
    /// no plan, not an error and not a zero percent.
    #[test]
    fn no_liabilities_is_no_plan() {
        let data = PositionData {
            collateral_base: 1_000_000_000,
            collateral_raw: 1_000_000_000,
            liability_base: 0,
            liability_raw: 0,
            scalar: SCALAR_7,
        };
        assert!(plan_liquidation(&data, &[], &[], 10_600_000, 8)
            .expect("plan")
            .is_none());
    }

    /// Positions are valued per reserve and sorted by effective value, so
    /// the selection starts with the largest of each side. This is the step
    /// the whole algorithm's shape depends on.
    #[test]
    fn positions_are_valued_and_sorted_by_effective_value() {
        // Built from the same reserve helpers `math::position`'s tests use.
        let (reserves, prices, positions) = super::tests_support::two_of_each();
        let (collateral, liabilities) =
            position_values(&reserves, &prices, &positions).expect("values");
        assert!(
            collateral
                .windows(2)
                .all(|pair| pair[0].effective >= pair[1].effective),
            "collateral is sorted largest first"
        );
        assert!(
            liabilities
                .windows(2)
                .all(|pair| pair[0].effective >= pair[1].effective),
            "liabilities are sorted largest first"
        );
        assert!(
            collateral
                .iter()
                .all(|value| value.raw > 0 && value.effective > 0),
            "a zero position is not a position"
        );
    }
}
