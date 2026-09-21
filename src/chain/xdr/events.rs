//! Pool event decoding.
//!
//! Topic and data shapes come from the v2 pool's `PoolEvents`. An event this
//! bot does not model decodes to `None` rather than an error: pools emit
//! events for emissions and administration that no liquidator needs, and a
//! contract upgrade may add more, neither of which may stall the poller. A
//! *modelled* event with an unexpected shape is an error, because that means
//! a shape this bot depends on has changed.
//!
//! `DefaultedDebt`, `DebtSetoff` and `OrphanSettled` name no account
//! (`PoolEvent::affected_accounts` answers none for them): a `b_rate` cut
//! that lowers every borrower's position in that reserve flags nobody for
//! a targeted re-read, and a `bad_debt(user)` call whose set-off clears the
//! debt entirely from the borrower's own supply emits only `DebtSetoff` —
//! no user-bearing event at all — so that borrower's row goes stale until
//! the next full scan reaches it. A known bound of event-driven tracking,
//! not a defect this module can close on its own.

use stellar_xdr::ScVal;

use super::decode::{auction_value, PoolStatus};
use super::{AuctionType, XdrError};
use crate::math::AuctionData;

/// A Blend v2 pool event the bot acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolEvent {
    /// Uncollateralised supply.
    Supply {
        asset: String,
        from: String,
        amount: i128,
        b_tokens: i128,
    },
    /// Withdrawal of uncollateralised supply.
    Withdraw {
        asset: String,
        from: String,
        amount: i128,
        b_tokens: i128,
    },
    /// Supply posted as collateral.
    SupplyCollateral {
        asset: String,
        from: String,
        amount: i128,
        b_tokens: i128,
    },
    /// Collateral withdrawn.
    WithdrawCollateral {
        asset: String,
        from: String,
        amount: i128,
        b_tokens: i128,
    },
    /// New borrowing.
    Borrow {
        asset: String,
        from: String,
        amount: i128,
        d_tokens: i128,
    },
    /// Debt repaid.
    Repay {
        asset: String,
        from: String,
        amount: i128,
        d_tokens: i128,
    },
    /// A flash loan, which also mints debt for `from` within the transaction.
    FlashLoan {
        asset: String,
        from: String,
        contract: String,
        amount: i128,
        d_tokens: i128,
    },
    /// An auction was created. `percent` is the share of the user's
    /// positions auctioned.
    NewAuction {
        auction_type: AuctionType,
        user: String,
        percent: u32,
        auction: AuctionData,
    },
    /// An auction was filled, wholly or in part, by `filler`.
    FillAuction {
        auction_type: AuctionType,
        user: String,
        filler: String,
        fill_percent: i128,
        filled: AuctionData,
    },
    /// An auction was deleted before being filled.
    DeleteAuction {
        auction_type: AuctionType,
        user: String,
    },
    /// A user's debt moved to the backstop — stock semantics only. The
    /// fork declares this event with zero call sites, so a pool built
    /// from ADR-0008 can never emit it: seeing one means the pool is
    /// running stock wasm and every fork assumption this bot makes is
    /// void (`NotificationKind::StockWasmDetected`).
    BadDebt {
        user: String,
        asset: String,
        d_tokens: i128,
    },
    /// Debt destroyed and the reserve's `b_rate` cut so suppliers absorb
    /// the loss. Verbatim from stock, but the fork emits it on two paths:
    /// the backstop's, and the user path `check_and_handle_user_bad_debt`
    /// runs inside a full liquidation fill — the event a modelled full
    /// fill's projected haircut (`math::setoff`) causes.
    DefaultedDebt { asset: String, d_tokens: i128 },
    /// The fork's set-off: before declaring a default it repays what it
    /// can from the borrower's own supply in the debt reserve. Names no
    /// account — the event carries only the asset.
    DebtSetoff {
        asset: String,
        b_tokens_burned: i128,
        d_tokens_repaid: i128,
    },
    /// A defaulted borrower's remaining collateral, moved into the pool
    /// contract's own `supply`. Emitted only when a residual default
    /// actually occurred: a set-off that clears the debt leaves the
    /// collateral with the borrower and emits nothing.
    CollateralOrphaned {
        user: String,
        asset: String,
        b_tokens: i128,
    },
    /// Orphaned collateral retired from pool custody by `gulp`, which is
    /// refused while the reserve carries any debt.
    OrphanSettled { asset: String, b_tokens: i128 },
    /// A reserve was added or reconfigured.
    SetReserve { asset: String, index: u32 },
    /// The pool's status changed, which gates what requests it accepts.
    SetStatus { status: PoolStatus },
}

