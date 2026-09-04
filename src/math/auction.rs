//! Dutch-auction scaling, ported from the contract's `scale_auction`.
//!
//! An auction runs for 400 blocks from its start. Over the first 200 the
//! filler receives a growing share of the lot for the whole bid; over the
//! next 200 it receives the whole lot for a shrinking bid; after 400 the bid
//! is nothing. The modifier moves 0.5% per block. Bids round up and lots
//! round down, so a rounding error can only cost the filler, never the pool.

use std::collections::BTreeMap;

use super::fixed::{mul_ceil, mul_floor, MathError, SCALAR_7};

/// Half a percent at 7 decimals: the per-block step of both ramps.
const PER_BLOCK_SCALAR: i128 = 50_000;
/// The block at which the lot ramp ends and the bid ramp begins.
const RAMP_BLOCKS: u32 = 200;
/// The block at which the bid ramp ends and the fill is free.
const RAMP_END_BLOCKS: u32 = 400;

/// An auction as the contract stores it: what the filler pays (`bid`), what
/// it receives (`lot`), and the block the auction began on. Amounts are
/// d-tokens, b-tokens or underlying depending on the auction type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuctionData {
    /// Asset address to amount the filler pays.
    pub bid: BTreeMap<String, i128>,
    /// Asset address to amount the filler receives.
    pub lot: BTreeMap<String, i128>,
    /// The block the auction started on.
    pub block: u32,
}

impl AuctionData {
    /// True when neither side has anything left.
    pub fn is_empty(&self) -> bool {
        self.bid.is_empty() && self.lot.is_empty()
    }
}

/// The result of scaling: what a fill at this block and percent exchanges,
/// and what would be left on the ledger afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaledAuction {
    /// The amounts this fill exchanges.
    pub to_fill: AuctionData,
    /// What remains after a partial fill; `None` means the fill closes it.
    pub remaining: Option<AuctionData>,
}

/// The bid modifier at `block_delta` blocks after the auction started, at 7
/// decimals: 100% for the first 200 blocks, then down to 0% at 400.
///
/// Returns `i128` rather than `Result` because the filler's fill-block
/// search (Phase 5) calls this in a tight loop over candidate blocks; the
/// arithmetic is bounded by the surrounding branch (see `scale_auction`'s
/// module docs) for every `block_delta`, but it still saturates instead of
/// using plain operators so that guarantee is not load-bearing for safety.
pub fn bid_modifier(block_delta: u32) -> i128 {
    if block_delta <= RAMP_BLOCKS {
        SCALAR_7
    } else if block_delta < RAMP_END_BLOCKS {
        // The branch guarantees `RAMP_BLOCKS < block_delta < RAMP_END_BLOCKS`,
        // so `elapsed` is in `1..RAMP_BLOCKS` and `decay` is in
        // `PER_BLOCK_SCALAR..SCALAR_7`: neither saturating op can trigger.
        let elapsed = block_delta.saturating_sub(RAMP_BLOCKS);
        let decay = i128::from(elapsed).saturating_mul(PER_BLOCK_SCALAR);
        SCALAR_7.saturating_sub(decay)
    } else {
        0
    }
}

/// The lot modifier at `block_delta` blocks after the auction started, at 7
/// decimals: 0% rising to 100% over the first 200 blocks, then 100%.
pub fn lot_modifier(block_delta: u32) -> i128 {
    if block_delta <= RAMP_BLOCKS {
        // Bounded by the branch: `block_delta <= RAMP_BLOCKS` (200), so the
        // product tops out at `200 * PER_BLOCK_SCALAR == SCALAR_7`.
        i128::from(block_delta).saturating_mul(PER_BLOCK_SCALAR)
    } else {
        SCALAR_7
    }
}

