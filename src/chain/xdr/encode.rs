//! Building the ScVals, operations and envelopes the bot sends.
//!
//! Two rules the host enforces and this module keeps: a contract map must be
//! sorted by key, and a `Symbol` is at most 32 bytes. Both are errors here
//! rather than surprises at simulation time.

use stellar_xdr::{
    HostFunction, InvokeContractArgs, InvokeHostFunctionOp, Limits, Memo, MuxedAccount, Operation,
    OperationBody, Preconditions, ReadXdr, ScAddress, ScMap, ScSymbol, ScVal, ScVec,
    SequenceNumber, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope,
    Uint256, VecM, WriteXdr,
};

use super::XdrError;

/// A contract `Symbol`, at most 32 bytes.
pub fn symbol(text: &str) -> Result<ScVal, XdrError> {
    ScSymbol::try_from(text)
        .map(ScVal::Symbol)
        .map_err(|()| XdrError::Symbol(text.to_string()))
}

/// A strkey (`C…` contract or `G…` account) as an `ScAddress`.
pub fn sc_address(strkey: &str) -> Result<ScAddress, XdrError> {
    strkey.parse().map_err(XdrError::Xdr)
}

/// A strkey as an `ScVal::Address`.
pub fn address(strkey: &str) -> Result<ScVal, XdrError> {
    Ok(ScVal::Address(sc_address(strkey)?))
}

/// A contract vector.
pub fn vec(items: Vec<ScVal>) -> Result<ScVal, XdrError> {
    Ok(ScVal::Vec(Some(ScVec::try_from(items)?)))
}

/// A contract map, sorted by key as the host requires.
pub fn map(entries: Vec<(ScVal, ScVal)>) -> Result<ScVal, XdrError> {
    Ok(ScVal::Map(Some(ScMap::sorted_from(entries)?)))
}

/// An `i128` as an `ScVal`. The XDR carries it as a signed high half and an
/// unsigned low half; `stellar-xdr` owns that split, so this is a named
/// wrapper rather than hand-rolled bit twiddling.
pub fn i128_val(value: i128) -> ScVal {
    ScVal::from(value)
}

/// The SEP-40 `Asset::Stellar(address)` an oracle's `lastprice` takes.
pub fn stellar_asset(asset: &str) -> Result<ScVal, XdrError> {
    vec(vec![symbol("Stellar")?, address(asset)?])
}

/// One `InvokeHostFunction` operation calling `function` on `contract`.
/// Authorisation is empty: simulation fills it in, and the bot's own calls
/// are covered by the source account's signature.
pub fn invoke_contract_op(
    contract: &str,
    function: &str,
    args: Vec<ScVal>,
) -> Result<Operation, XdrError> {
    let ScVal::Symbol(function_name) = symbol(function)? else {
        return Err(XdrError::Symbol(function.to_string()));
    };
    Ok(Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: sc_address(contract)?,
                function_name,
                args: VecM::try_from(args)?,
            }),
            auth: VecM::default(),
        }),
    })
}

/// Wraps an operation in an unsigned envelope for `simulateTransaction`.
///
/// The source account is all zeroes and the sequence number is zero: the RPC
/// does not validate either when simulating, and using a real account would
/// make every read depend on that account existing and being funded.
pub fn simulation_envelope(operation: Operation) -> Result<TransactionEnvelope, XdrError> {
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256([0_u8; 32])),
        fee: 100,
        seq_num: SequenceNumber(0),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: VecM::try_from(vec![operation])?,
        ext: TransactionExt::V0,
    };
    Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    }))
}

/// The depth and length an XDR read or write is bounded to.
///
/// The network caps a ledger entry and a transaction far below 1 MiB, and
/// Soroban caps `ScVal` nesting at 100, so these bounds never reject a value
/// the chain can actually produce — they only bound what a hostile or
/// broken RPC response can make the decoder allocate or recurse into.
pub const XDR_LIMITS: Limits = Limits {
    depth: 500,
    len: 1_048_576,
};

/// Base64 for the wire, bounded by `XDR_LIMITS`.
pub fn to_base64<T: WriteXdr>(value: &T) -> Result<String, XdrError> {
    value.to_xdr_base64(XDR_LIMITS).map_err(XdrError::Xdr)
}

/// The inverse of `to_base64`.
pub fn from_base64<T: ReadXdr>(text: &str) -> Result<T, XdrError> {
    T::from_xdr_base64(text, XDR_LIMITS).map_err(XdrError::Xdr)
}