impl PoolEvent {
    /// The accounts whose pool positions this event may have changed, and
    /// which the tracker must therefore re-read. Empty for events that
    /// change pool-wide state only.
    pub fn affected_accounts(&self) -> Vec<&str> {
        match self {
            Self::Supply { from, .. }
            | Self::Withdraw { from, .. }
            | Self::SupplyCollateral { from, .. }
            | Self::WithdrawCollateral { from, .. }
            | Self::Borrow { from, .. }
            | Self::Repay { from, .. }
            | Self::FlashLoan { from, .. } => vec![from],
            // `BadDebt` is stock-only (see its own doc): a fork pool never
            // emits it. On stock it moves the liability to the backstop,
            // whose own positions change too, but the event carries no
            // backstop address, so a consumer that wants to refresh it
            // adds `PoolInstance::backstop` itself.
            Self::NewAuction { user, .. }
            | Self::DeleteAuction { user, .. }
            | Self::CollateralOrphaned { user, .. }
            | Self::BadDebt { user, .. } => vec![user],
            Self::FillAuction { user, filler, .. } => vec![user, filler],
            Self::DefaultedDebt { .. }
            | Self::DebtSetoff { .. }
            | Self::OrphanSettled { .. }
            | Self::SetReserve { .. }
            | Self::SetStatus { .. } => Vec::new(),
        }
    }
}

fn shape(expected: &'static str, got: &impl std::fmt::Debug) -> XdrError {
    XdrError::Shape {
        expected,
        got: format!("{got:?}"),
    }
}

fn as_address(value: &ScVal) -> Result<String, XdrError> {
    match value {
        ScVal::Address(address) => Ok(address.to_string()),
        other => Err(shape("address", other)),
    }
}

fn as_u32(value: &ScVal) -> Result<u32, XdrError> {
    match value {
        ScVal::U32(number) => Ok(*number),
        other => Err(shape("u32", other)),
    }
}

fn as_i128(value: &ScVal) -> Result<i128, XdrError> {
    match value {
        ScVal::I128(parts) => Ok((i128::from(parts.hi) << 64) | i128::from(parts.lo)),
        other => Err(shape("i128", other)),
    }
}

/// The data vector of an event, required to hold exactly `expected` items.
fn data(value: &ScVal, expected: usize) -> Result<&[ScVal], XdrError> {
    match value {
        ScVal::Vec(Some(items)) if items.len() == expected => Ok(items.as_slice()),
        other => Err(shape("event data vector", other)),
    }
}

/// A topic at `index`, or a shape error naming what was expected.
fn topic(topics: &[ScVal], index: usize) -> Result<&ScVal, XdrError> {
    topics.get(index).ok_or_else(|| XdrError::Shape {
        expected: "another topic",
        got: format!("{} topics", topics.len()),
    })
}

/// Requires exactly `expected` topics. A recognised event with a surplus
/// topic is as much a shape mismatch as a missing one: silently ignoring
/// the extra topic would decode a shape this bot does not actually model.
fn require_topics(
    topics: &[ScVal],
    expected: usize,
    message: &'static str,
) -> Result<(), XdrError> {
    if topics.len() == expected {
        Ok(())
    } else {
        Err(XdrError::Shape {
            expected: message,
            got: format!("{} topics", topics.len()),
        })
    }
}

/// asset, from, amount, reserve tokens: the shape shared by `supply`,
/// `withdraw`, `supply_collateral`, `withdraw_collateral`, `borrow` and
/// `repay`. `topic_count_message` names the caller's event for the shape
/// error when the topic count is wrong.
fn two_sided(
    topics: &[ScVal],
    value: &ScVal,
    topic_count_message: &'static str,
) -> Result<(String, String, i128, i128), XdrError> {
    require_topics(topics, 3, topic_count_message)?;
    let asset = as_address(topic(topics, 1)?)?;
    let from = as_address(topic(topics, 2)?)?;
    let items = data(value, 2)?;
    Ok((asset, from, as_i128(&items[0])?, as_i128(&items[1])?))
}

