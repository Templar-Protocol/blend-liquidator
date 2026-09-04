//! Reserve state and the interest accrual the contract performs on every
//! load.
//!
//! Ledger entries hold a reserve as of its last update; the contract accrues
//! to the current ledger inside every call. `Reserve::accrue` ports
//! `Reserve::load` plus `interest::calc_accrual` from the v2 pool so the bot
//! values positions on the same numbers the contract will use. Rates are 12
//! decimals, factors and utilisation 7 decimals, `ir_mod` 7 decimals.

use super::fixed::{
    div_ceil, div_floor, mul_ceil, mul_floor, pow10, MathError, SCALAR_12, SCALAR_7,
    SECONDS_PER_YEAR,
};

/// The `ResConfig` ledger entry. Factors and rates are 7 decimals;
/// `supply_cap` is in the underlying's decimals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveConfig {
    pub index: u32,
    pub decimals: u32,
    pub c_factor: u32,
    pub l_factor: u32,
    pub util: u32,
    pub max_util: u32,
    pub r_base: u32,
    pub r_one: u32,
    pub r_two: u32,
    pub r_three: u32,
    pub reactivity: u32,
    pub supply_cap: i128,
    pub enabled: bool,
}

/// The `ResData` ledger entry. `d_rate` and `b_rate` are 12 decimals,
/// `ir_mod` is 7 decimals, supplies are token amounts in the underlying's
/// decimals, `last_time` is a ledger close time in seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveData {
    pub d_rate: i128,
    pub b_rate: i128,
    pub ir_mod: i128,
    pub b_supply: i128,
    pub d_supply: i128,
    pub backstop_credit: i128,
    pub last_time: u64,
}

/// A reserve as the contract's `Reserve` struct: config, data and the
/// underlying's scalar `10^decimals`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reserve {
    /// Strkey contract address of the underlying asset.
    pub asset: String,
    pub config: ReserveConfig,
    pub data: ReserveData,
    pub scalar: i128,
}

const UTIL_95: i128 = 9_500_000;
const UTIL_5: i128 = 500_000;
const IR_MOD_MAX: i128 = 10 * SCALAR_7;
const IR_MOD_MIN: i128 = SCALAR_7 / 10;

impl Reserve {
    /// Builds a reserve, deriving `scalar` from `config.decimals`.
    pub fn new(asset: String, config: ReserveConfig, data: ReserveData) -> Result<Self, MathError> {
        let scalar = pow10(config.decimals)?;
        Ok(Self {
            asset,
            config,
            data,
            scalar,
        })
    }

    /// Total borrowed underlying: `d_supply` at `d_rate`, rounded up.
    pub fn total_liabilities(&self) -> Result<i128, MathError> {
        self.to_asset_from_d_token(self.data.d_supply)
    }

    /// Total supplied underlying: `b_supply` at `b_rate`, rounded down.
    pub fn total_supply(&self) -> Result<i128, MathError> {
        self.to_asset_from_b_token(self.data.b_supply)
    }

    /// Utilisation at 7 decimals, capped at 100% so the rate curve stays fair.
    pub fn utilization(&self) -> Result<i128, MathError> {
        let liabilities = self.total_liabilities()?;
        let supply = self.total_supply()?;
        if liabilities == 0 {
            Ok(0)
        } else if liabilities >= supply {
            Ok(SCALAR_7)
        } else {
            div_ceil(liabilities, supply, SCALAR_7)
        }
    }

    /// d-tokens to underlying, rounded up (the borrower owes the ceiling).
    pub fn to_asset_from_d_token(&self, d_tokens: i128) -> Result<i128, MathError> {
        mul_ceil(d_tokens, self.data.d_rate, SCALAR_12)
    }

    /// b-tokens to underlying, rounded down (the supplier gets the floor).
    pub fn to_asset_from_b_token(&self, b_tokens: i128) -> Result<i128, MathError> {
        mul_floor(b_tokens, self.data.b_rate, SCALAR_12)
    }

