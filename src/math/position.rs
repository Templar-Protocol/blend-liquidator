//! Position valuation and the health factor, ported from the contract's
//! `PositionData`.
//!
//! Collateral rounds down and liabilities round up at every step, because
//! that is the direction the contract rounds and the bot must never believe
//! a position is healthier than the contract will find it. All base values
//! are in the oracle's decimals; the health factor is a ratio in that same
//! scale, so `1.0` is `10^oracle_decimals`.

use std::collections::BTreeMap;

use super::fixed::{div_floor, mul_ceil, mul_floor, pow10, MathError, SCALAR_7};
use super::reserve::Reserve;

/// A user's pool positions, keyed by reserve index, in b-tokens (collateral
/// and supply) and d-tokens (liabilities).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Positions {
    /// Collateralised supply, in b-tokens.
    pub collateral: BTreeMap<u32, i128>,
    /// Borrowings, in d-tokens.
    pub liabilities: BTreeMap<u32, i128>,
    /// Supply that is not collateral and does not affect the health factor.
    pub supply: BTreeMap<u32, i128>,
}

impl Positions {
    /// Positions that count against the pool's `max_positions`: collateral
    /// and liabilities, never plain supply.
    pub fn effective_count(&self) -> usize {
        self.collateral.len() + self.liabilities.len()
    }

    /// True when the user holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.collateral.is_empty() && self.liabilities.is_empty() && self.supply.is_empty()
    }
}

/// A snapshot of one pool oracle: its decimals and a price per asset, both
/// as the contract sees them. Every stored price is strictly positive — `new`
/// is the only way to build one, and it enforces that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OraclePrices {
    /// The oracle's `decimals()`.
    decimals: u32,
    /// `10^decimals`, the scale every base value is expressed in.
    scalar: i128,
    /// Asset contract address to price.
    prices: BTreeMap<String, i128>,
}

impl OraclePrices {
    /// Builds a snapshot, deriving the scalar from the oracle's decimals.
    ///
    /// Every price must be strictly positive: a zero or negative price would
    /// value collateral at nothing (or less), collapsing the health factor and
    /// making a healthy account look liquidatable to the bot. The fields are
    /// private so `scalar` always equals `10^decimals` and no price can be
    /// inserted past this check.
    pub fn new(decimals: u32, prices: BTreeMap<String, i128>) -> Result<Self, MathError> {
        let scalar = pow10(decimals)?;
        if prices.values().any(|price| *price <= 0) {
            return Err(MathError::InvalidInput("oracle price must be positive"));
        }
        Ok(Self {
            decimals,
            scalar,
            prices,
        })
    }

    /// The oracle's `decimals()`.
    #[must_use]
    pub fn decimals(&self) -> u32 {
        self.decimals
    }

    /// `10^decimals`, the scale every base value is expressed in.
    #[must_use]
    pub fn scalar(&self) -> i128 {
        self.scalar
    }

    /// The price of `asset`, or `MissingPrice` when the snapshot has none.
    /// A missing price is never a zero: valuing a position at zero would
    /// make it look liquidatable. Every price this returns is strictly
    /// positive, per the invariant `new` enforces.
    pub fn price(&self, asset: &str) -> Result<i128, MathError> {
        self.prices
            .get(asset)
            .copied()
            .ok_or_else(|| MathError::MissingPrice(asset.to_string()))
    }

    /// Every priced asset and its price. `price` answers for one asset named
    /// up front; a caller that instead needs to walk the whole snapshot —
    /// the oracle scan comparing every asset against its own remembered
    /// reference — reads this.
    #[must_use]
    pub fn prices(&self) -> &BTreeMap<String, i128> {
        &self.prices
    }
}

/// A position valued in the oracle's base asset, effective (factor-adjusted)
/// and raw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionData {
    /// Collateral after collateral factors.
    pub collateral_base: i128,
    /// Collateral before collateral factors.
    pub collateral_raw: i128,
    /// Liabilities after liability factors.
    pub liability_base: i128,
    /// Liabilities before liability factors.
    pub liability_raw: i128,
    /// The oracle scalar these values are expressed in.
    pub scalar: i128,
}

impl PositionData {
    /// `collateral_base / liability_base` in the oracle's scale, or `None`
    /// when there are no liabilities — the contract divides only after
    /// guarding that case, and an `Option` makes the guard unforgettable.
    pub fn health_factor(&self) -> Result<Option<i128>, MathError> {
        if self.liability_base == 0 {
            return Ok(None);
        }
        div_floor(self.collateral_base, self.liability_base, self.scalar).map(Some)
    }