/// The six two-sided position events, plus `flash_loan`, which shares their
/// asset/from shape but adds a counterparty contract.
fn decode_position_event(
    name: &str,
    topics: &[ScVal],
    value: &ScVal,
) -> Result<Option<PoolEvent>, XdrError> {
    let event = match name {
        "supply" => {
            let (asset, from, amount, b_tokens) = two_sided(topics, value, "supply with 3 topics")?;
            PoolEvent::Supply {
                asset,
                from,
                amount,
                b_tokens,
            }
        }
        "withdraw" => {
            let (asset, from, amount, b_tokens) =
                two_sided(topics, value, "withdraw with 3 topics")?;
            PoolEvent::Withdraw {
                asset,
                from,
                amount,
                b_tokens,
            }
        }
        "supply_collateral" => {
            let (asset, from, amount, b_tokens) =
                two_sided(topics, value, "supply_collateral with 3 topics")?;
            PoolEvent::SupplyCollateral {
                asset,
                from,
                amount,
                b_tokens,
            }
        }
        "withdraw_collateral" => {
            let (asset, from, amount, b_tokens) =
                two_sided(topics, value, "withdraw_collateral with 3 topics")?;
            PoolEvent::WithdrawCollateral {
                asset,
                from,
                amount,
                b_tokens,
            }
        }
        "borrow" => {
            let (asset, from, amount, d_tokens) = two_sided(topics, value, "borrow with 3 topics")?;
            PoolEvent::Borrow {
                asset,
                from,
                amount,
                d_tokens,
            }
        }
        "repay" => {
            let (asset, from, amount, d_tokens) = two_sided(topics, value, "repay with 3 topics")?;
            PoolEvent::Repay {
                asset,
                from,
                amount,
                d_tokens,
            }
        }
        "flash_loan" => {
            require_topics(topics, 4, "flash_loan with 4 topics")?;
            let items = data(value, 2)?;
            PoolEvent::FlashLoan {
                asset: as_address(topic(topics, 1)?)?,
                from: as_address(topic(topics, 2)?)?,
                contract: as_address(topic(topics, 3)?)?,
                amount: as_i128(&items[0])?,
                d_tokens: as_i128(&items[1])?,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// The auction lifecycle: `new_auction`, `fill_auction` and
/// `delete_auction`.
fn decode_auction_event(
    name: &str,
    topics: &[ScVal],
    value: &ScVal,
) -> Result<Option<PoolEvent>, XdrError> {
    let event = match name {
        "new_auction" => {
            require_topics(topics, 3, "new_auction with 3 topics")?;
            let items = data(value, 2)?;
            PoolEvent::NewAuction {
                auction_type: AuctionType::try_from(as_u32(topic(topics, 1)?)?)?,
                user: as_address(topic(topics, 2)?)?,
                percent: as_u32(&items[0])?,
                auction: auction_value(&items[1])?,
            }
        }
        "fill_auction" => {
            require_topics(topics, 3, "fill_auction with 3 topics")?;
            let items = data(value, 3)?;
            PoolEvent::FillAuction {
                auction_type: AuctionType::try_from(as_u32(topic(topics, 1)?)?)?,
                user: as_address(topic(topics, 2)?)?,
                filler: as_address(&items[0])?,
                fill_percent: as_i128(&items[1])?,
                filled: auction_value(&items[2])?,
            }
        }
        "delete_auction" => {
            require_topics(topics, 3, "delete_auction with 3 topics")?;
            if !matches!(value, ScVal::Void) {
                return Err(shape("delete_auction with void data", value));
            }
            PoolEvent::DeleteAuction {
                auction_type: AuctionType::try_from(as_u32(topic(topics, 1)?)?)?,
                user: as_address(topic(topics, 2)?)?,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// The rest: `bad_debt`, `defaulted_debt`, the fork's `debt_setoff`,
/// `collateral_orphaned` and `orphan_settled`, `set_reserve` and
/// `set_status`, which share no common shape.
fn decode_admin_event(
    name: &str,
    topics: &[ScVal],
    value: &ScVal,
) -> Result<Option<PoolEvent>, XdrError> {
    let event = match name {
        "bad_debt" => {
            require_topics(topics, 3, "bad_debt with 3 topics")?;
            PoolEvent::BadDebt {
                user: as_address(topic(topics, 1)?)?,
                asset: as_address(topic(topics, 2)?)?,
                d_tokens: as_i128(value)?,
            }
        }
        "defaulted_debt" => {
            require_topics(topics, 2, "defaulted_debt with 2 topics")?;
            PoolEvent::DefaultedDebt {
                asset: as_address(topic(topics, 1)?)?,
                d_tokens: as_i128(value)?,
            }
        }
        "debt_setoff" => {
            require_topics(topics, 2, "debt_setoff with 2 topics")?;
            let items = data(value, 2)?;
            PoolEvent::DebtSetoff {
                asset: as_address(topic(topics, 1)?)?,
                b_tokens_burned: as_i128(&items[0])?,
                d_tokens_repaid: as_i128(&items[1])?,
            }
        }
        "collateral_orphaned" => {
            require_topics(topics, 3, "collateral_orphaned with 3 topics")?;
            PoolEvent::CollateralOrphaned {
                user: as_address(topic(topics, 1)?)?,
                asset: as_address(topic(topics, 2)?)?,
                b_tokens: as_i128(value)?,
            }
        }
        "orphan_settled" => {
            require_topics(topics, 2, "orphan_settled with 2 topics")?;
            PoolEvent::OrphanSettled {
                asset: as_address(topic(topics, 1)?)?,
                b_tokens: as_i128(value)?,
            }
        }
        "set_reserve" => {
            require_topics(topics, 1, "set_reserve with 1 topic")?;
            let items = data(value, 2)?;
            PoolEvent::SetReserve {
                asset: as_address(&items[0])?,
                index: as_u32(&items[1])?,
            }
        }
        // Emitted with one topic by `update_status` and two by the admin's
        // `set_status`; the status itself is the data either way.
        "set_status" => {
            if !matches!(topics.len(), 1 | 2) {
                return Err(XdrError::Shape {
                    expected: "set_status with 1 or 2 topics",
                    got: format!("{} topics", topics.len()),
                });
            }
            PoolEvent::SetStatus {
                status: PoolStatus::try_from(as_u32(value)?)?,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// Decodes one contract event into a `PoolEvent`.
///
/// `topics` are the event's topics in order, the first being its name;
/// `value` is its data. Returns `Ok(None)` when the event is not one the bot
/// models: no topics at all and a first topic that is not a symbol mean the
/// same thing — this decoder does not recognise the event — and neither may
/// stall the poller with an error.
pub fn decode_pool_event(topics: &[ScVal], value: &ScVal) -> Result<Option<PoolEvent>, XdrError> {
    let Some(ScVal::Symbol(name)) = topics.first() else {
        return Ok(None);
    };
    let name = name.to_utf8_string_lossy();

    if let Some(event) = decode_position_event(&name, topics, value)? {
        return Ok(Some(event));
    }
    if let Some(event) = decode_auction_event(&name, topics, value)? {
        return Ok(Some(event));
    }
    decode_admin_event(&name, topics, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::xdr::encode::{address, i128_val, map, symbol};
    use crate::chain::xdr::from_base64;
    use crate::fixture::{mainnet_fixed_v2, text};
    use std::collections::BTreeMap;

    const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
    const EURC: &str = "CDTKPWPLOURQA2SGTKTUQOWRCBZEORB4BWBOMJ3D3ZTQQSGE5F6JBQLV";
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
    const FILLER: &str = "GCIH7OYR6LX6364PGGLAKGMZYLV37EAH6YXZAFK7RY7U4K7625XBH5EL";

    /// Decodes fixture event `index`.
    fn fixture_event(index: usize) -> Option<PoolEvent> {
        let fixture = mainnet_fixed_v2();
        let position = index.to_string();
        let raw = &fixture["events"][index];
        let topics: Vec<ScVal> = raw["topic"]
            .as_array()
            .expect("topics")
            .iter()
            .map(|topic| from_base64(topic.as_str().expect("base64")).expect("topic parses"))
            .collect();
        let value: ScVal =
            from_base64(text(&fixture, &["events", &position, "value"])).expect("value parses");
        decode_pool_event(&topics, &value).expect("decodes")
    }

    #[test]
    fn decodes_a_real_supply() {
        assert_eq!(
            fixture_event(0),
            Some(PoolEvent::Supply {
                asset: USDC.to_string(),
                from: "CDB2WMKQQNVZMEBY7Q7GZ5C7E7IAFSNMZ7GGVD6WKTCEWK7XOIAVZSAP".to_string(),
                amount: 102_008_939,
                b_tokens: 89_367_096,
            })
        );
    }

    #[test]
    fn decodes_a_real_supply_collateral() {
        assert_eq!(
            fixture_event(3),
            Some(PoolEvent::SupplyCollateral {
                asset: USDC.to_string(),
                from: "GCQFPCCKQ6SLE4Z56L2YZQVFMJAG3NY3VPGNGT477CNPQZ2LNVJQOG54".to_string(),
                amount: 19_762_500_000,
                b_tokens: 17_312_878_935,
            })
        );
    }

    #[test]
    fn decodes_a_real_borrow() {
        assert_eq!(
            fixture_event(6),
            Some(PoolEvent::Borrow {
                asset: EURC.to_string(),
                from: "GDU4ICJ4W23A4TPQZ6ORIAL2O7Y6ZG6BWWT4PNCJZEU3JZCFR37HTB4H".to_string(),
                amount: 150_000_000,
                d_tokens: 122_190_129,
            })
        );
    }

    #[test]
    fn decodes_a_real_repay() {
        assert_eq!(
            fixture_event(9),
            Some(PoolEvent::Repay {
                asset: XLM.to_string(),
                from: "CA4I5TPQAEF6C62B4UI7IDNDAPT5RUNSYGB6WSYNIQXHLG4JOFX2NSMH".to_string(),
                amount: 98_707_774,
                d_tokens: 98_555_365,
            })
        );
    }

    #[test]
    fn decodes_a_real_withdraw_collateral() {
        assert_eq!(
            fixture_event(12),
            Some(PoolEvent::WithdrawCollateral {
                asset: USDC.to_string(),
                from: "GCC4A2FN5BIXW6I57LKMP4XK7WVNZJWDCD5JZGGQKAI45PNTPC5NU6U4".to_string(),
                amount: 102_008_942,
                b_tokens: 89_367_100,
            })
        );
    }

    /// The auction data the synthetic auction events carry.
    fn auction_scval() -> ScVal {
        map(vec![
            (
                symbol("bid").expect("symbol"),
                map(vec![(address(USDC).expect("address"), i128_val(1_234))]).expect("bid"),
            ),
            (symbol("block").expect("symbol"), ScVal::U32(64_271_348)),
            (
                symbol("lot").expect("symbol"),
                map(vec![(address(XLM).expect("address"), i128_val(5_678))]).expect("lot"),
            ),
        ])
        .expect("auction")
    }

    fn expected_auction() -> AuctionData {
        AuctionData {
            bid: BTreeMap::from([(USDC.to_string(), 1_234)]),
            lot: BTreeMap::from([(XLM.to_string(), 5_678)]),
            block: 64_271_348,
        }
    }

    #[test]
    fn decodes_a_constructed_new_auction() {
        // Constructed, not captured: the pool had no liquidation in the RPC's
        // retained window when the fixture was taken. The shape is the
        // contract's `PoolEvents::new_auction`.
        let topics = vec![
            symbol("new_auction").expect("symbol"),
            ScVal::U32(0),
            address(USER).expect("address"),
        ];
        let value =
            crate::chain::xdr::encode::vec(vec![ScVal::U32(42), auction_scval()]).expect("data");
        assert_eq!(
            decode_pool_event(&topics, &value).expect("decodes"),
            Some(PoolEvent::NewAuction {
                auction_type: AuctionType::UserLiquidation,
                user: USER.to_string(),
                percent: 42,
                auction: expected_auction()
            })
        );
    }

    #[test]
    fn decodes_a_constructed_fill_auction() {
        let topics = vec![
            symbol("fill_auction").expect("symbol"),
            ScVal::U32(0),
            address(USER).expect("address"),
        ];
        let value = crate::chain::xdr::encode::vec(vec![
            address(FILLER).expect("address"),
            i128_val(75),
            auction_scval(),
        ])
        .expect("data");
        let event = decode_pool_event(&topics, &value)
            .expect("decodes")
            .expect("modelled");
        assert_eq!(
            event,
            PoolEvent::FillAuction {
                auction_type: AuctionType::UserLiquidation,
                user: USER.to_string(),
                filler: FILLER.to_string(),
                fill_percent: 75,
                filled: expected_auction(),
            }
        );
        // Both sides of a fill change positions, so both must be refreshed.
        assert_eq!(event.affected_accounts(), vec![USER, FILLER]);
    }

    #[test]
    fn decodes_a_constructed_delete_auction_and_bad_debt() {
        let topics = vec![
            symbol("delete_auction").expect("symbol"),
            ScVal::U32(0),
            address(USER).expect("address"),
        ];
        assert_eq!(
            decode_pool_event(&topics, &ScVal::Void).expect("decodes"),
            Some(PoolEvent::DeleteAuction {
                auction_type: AuctionType::UserLiquidation,
                user: USER.to_string()
            })
        );

        let topics = vec![
            symbol("bad_debt").expect("symbol"),
            address(USER).expect("address"),
            address(USDC).expect("address"),
        ];
        assert_eq!(
            decode_pool_event(&topics, &i128_val(9_999)).expect("decodes"),
            Some(PoolEvent::BadDebt {
                user: USER.to_string(),
                asset: USDC.to_string(),
                d_tokens: 9_999
            })
        );
    }

    #[test]
    fn decodes_a_constructed_debt_setoff() {
        // Fork `PoolEvents::debt_setoff`: 2 topics, data `(b_tokens_burned,
        // d_tokens_repaid)`.
        let topics = vec![
            symbol("debt_setoff").expect("symbol"),
            address(USDC).expect("address"),
        ];
        let value =
            crate::chain::xdr::encode::vec(vec![i128_val(1_200), i128_val(950)]).expect("data");
        assert_eq!(
            decode_pool_event(&topics, &value).expect("decodes"),
            Some(PoolEvent::DebtSetoff {
                asset: USDC.to_string(),
                b_tokens_burned: 1_200,
                d_tokens_repaid: 950,
            })
        );
    }

    #[test]
    fn decodes_a_constructed_orphan_settled() {
        // Fork `PoolEvents::orphan_settled`: 2 topics, scalar data.
        let topics = vec![
            symbol("orphan_settled").expect("symbol"),
            address(XLM).expect("address"),
        ];
        assert_eq!(
            decode_pool_event(&topics, &i128_val(77)).expect("decodes"),
            Some(PoolEvent::OrphanSettled {
                asset: XLM.to_string(),
                b_tokens: 77,
            })
        );
    }

    /// `collateral_orphaned` is the one fork event with three topics, and
    /// the borrower sits in topic 1 with the asset in topic 2 — the
    /// opposite order from `bad_debt`'s reading being wrong in a way no
    /// type catches, since both are addresses.
    #[test]
    fn collateral_orphaned_takes_the_user_from_topic_one() {
        let topics = vec![
            symbol("collateral_orphaned").expect("symbol"),
            address(USER).expect("address"),
            address(XLM).expect("address"),
        ];
        assert_eq!(
            decode_pool_event(&topics, &i128_val(4_321)).expect("decodes"),
            Some(PoolEvent::CollateralOrphaned {
                user: USER.to_string(),
                asset: XLM.to_string(),
                b_tokens: 4_321,
            })
        );
    }

    /// A modelled event with the wrong shape is an error, never `None`:
    /// `None` means "an event this bot does not model", and reading a
    /// changed shape as that would hide the change.
    #[test]
    fn a_fork_event_with_the_wrong_shape_is_an_error() {
        let two_topics = vec![
            symbol("collateral_orphaned").expect("symbol"),
            address(XLM).expect("address"),
        ];
        assert!(decode_pool_event(&two_topics, &i128_val(1)).is_err());

        let setoff = vec![
            symbol("debt_setoff").expect("symbol"),
            address(USDC).expect("address"),
        ];
        assert!(decode_pool_event(&setoff, &i128_val(1)).is_err());
    }

    /// Only `collateral_orphaned` names an account; the other two are
    /// reserve-wide and name nobody, like `defaulted_debt`.
    #[test]
    fn only_collateral_orphaned_names_an_account() {
        assert_eq!(
            PoolEvent::CollateralOrphaned {
                user: USER.to_string(),
                asset: XLM.to_string(),
                b_tokens: 1,
            }
            .affected_accounts(),
            vec![USER]
        );
        assert!(PoolEvent::DebtSetoff {
            asset: USDC.to_string(),
            b_tokens_burned: 1,
            d_tokens_repaid: 1,
        }
        .affected_accounts()
        .is_empty());
        assert!(PoolEvent::OrphanSettled {
            asset: XLM.to_string(),
            b_tokens: 1,
        }
        .affected_accounts()
        .is_empty());
    }

    #[test]
    fn an_unmodelled_event_is_none_rather_than_an_error() {
        // Pools emit events this bot has no use for, and new ones arrive with
        // contract upgrades. Neither may stop the poller.
        let topics = vec![symbol("gulp_emissions").expect("symbol")];
        assert_eq!(
            decode_pool_event(&topics, &i128_val(1)).expect("decodes"),
            None
        );
    }

    #[test]
    fn an_event_with_no_topics_is_not_a_pool_event() {
        // Zero topics and a non-symbol first topic mean the same thing to
        // this decoder: it does not recognise the event. Neither may error
        // the poller.
        assert_eq!(decode_pool_event(&[], &ScVal::Void).expect("decodes"), None);
    }

    #[test]
    fn decodes_a_flash_loan_event() {
        // `flash_loan` is the only arm that reads a fourth topic: the
        // contract that received the flash-minted debt. Topics are the
        // symbol, the asset, the borrower (`from`), then that contract;
        // data is `(amount, d_tokens)`, the same two-value shape `borrow`
        // carries, matching the contract's `PoolEvents::flash_loan`.
        let topics = vec![
            symbol("flash_loan").expect("symbol"),
            address(XLM).expect("address"),
            address(USER).expect("address"),
            address(FILLER).expect("address"),
        ];
        let value =
            crate::chain::xdr::encode::vec(vec![i128_val(500_000_000), i128_val(499_500_000)])
                .expect("data");
        let event = decode_pool_event(&topics, &value)
            .expect("decodes")
            .expect("modelled");
        assert_eq!(
            event,
            PoolEvent::FlashLoan {
                asset: XLM.to_string(),
                from: USER.to_string(),
                contract: FILLER.to_string(),
                amount: 500_000_000,
                d_tokens: 499_500_000,
            }
        );
        assert_eq!(event.affected_accounts(), vec![USER]);
    }

    #[test]
    fn a_modelled_event_with_the_wrong_shape_is_an_error() {
        let topics = vec![
            symbol("supply").expect("symbol"),
            address(USDC).expect("address"),
        ];
        assert!(matches!(
            decode_pool_event(&topics, &ScVal::Void),
            Err(XdrError::Shape { .. })
        ));
    }

    #[test]
    fn a_supply_event_with_a_surplus_topic_is_an_error() {
        // Four topics is one more than `supply`'s modelled shape (name,
        // asset, from); a decoder that only reads the first three would
        // silently accept it.
        let topics = vec![
            symbol("supply").expect("symbol"),
            address(USDC).expect("address"),
            address(USER).expect("address"),
            address(FILLER).expect("address"),
        ];
        let value = crate::chain::xdr::encode::vec(vec![i128_val(1), i128_val(1)]).expect("data");
        assert!(matches!(
            decode_pool_event(&topics, &value),
            Err(XdrError::Shape { .. })
        ));
    }

    #[test]
    fn a_delete_auction_with_non_void_data_is_an_error() {
        // The contract publishes `()` as `delete_auction`'s data; anything
        // else means this bot's model of the event is out of date.
        let topics = vec![
            symbol("delete_auction").expect("symbol"),
            ScVal::U32(0),
            address(USER).expect("address"),
        ];
        assert!(matches!(
            decode_pool_event(&topics, &i128_val(1)),
            Err(XdrError::Shape { .. })
        ));
    }

    #[test]
    fn an_unknown_auction_type_is_a_shape_error() {
        let topics = vec![
            symbol("new_auction").expect("symbol"),
            ScVal::U32(3),
            address(USER).expect("address"),
        ];
        let value =
            crate::chain::xdr::encode::vec(vec![ScVal::U32(42), auction_scval()]).expect("data");
        assert!(matches!(
            decode_pool_event(&topics, &value),
            Err(XdrError::Shape { .. })
        ));
    }

    #[test]
    fn affected_accounts_names_every_account_a_position_moved_for() {
        let supply = fixture_event(0).expect("modelled");
        assert_eq!(
            supply.affected_accounts(),
            vec!["CDB2WMKQQNVZMEBY7Q7GZ5C7E7IAFSNMZ7GGVD6WKTCEWK7XOIAVZSAP"]
        );
        let auction = PoolEvent::DeleteAuction {
            auction_type: AuctionType::UserLiquidation,
            user: USER.to_string(),
        };
        assert_eq!(auction.affected_accounts(), vec![USER]);
        let reserve = PoolEvent::SetReserve {
            asset: XLM.to_string(),
            index: 0,
        };
        assert!(reserve.affected_accounts().is_empty());
    }
}