/// The pool's `Request.request_type` discriminants, in the contract's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestType {
    /// 0: supply to the pool (not as collateral).
    Supply,
    /// 1: withdraw supplied tokens.
    Withdraw,
    /// 2: supply as collateral.
    SupplyCollateral,
    /// 3: withdraw collateral.
    WithdrawCollateral,
    /// 4: borrow.
    Borrow,
    /// 5: repay.
    Repay,
    /// 6: fill a user liquidation auction; `address` is the liquidated
    /// user, `amount` the percent to fill, 1 to 100.
    FillUserLiquidationAuction,
    /// 7: fill a bad-debt auction.
    FillBadDebtAuction,
    /// 8: fill an interest auction.
    FillInterestAuction,
    /// 9: delete a liquidation auction whose user is healthy again.
    DeleteLiquidationAuction,
}

impl RequestType {
    /// The contract's number for this request type.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            Self::Supply => 0,
            Self::Withdraw => 1,
            Self::SupplyCollateral => 2,
            Self::WithdrawCollateral => 3,
            Self::Borrow => 4,
            Self::Repay => 5,
            Self::FillUserLiquidationAuction => 6,
            Self::FillBadDebtAuction => 7,
            Self::FillInterestAuction => 8,
            Self::DeleteLiquidationAuction => 9,
        }
    }
}

/// One entry of a `submit` call's request list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// What to do.
    pub request_type: RequestType,
    /// The asset for supply, withdraw, borrow and repay; the user for the
    /// auction requests.
    pub address: String,
    /// The amount in the asset's decimals for supply, withdraw, borrow and
    /// repay. The three fill request types instead carry a percent, 1 to
    /// 100 — `Request::fill` is the way to build one of those, so this
    /// field need not be set directly by a caller filling an auction.
    pub amount: i128,
}

/// The contract's `Request` struct: a map keyed `address`, `amount`,
/// `request_type`, which is the order Soroban sorts the symbols into.
pub fn request(request: &Request) -> Result<ScVal, XdrError> {
    map(vec![
        (symbol("address")?, address(&request.address)?),
        (symbol("amount")?, i128_val(request.amount)),
        (
            symbol("request_type")?,
            ScVal::U32(request.request_type.code()),
        ),
    ])
}

/// A fill or auction percent, 1 to 100, the contract's range for
/// `new_auction`'s `percent` and the `amount` of the three fill requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillPercent(u32);

impl FillPercent {
    /// The value, known to be 1 to 100.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for FillPercent {
    type Error = XdrError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if (1..=100).contains(&value) {
            Ok(Self(value))
        } else {
            Err(XdrError::Shape {
                expected: "percent 1..=100",
                got: value.to_string(),
            })
        }
    }
}