/// Scales `auction` for a fill of `percent_filled` percent at `fill_block`.
///
/// `percent_filled` is a whole percentage in `1..=100`, as the contract's
/// fill request takes it. `fill_block` must be at or after the auction's
/// start block.
pub fn scale_auction(
    auction: &AuctionData,
    fill_block: u32,
    percent_filled: u32,
) -> Result<ScaledAuction, MathError> {
    if percent_filled == 0 || percent_filled > 100 {
        return Err(MathError::InvalidInput("percent_filled must be 1..=100"));
    }
    let block_delta = fill_block
        .checked_sub(auction.block)
        .ok_or(MathError::InvalidInput(
            "fill_block precedes the auction block",
        ))?;

    let bid_scale = bid_modifier(block_delta);
    let lot_scale = lot_modifier(block_delta);
    // 100 percent is one whole, so a percentage scales to 7 decimals by 10^5.
    let percent_scaled = i128::from(percent_filled)
        .checked_mul(100_000)
        .ok_or(MathError::Overflow)?;

    let mut to_fill = AuctionData {
        block: auction.block,
        ..AuctionData::default()
    };
    let mut remaining = AuctionData {
        block: auction.block,
        ..AuctionData::default()
    };

    for (asset, amount) in &auction.bid {
        let to_fill_base = mul_ceil(*amount, percent_scaled, SCALAR_7)?;
        let remaining_base = amount
            .checked_sub(to_fill_base)
            .ok_or(MathError::Overflow)?;
        if remaining_base > 0 {
            remaining.bid.insert(asset.clone(), remaining_base);
        }
        let scaled = mul_ceil(to_fill_base, bid_scale, SCALAR_7)?;
        if scaled > 0 {
            to_fill.bid.insert(asset.clone(), scaled);
        }
    }

    for (asset, amount) in &auction.lot {
        let to_fill_base = mul_floor(*amount, percent_scaled, SCALAR_7)?;
        let remaining_base = amount
            .checked_sub(to_fill_base)
            .ok_or(MathError::Overflow)?;
        if remaining_base > 0 {
            remaining.lot.insert(asset.clone(), remaining_base);
        }
        let scaled = mul_floor(to_fill_base, lot_scale, SCALAR_7)?;
        if scaled > 0 {
            to_fill.lot.insert(asset.clone(), scaled);
        }
    }

    let remaining = if remaining.is_empty() {
        None
    } else {
        Some(remaining)
    };
    Ok(ScaledAuction { to_fill, remaining })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASSET_1: &str = "asset_1";
    const ASSET_2: &str = "asset_2";
    const ASSET_3: &str = "asset_3";

    fn auction() -> AuctionData {
        AuctionData {
            bid: BTreeMap::from([
                (ASSET_1.to_string(), 1_000_000_000),
                (ASSET_2.to_string(), 2_000_000_001),
            ]),
            lot: BTreeMap::from([
                (ASSET_2.to_string(), 10_000_000),
                (ASSET_3.to_string(), 50_000_001),
            ]),
            block: 100,
        }
    }

    fn one_stroop_auction() -> AuctionData {
        AuctionData {
            bid: BTreeMap::from([(ASSET_1.to_string(), 1)]),
            lot: BTreeMap::from([(ASSET_2.to_string(), 1)]),
            block: 100,
        }
    }

    #[test]
    fn modifiers_ramp_the_lot_then_the_bid() {
        assert_eq!((lot_modifier(0), bid_modifier(0)), (0, SCALAR_7));
        assert_eq!(
            (lot_modifier(100), bid_modifier(100)),
            (5_000_000, SCALAR_7)
        );
        assert_eq!((lot_modifier(200), bid_modifier(200)), (SCALAR_7, SCALAR_7));
        assert_eq!(
            (lot_modifier(300), bid_modifier(300)),
            (SCALAR_7, 5_000_000)
        );
        assert_eq!((lot_modifier(399), bid_modifier(399)), (SCALAR_7, 50_000));
        assert_eq!((lot_modifier(400), bid_modifier(400)), (SCALAR_7, 0));
        assert_eq!((lot_modifier(4_000), bid_modifier(4_000)), (SCALAR_7, 0));
    }

    #[test]
    fn at_the_start_block_the_filler_pays_everything_and_receives_nothing() {
        let scaled = scale_auction(&auction(), 100, 100).expect("scales");
        assert_eq!(scaled.to_fill.block, 100);
        assert_eq!(scaled.to_fill.bid[ASSET_1], 1_000_000_000);
        assert_eq!(scaled.to_fill.bid[ASSET_2], 2_000_000_001);
        assert!(scaled.to_fill.lot.is_empty());
        assert_eq!(scaled.remaining, None);
    }

    #[test]
    fn halfway_through_the_lot_ramp_the_lot_rounds_down() {
        let scaled = scale_auction(&auction(), 200, 100).expect("scales");
        assert_eq!(scaled.to_fill.bid[ASSET_1], 1_000_000_000);
        assert_eq!(scaled.to_fill.bid[ASSET_2], 2_000_000_001);
        assert_eq!(scaled.to_fill.lot[ASSET_2], 5_000_000);
        assert_eq!(scaled.to_fill.lot[ASSET_3], 25_000_000);
        assert_eq!(scaled.remaining, None);
    }

    #[test]
    fn a_partial_fill_rounds_the_bid_up_and_leaves_the_rest() {
        let scaled = scale_auction(&auction(), 200, 50).expect("scales");
        assert_eq!(scaled.to_fill.bid[ASSET_1], 500_000_000);
        assert_eq!(scaled.to_fill.bid[ASSET_2], 1_000_000_001);
        assert_eq!(scaled.to_fill.lot[ASSET_2], 2_500_000);
        assert_eq!(scaled.to_fill.lot[ASSET_3], 12_500_000);
        let remaining = scaled.remaining.expect("half remains");
        assert_eq!(remaining.block, 100);
        assert_eq!(remaining.bid[ASSET_1], 500_000_000);
        assert_eq!(remaining.bid[ASSET_2], 1_000_000_000);
        assert_eq!(remaining.lot[ASSET_2], 5_000_000);
        assert_eq!(remaining.lot[ASSET_3], 25_000_001);
    }

    #[test]
    fn at_block_200_the_whole_auction_changes_hands() {
        let scaled = scale_auction(&auction(), 300, 100).expect("scales");
        assert_eq!(scaled.to_fill.bid[ASSET_1], 1_000_000_000);
        assert_eq!(scaled.to_fill.bid[ASSET_2], 2_000_000_001);
        assert_eq!(scaled.to_fill.lot[ASSET_2], 10_000_000);
        assert_eq!(scaled.to_fill.lot[ASSET_3], 50_000_001);
    }

    #[test]
    fn past_block_200_the_bid_decays_and_then_vanishes() {
        let half = scale_auction(&auction(), 400, 100).expect("scales");
        assert_eq!(half.to_fill.bid[ASSET_1], 500_000_000);
        assert_eq!(half.to_fill.bid[ASSET_2], 1_000_000_001);
        assert_eq!(half.to_fill.lot[ASSET_3], 50_000_001);

        let free = scale_auction(&auction(), 500, 100).expect("scales");
        assert!(free.to_fill.bid.is_empty());
        assert_eq!(free.to_fill.lot[ASSET_2], 10_000_000);
        assert_eq!(free.to_fill.lot[ASSET_3], 50_000_001);

        let still_free = scale_auction(&auction(), 600, 100).expect("scales");
        assert_eq!(still_free.to_fill, free.to_fill);
    }

    #[test]
    fn one_stroop_rounds_the_bid_up_and_the_lot_away() {
        let early = scale_auction(&one_stroop_auction(), 101, 10).expect("scales");
        assert_eq!(early.to_fill.bid[ASSET_1], 1);
        assert!(early.to_fill.lot.is_empty());
        // The whole stroop of bid was taken, so only the lot remains.
        let remaining = early.remaining.expect("lot remains");
        assert!(remaining.bid.is_empty());
        assert_eq!(remaining.lot[ASSET_2], 1);

        let late = scale_auction(&one_stroop_auction(), 499, 100).expect("scales");
        assert_eq!(late.to_fill.bid[ASSET_1], 1);
        assert_eq!(late.to_fill.lot[ASSET_2], 1);
        assert_eq!(late.remaining, None);
    }

    #[test]
    fn a_percent_outside_one_to_a_hundred_is_an_error() {
        assert_eq!(
            scale_auction(&auction(), 200, 0),
            Err(MathError::InvalidInput("percent_filled must be 1..=100"))
        );
        assert_eq!(
            scale_auction(&auction(), 200, 101),
            Err(MathError::InvalidInput("percent_filled must be 1..=100"))
        );
    }

    #[test]
    fn a_fill_block_before_the_auction_started_is_an_error() {
        assert_eq!(
            scale_auction(&auction(), 99, 100),
            Err(MathError::InvalidInput(
                "fill_block precedes the auction block"
            ))
        );
    }
}