    /// d-tokens to effective liability: underlying divided by `l_factor`, up.
    pub fn to_effective_asset_from_d_token(&self, d_tokens: i128) -> Result<i128, MathError> {
        let assets = self.to_asset_from_d_token(d_tokens)?;
        div_ceil(assets, i128::from(self.config.l_factor), SCALAR_7)
    }

    /// b-tokens to effective collateral: underlying times `c_factor`, down.
    pub fn to_effective_asset_from_b_token(&self, b_tokens: i128) -> Result<i128, MathError> {
        let assets = self.to_asset_from_b_token(b_tokens)?;
        mul_floor(assets, i128::from(self.config.c_factor), SCALAR_7)
    }

    /// Underlying to d-tokens, rounded up.
    pub fn to_d_token_up(&self, amount: i128) -> Result<i128, MathError> {
        div_ceil(amount, self.data.d_rate, SCALAR_12)
    }

    /// Underlying to d-tokens, rounded down.
    pub fn to_d_token_down(&self, amount: i128) -> Result<i128, MathError> {
        div_floor(amount, self.data.d_rate, SCALAR_12)
    }

    /// Underlying to b-tokens, rounded up.
    pub fn to_b_token_up(&self, amount: i128) -> Result<i128, MathError> {
        div_ceil(amount, self.data.b_rate, SCALAR_12)
    }

    /// Underlying to b-tokens, rounded down.
    pub fn to_b_token_down(&self, amount: i128) -> Result<i128, MathError> {
        div_floor(amount, self.data.b_rate, SCALAR_12)
    }

    /// Accrues interest to `now` exactly as the contract's `Reserve::load`:
    /// no-op within the same second, time-stamp only when nothing is
    /// supplied or borrowed, otherwise update `ir_mod`, `d_rate`, the
    /// backstop credit and `b_rate`. `now` before `last_time` is invalid.
    pub fn accrue(&mut self, bstop_rate: u32, now: u64) -> Result<(), MathError> {
        if now == self.data.last_time {
            return Ok(());
        }
        if now < self.data.last_time {
            return Err(MathError::InvalidInput("now is before last_time"));
        }
        if self.data.b_supply == 0 {
            self.data.last_time = now;
            return Ok(());
        }
        let cur_util = self.utilization()?;
        if cur_util == 0 {
            self.data.last_time = now;
            return Ok(());
        }
        let (loan_accrual, new_ir_mod) = calc_accrual(
            &self.config,
            cur_util,
            self.data.ir_mod,
            self.data.last_time,
            now,
        )?;
        self.data.ir_mod = new_ir_mod;

        let pre_update_supply = self.total_supply()?;
        let pre_update_liabilities = self.total_liabilities()?;
        self.data.d_rate = mul_ceil(loan_accrual, self.data.d_rate, SCALAR_12)?;
        let accrued = self
            .total_liabilities()?
            .checked_sub(pre_update_liabilities)
            .ok_or(MathError::Overflow)?;
        if accrued > 0 {
            let mut new_backstop_credit = 0;
            if bstop_rate > 0 {
                new_backstop_credit = mul_floor(accrued, i128::from(bstop_rate), SCALAR_7)?;
                self.data.backstop_credit = self
                    .data
                    .backstop_credit
                    .checked_add(new_backstop_credit)
                    .ok_or(MathError::Overflow)?;
            }
            let supply_after = pre_update_supply
                .checked_add(accrued)
                .and_then(|s| s.checked_sub(new_backstop_credit))
                .ok_or(MathError::Overflow)?;
            self.data.b_rate = div_floor(supply_after, self.data.b_supply, SCALAR_12)?;
        }
        self.data.last_time = now;
        Ok(())
    }
}