impl Request {
    /// A fill request: `FillUserLiquidationAuction`, `FillBadDebtAuction` or
    /// `FillInterestAuction`, the three request types that carry a user and
    /// a fill percent rather than an asset and an amount. Any other
    /// request type is refused here, before a request the contract would
    /// reject is ever built.
    pub fn fill(
        request_type: RequestType,
        user: &str,
        percent: FillPercent,
    ) -> Result<Self, XdrError> {
        match request_type {
            RequestType::FillUserLiquidationAuction
            | RequestType::FillBadDebtAuction
            | RequestType::FillInterestAuction => Ok(Self {
                request_type,
                address: user.to_string(),
                amount: i128::from(percent.get()),
            }),
            other => Err(XdrError::Shape {
                expected: "a fill request type",
                got: format!("{other:?}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";

    #[test]
    fn symbols_match_the_wire_format() {
        assert_eq!(
            to_base64(&symbol("new_auction").expect("symbol")).expect("base64"),
            "AAAADwAAAAtuZXdfYXVjdGlvbgA="
        );
    }

    #[test]
    fn a_symbol_over_32_bytes_is_an_error() {
        let long = "a".repeat(33);
        assert!(matches!(symbol(&long), Err(XdrError::Symbol(_))));
        assert!(symbol(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn a_stellar_asset_is_a_two_element_enum_vector() {
        let encoded = to_base64(&stellar_asset(XLM).expect("asset")).expect("base64");
        assert_eq!(
            encoded,
            "AAAAEAAAAAEAAAACAAAADwAAAAdTdGVsbGFyAAAAABIAAAABJbT82FmuwvpjSEOMSJs8PBDJi20hvk/TyzDLaJU++Xc="
        );
    }

    #[test]
    fn i128_values_round_trip_through_their_high_and_low_halves() {
        for value in [0_i128, 1, -1, i128::MAX, i128::MIN, 1_228_743_739_744] {
            let encoded = to_base64(&i128_val(value)).expect("base64");
            let decoded: ScVal = from_base64(&encoded).expect("parses");
            match decoded {
                ScVal::I128(parts) => {
                    assert_eq!((i128::from(parts.hi) << 64) | i128::from(parts.lo), value);
                }
                other => panic!("expected i128, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_map_is_sorted_by_key_as_the_host_requires() {
        let unsorted = map(vec![
            (symbol("user").expect("symbol"), ScVal::U32(1)),
            (symbol("auct_type").expect("symbol"), ScVal::U32(0)),
        ])
        .expect("map");
        match unsorted {
            ScVal::Map(Some(entries)) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].key, symbol("auct_type").expect("symbol"));
                assert_eq!(entries[1].key, symbol("user").expect("symbol"));
            }
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[test]
    fn a_simulation_envelope_carries_one_invoke_operation() {
        let operation =
            invoke_contract_op(POOL, "get_reserve", vec![address(XLM).expect("address")])
                .expect("op");
        let envelope = simulation_envelope(operation).expect("envelope");
        let text = to_base64(&envelope).expect("base64");
        let parsed: TransactionEnvelope = from_base64(&text).expect("parses");
        let TransactionEnvelope::Tx(v1) = parsed else {
            panic!("expected a v1 envelope")
        };
        assert_eq!(v1.tx.operations.len(), 1);
        assert!(v1.signatures.is_empty(), "a simulation is never signed");
        let OperationBody::InvokeHostFunction(invoke) = &v1.tx.operations[0].body else {
            panic!("expected an invoke-host-function operation")
        };
        let HostFunction::InvokeContract(args) = &invoke.host_function else {
            panic!("expected a contract invocation")
        };
        assert_eq!(args.function_name, symbol_name("get_reserve"));
        assert_eq!(args.args.len(), 1);
    }

    /// The bare `ScSymbol` for a name, for comparing against a decoded call.
    fn symbol_name(text: &str) -> ScSymbol {
        match symbol(text).expect("symbol") {
            ScVal::Symbol(name) => name,
            other => panic!("expected a symbol, got {other:?}"),
        }
    }

    #[test]
    fn request_types_carry_the_contracts_numbers_in_order() {
        let all = [
            RequestType::Supply,
            RequestType::Withdraw,
            RequestType::SupplyCollateral,
            RequestType::WithdrawCollateral,
            RequestType::Borrow,
            RequestType::Repay,
            RequestType::FillUserLiquidationAuction,
            RequestType::FillBadDebtAuction,
            RequestType::FillInterestAuction,
            RequestType::DeleteLiquidationAuction,
        ];
        for (expected, request_type) in (0_u32..).zip(all) {
            assert_eq!(request_type.code(), expected);
        }
    }

    #[test]
    fn a_request_is_a_struct_map_with_sorted_keys() {
        let user = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
        let value = request(&Request {
            request_type: RequestType::FillUserLiquidationAuction,
            address: user.to_string(),
            amount: 60,
        })
        .expect("encodes");
        let ScVal::Map(Some(entries)) = value else {
            panic!("expected a map")
        };
        let keys: Vec<String> = entries
            .iter()
            .map(|entry| match &entry.key {
                ScVal::Symbol(symbol) => symbol.to_utf8_string_lossy(),
                other => panic!("non-symbol key {other:?}"),
            })
            .collect();
        assert_eq!(keys, ["address", "amount", "request_type"]);
        assert_eq!(entries[0].val, address(user).expect("address"));
        assert_eq!(entries[1].val, i128_val(60));
        assert_eq!(entries[2].val, ScVal::U32(6));
    }

    #[test]
    fn a_fill_percent_outside_one_to_a_hundred_and_a_non_fill_request_type_are_refused() {
        for percent in [0, 101] {
            let error = FillPercent::try_from(percent).unwrap_err();
            assert!(
                matches!(
                    error,
                    XdrError::Shape {
                        expected: "percent 1..=100",
                        ..
                    }
                ),
                "{error:?}"
            );
        }
        let user = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
        let percent = FillPercent::try_from(50).expect("50 is in range");
        let error = Request::fill(RequestType::Supply, user, percent).unwrap_err();
        assert!(
            matches!(
                error,
                XdrError::Shape {
                    expected: "a fill request type",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn fill_builds_a_request_for_each_of_the_three_fill_types() {
        let user = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
        let percent = FillPercent::try_from(60).expect("60 is in range");
        for request_type in [
            RequestType::FillUserLiquidationAuction,
            RequestType::FillBadDebtAuction,
            RequestType::FillInterestAuction,
        ] {
            let built = Request::fill(request_type, user, percent).expect("a fill type");
            assert_eq!(built.request_type, request_type);
            assert_eq!(built.address, user);
            assert_eq!(built.amount, 60);
        }
    }
}
