//! The ADR-0008 fork's default path, ported.
//!
//! When a fill takes 100% of a liquidation auction the contract runs
//! `check_and_handle_user_bad_debt` over the borrower *inside the
//! filler's own transaction*, before `validate_submit` checks the
//! filler's health (`pool/src/auctions/user_liquidation_auction.rs:206-222`).
//! That path can reduce a reserve's `b_rate`, which makes every holder
//! of collateral in that reserve — the filler included — worth less at
//! check time than the pre-fill snapshot said.
//!
//! This module answers what the reserves look like afterwards, so
//! [`crate::math::fill::plan_fill`] can project the filler's position
//! against them rather than against the snapshot it read. Nothing here
//! does I/O and nothing panics.

use std::collections::BTreeMap;

use super::fixed::{div_ceil, MathError, SCALAR_12};
use super::position::{calculate_position_data, OraclePrices, Positions};
use super::reserve::Reserve;

/// The pool's reserves after the contract's default path has run over one
/// borrower, and whether it defaulted anything at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultOutcome {
    /// A clone of the caller's reserves with any `b_rate` haircut
    /// applied. The caller's own map is never mutated.
    pub reserves: BTreeMap<u32, Reserve>,
    /// Whether step 2 destroyed any debt. False when the gate did not
    /// open, and false when step 1's set-off cleared every liability
    /// from the borrower's own supply — in which case no `b_rate` moved
    /// and `reserves` is an unchanged clone.
    pub defaulted: bool,
}

