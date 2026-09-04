//! Pure, chain-agnostic mathematics ported from the Blend v2 pool contract.
//!
//! Every function here mirrors a contract function with the same rounding
//! direction, because the bot's decisions are only as good as their
//! agreement with what the contract will compute at execution. Nothing in
//! this module performs I/O or panics: overflow and division by zero are
//! `MathError`s.

pub mod auction;
pub mod fixed;
pub mod position;
pub mod reserve;

pub use fixed::{
    div_ceil, div_floor, mul_ceil, mul_floor, pow10, MathError, SCALAR_12, SCALAR_7,
    SECONDS_PER_YEAR,
};
pub use position::{calculate_position_data, OraclePrices, PositionData, Positions};
pub use reserve::{calc_accrual, Reserve, ReserveConfig, ReserveData};
