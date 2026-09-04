//! Ledger keys for the Blend v2 pool's storage.
//!
//! Durability is part of the key: pool configuration, reserves and positions
//! live in persistent storage, auctions in temporary storage. Reading an
//! auction with the persistent durability returns nothing rather than an
//! error, which is why these are constructed in one place and pinned by
//! wire-format tests.

use stellar_xdr::{ContractDataDurability, LedgerKey, LedgerKeyContractData, ScVal};

use super::encode::{address, map, sc_address, symbol, vec};
use super::XdrError;

fn contract_data(
    pool: &str,
    key: ScVal,
    durability: ContractDataDurability,
) -> Result<LedgerKey, XdrError> {
    Ok(LedgerKey::ContractData(LedgerKeyContractData {
        contract: sc_address(pool)?,
        key,
        durability,
    }))
}

/// The pool's contract instance: admin, backstop, BLND token, name, config.
pub fn instance(pool: &str) -> Result<LedgerKey, XdrError> {
    contract_data(
        pool,
        ScVal::LedgerKeyContractInstance,
        ContractDataDurability::Persistent,
    )
}

/// `ResList`: the reserve addresses, in the index order positions use.
pub fn reserve_list(pool: &str) -> Result<LedgerKey, XdrError> {
    contract_data(pool, symbol("ResList")?, ContractDataDurability::Persistent)
}

/// `ResConfig(asset)`: the reserve's factors and rate curve.
pub fn reserve_config(pool: &str, asset: &str) -> Result<LedgerKey, XdrError> {
    let key = vec(vec![symbol("ResConfig")?, address(asset)?])?;
    contract_data(pool, key, ContractDataDurability::Persistent)
}

/// `ResData(asset)`: the reserve's rates and supplies as of its last update.
pub fn reserve_data(pool: &str, asset: &str) -> Result<LedgerKey, XdrError> {
    let key = vec(vec![symbol("ResData")?, address(asset)?])?;
    contract_data(pool, key, ContractDataDurability::Persistent)
}

/// `Positions(user)`: one user's collateral, liabilities and supply.
pub fn positions(pool: &str, user: &str) -> Result<LedgerKey, XdrError> {
    let key = vec(vec![symbol("Positions")?, address(user)?])?;
    contract_data(pool, key, ContractDataDurability::Persistent)
}

/// `Auction(AuctionKey { user, auct_type })`, in temporary storage.
/// `auction_type` is 0 for a user liquidation, 1 for bad debt, 2 for interest.
pub fn auction(pool: &str, user: &str, auction_type: u32) -> Result<LedgerKey, XdrError> {
    let auction_key = map(vec![
        (symbol("auct_type")?, ScVal::U32(auction_type)),
        (symbol("user")?, address(user)?),
    ])?;
    let key = vec(vec![symbol("Auction")?, auction_key])?;
    contract_data(pool, key, ContractDataDurability::Temporary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::xdr::encode::to_base64;
    use stellar_xdr::{ContractDataDurability, LedgerKey, Limits, ReadXdr};

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";

    #[test]
    fn instance_key_matches_the_wire_format() {
        let key = to_base64(&instance(POOL).expect("key")).expect("base64");
        assert_eq!(
            key,
            "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAABQAAAAB"
        );
    }

    #[test]
    fn reserve_list_key_matches_the_wire_format() {
        let key = to_base64(&reserve_list(POOL).expect("key")).expect("base64");
        assert_eq!(
            key,
            "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAAA8AAAAHUmVzTGlzdAAAAAAB"
        );
    }

    #[test]
    fn reserve_keys_match_the_wire_format() {
        let config = to_base64(&reserve_config(POOL, XLM).expect("key")).expect("base64");
        assert_eq!(config, "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAABAAAAABAAAAAgAAAA8AAAAJUmVzQ29uZmlnAAAAAAAAEgAAAAEltPzYWa7C+mNIQ4xImzw8EMmLbSG+T9PLMMtolT75dwAAAAE=");
        let data = to_base64(&reserve_data(POOL, XLM).expect("key")).expect("base64");
        assert_eq!(data, "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAABAAAAABAAAAAgAAAA8AAAAHUmVzRGF0YQAAAAASAAAAASW0/NhZrsL6Y0hDjEibPDwQyYttIb5P08swy2iVPvl3AAAAAQ==");
    }

    #[test]
    fn positions_key_matches_the_wire_format() {
        let key = to_base64(&positions(POOL, USER).expect("key")).expect("base64");
        assert_eq!(key, "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAABAAAAABAAAAAgAAAA8AAAAJUG9zaXRpb25zAAAAAAAAEgAAAAAAAAAAwWvxVekgt/bc4cnQA7BvYRCl1YIAjrruP0ttEbPr7t0AAAAB");
    }

    #[test]
    fn auction_key_matches_the_wire_format() {
        let key = to_base64(&auction(POOL, USER, 0).expect("key")).expect("base64");
        assert_eq!(key, "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAABAAAAABAAAAAgAAAA8AAAAHQXVjdGlvbgAAAAARAAAAAQAAAAIAAAAPAAAACWF1Y3RfdHlwZQAAAAAAAAMAAAAAAAAADwAAAAR1c2VyAAAAEgAAAAAAAAAAwWvxVekgt/bc4cnQA7BvYRCl1YIAjrruP0ttEbPr7t0AAAAA");
    }

    #[test]
    fn state_is_persistent_and_auctions_are_temporary() {
        // The durability is part of the key: asking for an auction in
        // persistent storage silently finds nothing.
        for key in [
            instance(POOL),
            reserve_list(POOL),
            reserve_config(POOL, XLM),
            reserve_data(POOL, XLM),
            positions(POOL, USER),
        ] {
            match key.expect("key") {
                LedgerKey::ContractData(data) => {
                    assert_eq!(data.durability, ContractDataDurability::Persistent);
                }
                other => panic!("expected contract data, got {other:?}"),
            }
        }
        match auction(POOL, USER, 0).expect("key") {
            LedgerKey::ContractData(data) => {
                assert_eq!(data.durability, ContractDataDurability::Temporary);
            }
            other => panic!("expected contract data, got {other:?}"),
        }
    }

    #[test]
    fn a_key_round_trips_through_base64() {
        let key = positions(POOL, USER).expect("key");
        let text = to_base64(&key).expect("base64");
        let parsed = LedgerKey::from_xdr_base64(&text, Limits::none()).expect("parses");
        assert_eq!(parsed, key);
    }

    #[test]
    fn a_bad_pool_address_is_an_error_not_a_panic() {
        assert!(matches!(
            instance("not-an-address"),
            Err(crate::chain::xdr::XdrError::Xdr(_))
        ));
    }
}
