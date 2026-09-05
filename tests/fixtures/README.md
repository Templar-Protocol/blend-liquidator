# Test fixtures

`mainnet-fixed-v2.json` is a snapshot of the Blend v2 mainnet pool
`CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD` ("Fixed") taken
at ledger 64271347 (close time 1788534414). It holds, all as base64 XDR:

- the pool's contract instance and `ResList` ledger entries;
- each reserve's `ResConfig` and `ResData` entries, the contract's own
  `get_reserve` answer at the same ledger, and the oracle's `lastprice`;
- the oracle's `decimals`;
- two borrowers' `Positions` entries and `get_positions` answers;
- fifteen real pool events (`supply`, `supply_collateral`, `borrow`,
  `repay`, `withdraw_collateral`) from the retained history.

Every entry and simulation was taken at one ledger: the capture re-fetches
the entries after the simulations and requires byte equality, retrying
otherwise. That is what lets tests assert that accruing the stored entries
to the close time reproduces `get_reserve` exactly.

## Refreshing

```bash
cargo run --example capture_fixture -- \
  https://mainnet.sorobanrpc.com \
  CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD \
  tests/fixtures/mainnet-fixed-v2.json \
  GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE \
  GCIH7OYR6LX6364PGGLAKGMZYLV37EAH6YXZAFK7RY7U4K7625XBH5EL
```

The tool needs `curl` on the PATH. A refresh changes every literal the
tests assert (rates, prices, position values), so refresh only when a
contract upgrade changes a shape, and update the literals in the same
change. Borrower addresses can be found with the public Blend analytics
API: `GET https://api.blend.templarfi.org/v1/analytics/state/positions?healthFactorMax=100&poolId=<pool>&limit=5`.