    /// Whether the health factor exceeds `max` (7 decimals). No liabilities
    /// counts as over any bound, matching `PositionData::is_hf_over`.
    pub fn is_hf_over(&self, max: i128) -> Result<bool, MathError> {
        match self.health_factor()? {
            None => Ok(true),
            Some(health_factor) => Ok(health_factor > mul_ceil(self.scalar, max, SCALAR_7)?),
        }
    }

    /// Whether the health factor is below `min` (7 decimals). No liabilities
    /// counts as under nothing, matching `PositionData::is_hf_under`.
    pub fn is_hf_under(&self, min: i128) -> Result<bool, MathError> {
        match self.health_factor()? {
            None => Ok(false),
            Some(health_factor) => Ok(health_factor < mul_floor(self.scalar, min, SCALAR_7)?),
        }
    }
}

/// Values `positions` against `reserves` and `prices`.
///
/// `reserves` must be keyed by `ReserveConfig::index` — the same key
/// `Positions`' `collateral` and `liabilities` maps use — and already
/// accrued to the decision's ledger. A map keyed by a reserve's position in
/// `ResList` instead is only correct while that position happens to match
/// the reserve's `index`; the two are not the same number in general.
///
/// A position in an index the pool does not have, or in an asset the oracle
/// snapshot does not price, is an error rather than a zero: both mean the
/// caller's view of the pool is incomplete, and a zero would silently
/// understate a position.
pub fn calculate_position_data(
    reserves: &BTreeMap<u32, Reserve>,
    prices: &OraclePrices,
    positions: &Positions,
) -> Result<PositionData, MathError> {
    let mut data = PositionData {
        collateral_base: 0,
        collateral_raw: 0,
        liability_base: 0,
        liability_raw: 0,
        scalar: prices.scalar(),
    };

    for (index, b_tokens) in &positions.collateral {
        if *b_tokens == 0 {
            continue;
        }
        let reserve = reserves
            .get(index)
            .ok_or(MathError::MissingReserve(*index))?;
        let price = prices.price(&reserve.asset)?;
        let effective = reserve.to_effective_asset_from_b_token(*b_tokens)?;
        let raw = reserve.to_asset_from_b_token(*b_tokens)?;
        data.collateral_base = data
            .collateral_base
            .checked_add(mul_floor(price, effective, reserve.scalar)?)
            .ok_or(MathError::Overflow)?;
        data.collateral_raw = data
            .collateral_raw
            .checked_add(mul_floor(price, raw, reserve.scalar)?)
            .ok_or(MathError::Overflow)?;
    }

    for (index, d_tokens) in &positions.liabilities {
        if *d_tokens == 0 {
            continue;
        }
        let reserve = reserves
            .get(index)
            .ok_or(MathError::MissingReserve(*index))?;
        let price = prices.price(&reserve.asset)?;
        let effective = reserve.to_effective_asset_from_d_token(*d_tokens)?;
        let raw = reserve.to_asset_from_d_token(*d_tokens)?;
        data.liability_base = data
            .liability_base
            .checked_add(mul_ceil(price, effective, reserve.scalar)?)
            .ok_or(MathError::Overflow)?;
        data.liability_raw = data
            .liability_raw
            .checked_add(mul_ceil(price, raw, reserve.scalar)?)
            .ok_or(MathError::Overflow)?;
    }

    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::reserve::{ReserveConfig, ReserveData};

    const ASSET_A: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const ASSET_B: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    /// 7 decimals, both rates 1.1, both factors 0.75.
    fn reserve(index: u32, asset: &str) -> Reserve {
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
            b_supply: 1_000_000_000,
            d_supply: 500_000_000,
            backstop_credit: 0,
            last_time: 0,
        };
        Reserve::new(asset.to_string(), config, data).expect("7 decimals fit")
    }

    /// Oracle at 7 decimals with both assets priced at 2.0.
    fn prices() -> OraclePrices {
        let mut map = BTreeMap::new();
        map.insert(ASSET_A.to_string(), 20_000_000);
        map.insert(ASSET_B.to_string(), 20_000_000);
        OraclePrices::new(7, map).expect("7 decimals fit")
    }

    fn one_reserve() -> BTreeMap<u32, Reserve> {
        BTreeMap::from([(0, reserve(0, ASSET_A))])
    }

    fn two_reserves() -> BTreeMap<u32, Reserve> {
        BTreeMap::from([(0, reserve(0, ASSET_A)), (1, reserve(1, ASSET_B))])
    }

    fn position(entries: &[(u32, i128, i128)]) -> Positions {
        let mut positions = Positions::default();
        for (index, collateral, liability) in entries {
            if *collateral > 0 {
                positions.collateral.insert(*index, *collateral);
            }
            if *liability > 0 {
                positions.liabilities.insert(*index, *liability);
            }
        }
        positions
    }

    #[test]
    fn values_one_position_the_way_the_contract_does() {
        // 2_000_000 b-tokens at rate 1.1 is 2_200_000 underlying, 1_650_000
        // effective, 3_300_000 base at price 2.0. 1_000_000 d-tokens is
        // 1_100_000 underlying, 1_466_667 effective, 2_933_334 base.
        let data = calculate_position_data(
            &one_reserve(),
            &prices(),
            &position(&[(0, 2_000_000, 1_000_000)]),
        )
        .expect("values");
        assert_eq!(data.collateral_base, 3_300_000);
        assert_eq!(data.collateral_raw, 4_400_000);
        assert_eq!(data.liability_base, 2_933_334);
        assert_eq!(data.liability_raw, 2_200_000);
        assert_eq!(data.scalar, SCALAR_7);
        assert_eq!(data.health_factor(), Ok(Some(11_249_997)));
    }

    #[test]
    fn sums_across_reserves() {
        let positions = position(&[(0, 2_000_000, 1_000_000), (1, 2_000_000, 1_000_000)]);
        let data = calculate_position_data(&two_reserves(), &prices(), &positions).expect("values");
        assert_eq!(data.collateral_base, 6_600_000);
        assert_eq!(data.collateral_raw, 8_800_000);
        assert_eq!(data.liability_base, 5_866_668);
        assert_eq!(data.liability_raw, 4_400_000);
        // Doubling both sides leaves the ratio, and so the floor, unchanged.
        assert_eq!(data.health_factor(), Ok(Some(11_249_997)));
    }

    #[test]
    fn a_reserve_with_no_position_contributes_nothing() {
        let positions = position(&[(0, 2_000_000, 1_000_000)]);
        let one = calculate_position_data(&one_reserve(), &prices(), &positions).expect("values");
        let two = calculate_position_data(&two_reserves(), &prices(), &positions).expect("values");
        assert_eq!(one, two);
    }

    #[test]
    fn health_factor_is_absent_without_liabilities() {
        let data =
            calculate_position_data(&one_reserve(), &prices(), &position(&[(0, 2_000_000, 0)]))
                .expect("values");
        assert_eq!(data.liability_base, 0);
        assert_eq!(data.health_factor(), Ok(None));
        // The contract treats no liabilities as over any maximum and under no
        // minimum, and so does this port.
        assert_eq!(data.is_hf_over(11_500_000), Ok(true));
        assert_eq!(data.is_hf_under(10_300_000), Ok(false));
    }

    #[test]
    fn hf_bounds_scale_a_7_decimal_threshold_to_the_oracle_scalar() {
        // Health factor 1.1249997 against the contract's own bounds.
        let data = calculate_position_data(
            &one_reserve(),
            &prices(),
            &position(&[(0, 2_000_000, 1_000_000)]),
        )
        .expect("values");
        assert_eq!(data.is_hf_over(11_500_000), Ok(false));
        assert_eq!(data.is_hf_over(11_000_000), Ok(true));
        assert_eq!(data.is_hf_under(11_500_000), Ok(true));
        assert_eq!(data.is_hf_under(10_300_000), Ok(false));
    }

    #[test]
    fn a_position_in_an_unknown_reserve_is_an_error() {
        let positions = position(&[(7, 2_000_000, 0)]);
        assert_eq!(
            calculate_position_data(&one_reserve(), &prices(), &positions),
            Err(MathError::MissingReserve(7))
        );
    }

    #[test]
    fn a_reserve_without_a_price_is_an_error() {
        let mut map = BTreeMap::new();
        map.insert(ASSET_B.to_string(), 20_000_000);
        let prices = OraclePrices::new(7, map).expect("scalar");
        let positions = position(&[(0, 2_000_000, 0)]);
        assert_eq!(
            calculate_position_data(&one_reserve(), &prices, &positions),
            Err(MathError::MissingPrice(ASSET_A.to_string()))
        );
    }

    #[test]
    fn values_positions_with_oracle_decimals_that_differ_from_the_reserves() {
        // The contract's own `health_factor.rs` tests price at 9 oracle
        // decimals for exactly this reason: a reserve's decimals and the
        // oracle's decimals are independent numbers, and a port that
        // silently assumed they matched would be wrong the first time a
        // pool used a non-7-decimal oracle.
        //
        // One reserve at 7 decimals, rate 1.0 on both sides (so token
        // amounts equal underlying exactly, with no rounding from the
        // rate), c_factor 0.8, l_factor 0.5 — both chosen so every division
        // below is exact.
        let config = ReserveConfig {
            index: 0,
            decimals: 7,
            c_factor: 8_000_000,
            l_factor: 5_000_000,
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
        let reserve_data = ReserveData {
            d_rate: 1_000_000_000_000,
            b_rate: 1_000_000_000_000,
            ir_mod: SCALAR_7,
            b_supply: 1_000_000_000,
            d_supply: 500_000_000,
            backstop_credit: 0,
            last_time: 0,
        };
        let reserve =
            Reserve::new(ASSET_A.to_string(), config, reserve_data).expect("7 decimals fit");
        let reserves = BTreeMap::from([(0, reserve)]);

        // A 9-decimal oracle (scalar 1_000_000_000) pricing the asset at 3.0.
        let mut price_map = BTreeMap::new();
        price_map.insert(ASSET_A.to_string(), 3_000_000_000);
        let prices = OraclePrices::new(9, price_map).expect("9 decimals fit");

        // 1_000_000 b-tokens collateral, 2_000_000 d-tokens liability, both
        // on the one reserve.
        let positions = position(&[(0, 1_000_000, 2_000_000)]);

        // Collateral: 1_000_000 b-tokens at rate 1.0 is 1_000_000 underlying
        // (raw). Effective = floor(1_000_000 * c_factor(0.8) / SCALAR_7)
        //                  = floor(1_000_000 * 8_000_000 / 10_000_000)
        //                  = 800_000.
        // collateral_raw  = floor(price * raw / reserve.scalar)
        //                 = floor(3_000_000_000 * 1_000_000 / 10_000_000)
        //                 = 300_000_000
        // collateral_base = floor(price * effective / reserve.scalar)
        //                 = floor(3_000_000_000 * 800_000 / 10_000_000)
        //                 = 240_000_000
        //
        // Liability: 2_000_000 d-tokens at rate 1.0 is 2_000_000 underlying
        // (raw). Effective = ceil(2_000_000 * SCALAR_7 / l_factor(0.5))
        //                  = ceil(2_000_000 * 10_000_000 / 5_000_000)
        //                  = 4_000_000.
        // liability_raw   = ceil(price * raw / reserve.scalar)
        //                 = ceil(3_000_000_000 * 2_000_000 / 10_000_000)
        //                 = 600_000_000
        // liability_base  = ceil(price * effective / reserve.scalar)
        //                 = ceil(3_000_000_000 * 4_000_000 / 10_000_000)
        //                 = 1_200_000_000
        //
        // health_factor = floor(collateral_base * oracle_scalar / liability_base)
        //               = floor(240_000_000 * 1_000_000_000 / 1_200_000_000)
        //               = 200_000_000   (0.2 at the oracle's 9 decimals)
        let data = calculate_position_data(&reserves, &prices, &positions).expect("values");
        assert_eq!(data.collateral_base, 240_000_000);
        assert_eq!(data.collateral_raw, 300_000_000);
        assert_eq!(data.liability_base, 1_200_000_000);
        assert_eq!(data.liability_raw, 600_000_000);
        assert_eq!(data.scalar, 1_000_000_000);
        assert_eq!(data.health_factor(), Ok(Some(200_000_000)));
    }

    #[test]
    fn effective_count_ignores_uncollateralised_supply() {
        let mut positions = position(&[(0, 2_000_000, 1_000_000)]);
        positions.supply.insert(1, 5_000_000);
        assert_eq!(positions.effective_count(), 2);
        assert!(!positions.is_empty());
        assert!(Positions::default().is_empty());
    }

    #[test]
    fn rejects_a_non_positive_price() {
        // A zero or negative price would value collateral at nothing (or
        // less), collapsing the health factor so a healthy account looks
        // liquidatable.
        let mut zero = BTreeMap::new();
        zero.insert(ASSET_A.to_string(), 0);
        assert_eq!(
            OraclePrices::new(7, zero),
            Err(MathError::InvalidInput("oracle price must be positive"))
        );

        let mut negative = BTreeMap::new();
        negative.insert(ASSET_A.to_string(), -1);
        assert_eq!(
            OraclePrices::new(7, negative),
            Err(MathError::InvalidInput("oracle price must be positive"))
        );
    }
}
