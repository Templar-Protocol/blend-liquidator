//! Pure, chain-agnostic mathematics ported from the Precept pool contract.
//!
//! Every function here mirrors a contract function with the same rounding
//! direction, because the bot's decisions are only as good as their
//! agreement with what the contract will compute at execution. Nothing in
//! this module performs I/O or panics: overflow and division by zero are
//! `MathError`s.
//!
//! Ported from stock `blend-capital/blend-contracts-v2` tag `v2.0.0`, whose
//! arithmetic Precept keeps unchanged — though not every file whole: the
//! fork's `auction.rs` also refuses every auction type but a user
//! liquidation, and runs the default path on a full fill:
//! `pool/src/pool/{reserve,interest,health_factor}.rs` and
//! `pool/src/auctions/auction.rs`. The next contract upgrade is a bounded
//! diff against those four files.

pub mod auction;
pub mod fill;
pub mod fixed;
pub mod liquidation;
pub mod position;
pub mod reserve;
pub mod setoff;
pub mod unwind;

pub use auction::{scale_auction, AuctionData, ScaledAuction};
pub use fixed::{
    div_ceil, div_floor, mul_ceil, mul_floor, pow10, MathError, SCALAR_12, SCALAR_7,
    SECONDS_PER_YEAR,
};
pub use position::{calculate_position_data, OraclePrices, PositionData, Positions};
pub use reserve::{calc_accrual, Reserve, ReserveConfig, ReserveData};