/// What the contract's `check_and_handle_user_bad_debt` would do to the
/// pool's reserves, given the borrower's positions *after* a fill.
///
/// The gate is the contract's, read the same way: the borrower must have
/// liabilities and `collateral_raw == 0`
/// (`pool/src/pool/bad_debt.rs:56-58`). Below it, per liability asset in
/// reserve-index order:
///
/// 1. **Set-off.** The borrower's own `supply` in that reserve repays
///    what it covers, burning b-tokens. This lowers `b_supply`, which
///    step 2 then divides by — so the order is not cosmetic: computing
///    the haircut against the pre-set-off `b_supply` understates it, and
///    an understated haircut is exactly the optimistic projection that
///    earns `InvalidHf` from the contract.
/// 2. **Default.** Whatever debt remains is destroyed and the reserve's
///    `b_rate` falls by `ceil(default_amount × SCALAR_12 / b_supply)`,
///    floored at zero (`pool/src/pool/user.rs:102-119`). A reserve with
///    no b-tokens left has nobody to charge, so the debt is destroyed
///    and no rate moves.
///
/// Confiscation — the contract's third step — moves the borrower's
/// remaining collateral to the pool's own address. It is not modelled
/// because it cannot run: the gate required `collateral_raw == 0`, and
/// it changes no reserve rate even when it does.
///
/// # Errors
///
/// `MathError::MissingReserve` when a liability names an index this pool
/// has no reserve for, `MathError::MissingPrice` when the oracle snapshot
/// cannot price one the gate must value, and `MathError::Overflow` or
/// `DivisionByZero` from the conversions themselves. None is ever read as
/// a zero: an incomplete view of the pool cannot be projected against.
pub fn project_default(
    reserves: &BTreeMap<u32, Reserve>,
    prices: &OraclePrices,
    borrower: &Positions,
) -> Result<DefaultOutcome, MathError> {
    let mut reserves = reserves.clone();
    if borrower.liabilities.is_empty() {
        return Ok(DefaultOutcome {
            reserves,
            defaulted: false,
        });
    }
    let data = calculate_position_data(&reserves, prices, borrower)?;
    if data.collateral_raw != 0 {
        return Ok(DefaultOutcome {
            reserves,
            defaulted: false,
        });
    }

    let mut defaulted_any = false;
    for (index, d_tokens) in &borrower.liabilities {
        let Some(reserve) = reserves.get_mut(index) else {
            return Err(MathError::MissingReserve(*index));
        };
        let mut defaulted = *d_tokens;

        // Step 1: set-off from the borrower's own supply. A `b_rate` of
        // zero is not a division this may attempt, and leaves nothing for
        // the supply to be worth anyway.
        let claim = borrower.supply.get(index).copied().unwrap_or(0);
        if claim > 0 && reserve.data.b_rate > 0 {
            let debt_assets = reserve.to_asset_from_d_token(*d_tokens)?;
            let b_tokens = claim.min(reserve.to_b_token_up(debt_assets)?);
            let covered = reserve.to_asset_from_b_token(b_tokens)?;
            let repaid = (*d_tokens).min(reserve.to_d_token_down(covered)?);
            if repaid > 0 {
                // `remove_supply` burns the b-tokens: `b_supply` falls
                // before step 2 divides by it.
                reserve.data.b_supply = reserve
                    .data
                    .b_supply
                    .checked_sub(b_tokens)
                    .ok_or(MathError::Overflow)?;
                reserve.data.d_supply = reserve
                    .data
                    .d_supply
                    .checked_sub(repaid)
                    .ok_or(MathError::Overflow)?;
                defaulted = defaulted.checked_sub(repaid).ok_or(MathError::Overflow)?;
            }
        }

        // Step 2: destroy the rest, and charge every supplier for it.
        if defaulted > 0 {
            reserve.data.d_supply = reserve
                .data
                .d_supply
                .checked_sub(defaulted)
                .ok_or(MathError::Overflow)?;
            if reserve.data.b_supply > 0 {
                let default_amount = reserve.to_asset_from_d_token(defaulted)?;
                let loss = div_ceil(default_amount, reserve.data.b_supply, SCALAR_12)?;
                reserve.data.b_rate = reserve
                    .data
                    .b_rate
                    .checked_sub(loss)
                    .ok_or(MathError::Overflow)?
                    .max(0);
            }
            defaulted_any = true;
        }
    }
    Ok(DefaultOutcome {
        reserves,
        defaulted: defaulted_any,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::fixed::SCALAR_7;
    use crate::math::reserve::{ReserveConfig, ReserveData};

    const ASSET_A: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const ASSET_B: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    /// 7 decimals, both rates 1.1, both factors 0.75 — the fixture shape
    /// `math::position`'s and `math::reserve`'s own tests already use,
    /// with the two supplies each case needs to pin left to the caller.
    fn reserve(index: u32, asset: &str, b_supply: i128, d_supply: i128) -> Reserve {
        let config = ReserveConfig {
            index,
            decimals: 7,
            c_factor: 7_500_000,
            l_factor: 7_500_000,
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
            d_rate: 1_100_000_000_000,
            b_rate: 1_100_000_000_000,
            ir_mod: SCALAR_7,
            b_supply,
            d_supply,
            backstop_credit: 0,
            last_time: 0,
        };
        Reserve::new(asset.to_string(), config, data).expect("7 decimals fit")
    }

    /// Reserve 0 is the debt reserve every case below borrows from;
    /// reserve 1 exists so a collateral position can live somewhere else.
    fn two_reserves(b_supply: i128, d_supply: i128) -> BTreeMap<u32, Reserve> {
        BTreeMap::from([
            (0, reserve(0, ASSET_A, b_supply, d_supply)),
            (1, reserve(1, ASSET_B, 1_000_000_000, 500_000_000)),
        ])
    }

    /// Oracle at 7 decimals with both assets priced at 2.0.
    fn prices() -> OraclePrices {
        let mut map = BTreeMap::new();
        map.insert(ASSET_A.to_string(), 20_000_000);
        map.insert(ASSET_B.to_string(), 20_000_000);
        OraclePrices::new(7, map).expect("7 decimals fit")
    }

    fn positions(
        collateral: &[(u32, i128)],
        liabilities: &[(u32, i128)],
        supply: &[(u32, i128)],
    ) -> Positions {
        Positions {
            collateral: collateral.iter().copied().collect(),
            liabilities: liabilities.iter().copied().collect(),
            supply: supply.iter().copied().collect(),
        }
    }

    /// The gate: liabilities *and* zero raw collateral, both.
    #[test]
    fn the_gate_needs_liabilities_and_no_raw_collateral() {
        let reserves = two_reserves(1_000_000, 500_000);

        // (a) Collateral remaining. 200_000 b-tokens of reserve 1 at
        // b_rate 1.1 are 220_000 assets, priced at 2.0 for a
        // `collateral_raw` of 440_000 — the contract returns early.
        let with_collateral = positions(&[(1, 200_000)], &[(0, 100_000)], &[]);
        let outcome = project_default(&reserves, &prices(), &with_collateral).expect("projects");
        assert!(!outcome.defaulted);
        assert_eq!(outcome.reserves, reserves);

        // (b) No liabilities at all: nothing to set off and nothing to
        // default, whatever the borrower still holds.
        let no_debt = positions(&[], &[], &[(0, 400_000)]);
        let outcome = project_default(&reserves, &prices(), &no_debt).expect("projects");
        assert!(!outcome.defaulted);
        assert_eq!(outcome.reserves, reserves);
    }

    /// Step 1 alone: the borrower's own supply in the debt reserve
    /// covers the whole debt, so nothing is destroyed and no `b_rate`
    /// moves. This is the case the cheap fallback predicate would
    /// wrongly decline, and the reason modelling is worth its cost.
    #[test]
    fn a_setoff_that_clears_the_debt_moves_no_b_rate() {
        let reserves = two_reserves(1_000_000, 500_000);
        // 100_000 d-tokens at d_rate 1.1 owe 110_000 assets, which is
        // 100_000 b-tokens at b_rate 1.1 — well under the 500_000 the
        // borrower supplies.
        let borrower = positions(&[], &[(0, 100_000)], &[(0, 500_000)]);
        let outcome = project_default(&reserves, &prices(), &borrower).expect("projects");

        assert!(!outcome.defaulted);
        let after = &outcome.reserves[&0];
        assert_eq!(after.data.b_rate, reserves[&0].data.b_rate);
        // The set-off still happened: the b-tokens were burnt and the
        // debt repaid, they simply cost no other supplier anything.
        assert_eq!(after.data.b_supply, 900_000);
        assert_eq!(after.data.d_supply, 400_000);
    }

    /// Step 2: debt beyond the set-off is destroyed and every supplier
    /// of that reserve pays for it through `b_rate`.
    #[test]
    fn a_residual_default_cuts_b_rate_for_every_supplier() {
        let reserves = two_reserves(1_000_000, 500_000);
        let borrower = positions(&[], &[(0, 100_000)], &[]);
        let outcome = project_default(&reserves, &prices(), &borrower).expect("projects");

        assert!(outcome.defaulted);
        let after = &outcome.reserves[&0];
        assert!(after.data.b_rate < reserves[&0].data.b_rate);
        assert_eq!(after.data.d_supply, 400_000);
        // Untouched reserves come back exactly as they went in.
        assert_eq!(outcome.reserves[&1], reserves[&1]);
    }

    /// The haircut rounds **up** and floors at zero, exactly as
    /// `fixed_div_ceil` and the contract's own clamp do.
    #[test]
    fn the_haircut_rounds_up_and_floors_at_zero() {
        // Case 1: a partial set-off, then a residual default.
        //
        // The borrower owes 100_000 d-tokens — 110_000 assets at d_rate
        // 1.1 — and supplies 30_000 b-tokens, worth 33_000 assets at
        // b_rate 1.1, which repays 33_000 / 1.1 = 30_000 d-tokens.
        // So 30_000 b-tokens burn: `b_supply` is 3_030_000 - 30_000 =
        // 3_000_000 when step 2 divides by it, and 70_000 d-tokens
        // default, worth ceil(70_000 x 1.1) = 77_000 assets.
        //
        // 77_000 x 1e12 = 77_000_000_000_000_000, and
        // 3_000_000 x 25_666_666_666 = 76_999_999_998_000_000, leaving
        // 2_000_000 over — so the ceiling is 25_666_666_667 and the rate
        // lands at 1_100_000_000_000 - 25_666_666_667.
        let reserves = two_reserves(3_030_000, 500_000);
        let borrower = positions(&[], &[(0, 100_000)], &[(0, 30_000)]);
        let outcome = project_default(&reserves, &prices(), &borrower).expect("projects");

        assert!(outcome.defaulted);
        let after = &outcome.reserves[&0];
        assert_eq!(after.data.b_supply, 3_000_000);
        assert_eq!(after.data.d_supply, 400_000);
        assert_eq!(after.data.b_rate, 1_074_333_333_333);
        assert_eq!(reserves[&0].data.b_rate - after.data.b_rate, 25_666_666_667);

        // Order check. Against the *pre*-set-off `b_supply` of 3_030_000
        // the same 77_000 assets divide to
        // ceil(77_000_000_000_000_000 / 3_030_000) = 25_412_541_255
        // (3_030_000 x 25_412_541_254 leaves 380_000 over) — a smaller
        // haircut and a higher `b_rate`, which is the optimistic
        // projection that earns 1205 from the contract. Swap the two
        // steps and this is the rate that would land instead, higher by
        // the 254_125_412 the smaller divisor spares every supplier.
        assert_ne!(after.data.b_rate, 1_074_587_458_745);
        assert_ne!(after.data.b_rate, reserves[&0].data.b_rate - 25_412_541_255);

        // Case 2: a default larger than the whole rate. 2_000_000
        // d-tokens are ceil(2_000_000 x 1.1) = 2_200_000 assets over a
        // `b_supply` of 1_000_000, a loss of 2_200_000_000_000 against a
        // `b_rate` of 1_100_000_000_000. The clamp, never a negative.
        let reserves = two_reserves(1_000_000, 5_000_000);
        let borrower = positions(&[], &[(0, 2_000_000)], &[]);
        let outcome = project_default(&reserves, &prices(), &borrower).expect("projects");

        assert!(outcome.defaulted);
        assert_eq!(outcome.reserves[&0].data.b_rate, 0);
    }

    /// A liability naming an index the pool has no reserve for is the
    /// same error `math::fill`'s `reserve_for` raises, never a zero.
    #[test]
    fn a_liability_with_no_reserve_is_an_error() {
        let reserves = two_reserves(1_000_000, 500_000);
        let borrower = positions(&[], &[(7, 100_000)], &[]);
        assert_eq!(
            project_default(&reserves, &prices(), &borrower),
            Err(MathError::MissingReserve(7))
        );
    }
}