/// The contract's `interest::calc_accrual`: returns the loan accrual factor
/// at 12 decimals and the next interest-rate modifier at 7 decimals.
/// `cur_util` is 7 decimals; `now` must be at least one second after
/// `last_time`.
pub fn calc_accrual(
    config: &ReserveConfig,
    cur_util: i128,
    ir_mod: i128,
    last_time: u64,
    now: u64,
) -> Result<(i128, i128), MathError> {
    let target_util = i128::from(config.util);
    let cur_ir = if cur_util <= target_util {
        let util_scalar = div_ceil(cur_util, target_util, SCALAR_7)?;
        let base_rate =
            mul_ceil(util_scalar, i128::from(config.r_one), SCALAR_7)? + i128::from(config.r_base);
        mul_ceil(base_rate, ir_mod, SCALAR_7)?
    } else if cur_util <= UTIL_95 {
        let util_scalar = div_ceil(cur_util - target_util, UTIL_95 - target_util, SCALAR_7)?;
        let base_rate = mul_ceil(util_scalar, i128::from(config.r_two), SCALAR_7)?
            + i128::from(config.r_one)
            + i128::from(config.r_base);
        mul_ceil(base_rate, ir_mod, SCALAR_7)?
    } else {
        let util_scalar = div_ceil(cur_util - UTIL_95, UTIL_5, SCALAR_7)?;
        let extra_rate = mul_ceil(util_scalar, i128::from(config.r_three), SCALAR_7)?;
        let intersection = mul_ceil(
            ir_mod,
            i128::from(config.r_two) + i128::from(config.r_one) + i128::from(config.r_base),
            SCALAR_7,
        )?;
        extra_rate + intersection
    };

    let delta_time = i128::from(
        now.checked_sub(last_time)
            .ok_or(MathError::InvalidInput("now is before last_time"))?,
    );
    if delta_time < 1 {
        return Err(MathError::InvalidInput("no time elapsed"));
    }
    let util_dif = cur_util - target_util;
    let util_error = delta_time
        .checked_mul(util_dif)
        .ok_or(MathError::Overflow)?;
    let new_ir_mod = if util_dif >= 0 {
        let rate_dif = mul_floor(util_error, i128::from(config.reactivity), SCALAR_7)?;
        (ir_mod + rate_dif).min(IR_MOD_MAX)
    } else {
        let rate_dif = mul_ceil(util_error, i128::from(config.reactivity), SCALAR_7)?;
        (ir_mod + rate_dif).max(IR_MOD_MIN)
    };

    let time_weight = delta_time
        .checked_mul(SCALAR_12)
        .ok_or(MathError::Overflow)?
        / SECONDS_PER_YEAR;
    let accrual = SCALAR_12 + mul_ceil(time_weight, cur_ir, SCALAR_7)?;
    Ok((accrual, new_ir_mod))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xlm_config() -> ReserveConfig {
        ReserveConfig {
            index: 0,
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
        }
    }

    fn xlm_data() -> ReserveData {
        ReserveData {
            d_rate: 1_001_568_283_884,
            b_rate: 1_000_022_303_241,
            ir_mod: 1_000_000,
            b_supply: 7_654_654_078_715_796,
            d_supply: 13_201_825_877_188,
            backstop_credit: 31_426_481,
            last_time: 1_788_533_688,
        }
    }

    fn simple_reserve() -> Reserve {
        let config = ReserveConfig {
            c_factor: 7_500_000,
            l_factor: 7_500_000,
            ..xlm_config()
        };
        let data = ReserveData {
            d_rate: 1_100_000_000_000,
            b_rate: 1_100_000_000_000,
            ir_mod: SCALAR_7,
            b_supply: 1_000_000,
            d_supply: 500_000,
            backstop_credit: 0,
            last_time: 0,
        };
        Reserve::new("XLM".to_string(), config, data).expect("7 decimals fit")
    }

    #[test]
    fn new_derives_the_scalar_from_decimals() {
        let reserve = simple_reserve();
        assert_eq!(reserve.scalar, SCALAR_7);
        let config = ReserveConfig {
            decimals: 39,
            ..xlm_config()
        };
        assert_eq!(
            Reserve::new("X".to_string(), config, xlm_data()).err(),
            Some(MathError::Overflow)
        );
    }

    #[test]
    fn token_conversions_round_the_way_the_contract_does() {
        let reserve = simple_reserve();
        // d-tokens to underlying round up; b-tokens to underlying round down.
        assert_eq!(reserve.to_asset_from_d_token(1_000), Ok(1_100));
        assert_eq!(reserve.to_asset_from_d_token(1), Ok(2));
        assert_eq!(reserve.to_asset_from_b_token(1_000), Ok(1_100));
        assert_eq!(reserve.to_asset_from_b_token(1), Ok(1));
        // Effective liability divides by the liability factor and rounds up;
        // effective collateral multiplies by the collateral factor and rounds down.
        assert_eq!(reserve.to_effective_asset_from_d_token(1_000), Ok(1_467));
        assert_eq!(reserve.to_effective_asset_from_b_token(1_000), Ok(825));
        // Underlying to tokens, both directions of rounding.
        assert_eq!(reserve.to_d_token_up(1_101), Ok(1_001));
        assert_eq!(reserve.to_d_token_down(1_101), Ok(1_000));
        assert_eq!(reserve.to_b_token_up(1_101), Ok(1_001));
        assert_eq!(reserve.to_b_token_down(1_101), Ok(1_000));
    }

    #[test]
    fn utilization_is_liabilities_over_supply_capped_at_one() {
        let reserve = simple_reserve();
        // 550_000 liabilities over 1_100_000 supply, rounded up at 7 decimals.
        assert_eq!(reserve.utilization(), Ok(5_000_000));
        let mut full = simple_reserve();
        full.data.d_supply = full.data.b_supply * 2;
        assert_eq!(full.utilization(), Ok(SCALAR_7));
        let mut empty = simple_reserve();
        empty.data.d_supply = 0;
        assert_eq!(empty.utilization(), Ok(0));
    }

    #[test]
    fn accrue_reproduces_the_contracts_get_reserve() {
        let mut reserve =
            Reserve::new("XLM".to_string(), xlm_config(), xlm_data()).expect("scalar");
        reserve.accrue(2_000_000, 1_788_533_824).expect("accrues");
        assert_eq!(reserve.data.d_rate, 1_001_568_288_260);
        assert_eq!(reserve.data.b_rate, 1_000_022_303_247);
        assert_eq!(reserve.data.ir_mod, 1_000_000);
        assert_eq!(reserve.data.backstop_credit, 31_438_035);
        assert_eq!(reserve.data.last_time, 1_788_533_824);
        assert_eq!(reserve.data.b_supply, 7_654_654_078_715_796);
        assert_eq!(reserve.data.d_supply, 13_201_825_877_188);
    }

    #[test]
    fn accrue_is_a_no_op_within_the_same_second() {
        let mut reserve =
            Reserve::new("XLM".to_string(), xlm_config(), xlm_data()).expect("scalar");
        let before = reserve.data.clone();
        reserve
            .accrue(2_000_000, before.last_time)
            .expect("accrues");
        assert_eq!(reserve.data, before);
    }

    #[test]
    fn accrue_only_stamps_time_when_nothing_is_borrowed() {
        let mut reserve = simple_reserve();
        reserve.data.d_supply = 0;
        reserve.accrue(2_000_000, 100).expect("accrues");
        assert_eq!(reserve.data.d_rate, 1_100_000_000_000);
        assert_eq!(reserve.data.last_time, 100);
        let mut empty = simple_reserve();
        empty.data.b_supply = 0;
        empty.accrue(2_000_000, 100).expect("accrues");
        assert_eq!(empty.data.last_time, 100);
    }

    #[test]
    fn accrue_rejects_time_running_backwards() {
        let mut reserve =
            Reserve::new("XLM".to_string(), xlm_config(), xlm_data()).expect("scalar");
        assert_eq!(
            reserve.accrue(2_000_000, 1_788_533_000),
            Err(MathError::InvalidInput("now is before last_time"))
        );
    }
}
