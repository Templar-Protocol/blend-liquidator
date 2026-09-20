# blend-liquidator on the ADR-0008 fork

**Status:** adopted direction. This document amends
`docs/specs/2026-09-04-blend-liquidator-bot-design.md` (the bot design spec,
"the design spec" below) wherever the two disagree. Everything the design spec
says that this document does not contradict still stands.

## Source of truth

`https://github.com/Templar-Protocol/blend-contracts-v2` is the source of
truth for pool behaviour, replacing `blend-capital/blend-contracts-v2`, which
Phases 1 through 7 were built against.

The bot targets the fork. Where the fork is byte-identical to stock — which is
most of it, including all of the arithmetic this crate ports — the existing
port stays valid and this document says so explicitly, so that a later reader
does not re-derive it.

## Verification basis

Every statement below was checked against source, not against the briefing
that prompted this document. Two trees were read side by side:

| Tree | Ref |
|---|---|
| Fork | `Templar-Protocol/blend-contracts-v2`, PR #3, branch `adr/0008-0011-local-security-fork`, head `54afdae` |
| Stock | `blend-capital/blend-contracts-v2` `main` @ `ba22b48`, which `git diff v2.0.0 HEAD -- pool/src backstop/src` proves equal to `v2.0.0` |

Line references in this document are the fork's unless a row says "stock".

**The fork is not deployable yet.** PR #3 is a draft whose own description
opens with "SECURITY GATE OPEN: not ready for publication, release,
deployment, activation, or handling funds." The repository has no releases and
no tags. The branch head may be re-pushed; §7 records what that costs.

---

## 1. What does not change

These files are **byte-identical** between fork and stock, and each one is
load-bearing for this crate:

| File | What it pins |
|---|---|
| `pool/src/errors.rs` | Every error code. 1200, 1205, 1210, 1211, 1212, 1213, 1214, 1220, 1221, 1222, 1224 keep their number and meaning. There are no new codes. |
| `pool/src/storage.rs` | `AuctionKey { auct_type: u32, user: Address }`, `PoolDataKey::Auction`, and **temporary** durability for auction get/has/set/del. Everything else persistent. |
| `pool/src/pool/submit.rs` | `validate_submit`: max positions, then `AuctionInProgress` (1212), then max-util, then — only when `check_health && has_liabilities()` — `is_hf_under(1_0000100)` → 1205, then `collateral_base < min_collateral` → 1224. |
| `pool/src/pool/reserve.rs`, `pool/src/pool/interest.rs` | `b_rate`/`d_rate` accrual, the SCALAR_12 rate scale, `to_b_token_up` and the `to_asset_from_*` conversions. **The `math::reserve` port stays valid.** |
| `pool/src/pool/health_factor.rs` | `PositionData`'s fields and `is_hf_over`/`is_hf_under`. |
| `pool/src/constants.rs`, `pool/src/validator.rs` | — |

So do these behaviours, which live in files that changed elsewhere:

- **The Dutch ramp is untouched.** `scale_auction`
  (`pool/src/auctions/auction.rs:216-233`) is stock, including the boundary:
  `block_dif > 200` takes the bid-decay branch, so at exactly 200 the lot is
  already whole *and* the bid is still whole, and decay starts at 201.
- **`auction.block` is the creation block plus one**
  (`pool/src/auctions/user_liquidation_auction.rs:32,37`), and `block_dif` is
  `ledger_sequence − auction.block` (`:220`). The bot's `start_ledger` already
  stores the block the event reported, which is that value.
- **The auction-creation maths is stock**: the incentive multiplier, the
  estimated-withdrawal cap, the full-liquidation branch and the health-factor
  bounds.
- **`new_auction`, `del_auction`, `submit`, `get_auction`, `get_positions`,
  `get_reserve` and `get_config` keep their signatures.**
- **`RequestType` 0–9 are unchanged**, and no new contract entry point exists.
- **`Positions { collateral, liabilities, supply }` is unchanged.**

`tests/fixtures/mainnet-fixed-v2.json` therefore needs no re-attestation for
the arithmetic it pins: it attests `get_reserve` and `get_positions`, both of
whose implementations are byte-identical on the fork.

### 1.1 The incentive multiplier, and what `lf` means

The design spec's `1 + (1 − cf / lf) / 2` is confirmed stock
(`pool/src/auctions/user_liquidation_auction.rs:108-125`), and
`math::liquidation` already implements it.

One trap, because the briefing that prompted this document invites it: `lf` is
**not** the reserve's liability factor. It is `effective_liabilities /
raw_liabilities`, which is the *inverse* of the weighted liability factor and
is therefore at least one. A pool that publishes a liability factor of 0.75
contributes `lf = 1.333`, not `0.75`. `cf` is `effective_collateral /
raw_collateral` and is at most one. Substituting published factors directly
into the formula produces an incentive that is wrong in the safe-looking
direction.

---

## 2. What changes

### 2.1 Default handling: debt is destroyed, not transferred

This is the fork's reason for existing and the one change that reaches the
bot's money math.

Stock moved a defaulted borrower's liabilities into the backstop's own
`Positions` and emitted `bad_debt(user, asset, d_tokens)`. The fork's
`check_and_handle_user_bad_debt` (`pool/src/pool/bad_debt.rs:50-113`) does
three things instead, in order, per liability asset:

1. **Set-off.** The borrower's own ordinary *supply* in the debt reserve
   repays as much of that debt as it covers. Emits `debt_setoff`.
2. **Default.** Whatever debt remains is destroyed by
   `User::default_liabilities` (`pool/src/pool/user.rs:102-119`), which
   removes the liability and then reduces the reserve's `b_rate` by
   `default_amount / b_supply`, rounded up, floored at zero. **Every supplier
   of that reserve takes the loss pro rata, in that same transaction.** Emits
   `defaulted_debt`.
3. **Confiscation.** Only when step 2 actually defaulted something, every
   remaining collateral position is moved to the pool contract's own address
   as `supply` — not as collateral. Emits `collateral_orphaned` per asset.

The gate on the whole path changed too. Stock asked whether the collateral map
was empty; the fork asks whether `PositionData::calculate_from_positions(…)
.collateral_raw != 0` (`pool/src/pool/bad_debt.rs:56-58`). That is an
**oracle-valued** test, so it prices the position, and a reserve whose price
is missing or stale now makes `bad_debt(user)` panic 1210 where stock would
never have priced at all.

### 2.2 A full fill can move the filler's own health factor

`fill_user_liq_auction` calls `check_and_handle_user_bad_debt` whenever
`is_full_fill` (`pool/src/auctions/user_liquidation_auction.rs:206-222`), and
`is_full_fill` is true when the scaled remainder is empty — in practice, a
100% fill. The call site is stock; what it now calls is §2.1's rewritten
function.

So on a fork pool, **a 100% fill can cut the `b_rate` of a reserve the filler
itself holds collateral in, inside the filler's own transaction, before
`validate_submit` runs its health check.** The filler's collateral is worth
less at check time than the snapshot it planned against said it would be.

This breaks an invariant the design spec's §5 states outright: that `plan_fill`
"projects the filler's own post-fill position exactly". On the fork the
projection is an **upper bound**, and the fork's own test suite proves a filler
refused for exactly this reason, with error 1205
(`test-suites/tests/test_pool_default_orphan_scenarios.rs:760-765,792-803`).

The existing recovery path happens to survive it: `Executor::execute` maps 1205
to `ExecOutcome::Replan`, the filler re-plans at half the percent, and a
half-percent fill is not a full fill, so the default never runs. But that works
by accident, costs a submission, and gives up the auction's other half. §4
makes it deliberate.

### 2.3 The bad-debt auction is unreachable; the `bad_debt` call matters more

`create_auction` and `fill_auction` both now panic 1200 for any auction type
other than `UserLiquidation` (`pool/src/auctions/auction.rs:82-97,150-157`).
On a fork pool:

- `new_auction(1, …)` and `new_auction(2, …)` raise **1200**.
- `RequestType::FillBadDebtAuction` (7) and `FillInterestAuction` (8) raise
  **1200**.

The enums, the storage keys and the read paths still carry all three types;
only create and fill reject. The briefing's "do not implement the stock
bad-debt-auction flow" is therefore correct and now contract-enforced.

**The `bad_debt(user)` entry point is the opposite case: it is kept, rewired,
and more useful than before.** It is the only way to clear a defaulted
borrower, and clearing one is what triggers the set-off and the supplier
haircut the pool depends on. It refuses the backstop address, refuses while a
user-liquidation auction is open, and panics 1200 when the borrower does not
actually qualify (`pool/src/pool/bad_debt.rs:8-27`). The bot keeps calling it.

### 2.4 Orphan custody and `gulp`

Confiscated collateral lands in `Positions.supply` for the pool's **own
contract address**, readable through the ordinary `get_positions` view.

`gulp` is repurposed entirely (`pool/src/pool/gulp.rs:7-30`). It is
permissionless, it always returns zero — so the unchanged wrapper always emits
`gulp(asset, 0)` — and it raises **1200** whenever the reserve has `d_supply >
0` or any b-token emissions state exists. Since a reserve that anybody borrows
from has `d_supply > 0`, orphaned collateral is dead capital until that reserve
is debt-free. It emits `orphan_settled` when it does retire some.

Two consequences for the bot, both small:

- The pool's own address becomes a `Positions` holder. It holds supply only,
  never collateral or liabilities, so `position_data` answers `None` and the
  tracker deletes the row — harmless. `create_user_liq_auction_data` already
  refuses `user == e.current_contract_address()` with 1211 (stock).
- The briefing's "exclude pool-custody b-tokens from available and withdrawable
  liquidity" is not a requirement this bot can fail: it models its own wallet
  and its own position, never pool-wide liquidity. It is recorded here so the
  omission is a decision rather than an oversight.

### 2.5 Fill timing: the free-fill window

The ramp is stock, so nothing about *when* an auction is worth filling changed
mechanically. What changed is that the endgame is now strictly better than the
bot assumes.

At `block_dif >= 400` the bid modifier is zero, and a zero-scaled bid is **not
stored as a zero entry** — `if to_fill_scaled > 0`
(`pool/src/auctions/auction.rs:246-249`) leaves the key absent, so
`to_fill_auction.bid` is an **empty map**. The filler takes the whole lot and
assumes no liabilities at all. The fork asserts this literally
(`test-suites/tests/test_pool_default_orphan_scenarios.rs:783`:
`assert!(filled.bid.is_empty())`). There is no zero-amount guard anywhere on
that path.

**There is no fill cutoff.** `fill_auction` guards only the auction type and
`user == filler_state.address`; `block_dif` appears nowhere in it, and inside
`scale_auction` only in the modifier arithmetic. So a fill at `block_dif` 400,
500 or 1000 is equally valid, and equally free, for as long as the auction
entry exists.

What happens at 500 is that `delete_stale_auction` stops refusing:
`if auction.block + 500 > e.ledger().sequence() { panic_with_error!(BadRequest) }`
(`pool/src/auctions/auction.rs:99-112`). It is stock, and permissionless
(`pool/src/contract.rs:571-577` has no `require_auth`), but it deletes nothing
by itself — somebody has to call it. So 500 is where waiting stops being a race
against another filler and starts being a race against anyone willing to spend
a transaction deleting the auction.

The bot today reaches the *start* of the free region and no further.
`plan_fill`'s gate is `earliest - start > RAMP_END_BLOCKS`
(`src/math/fill.rs:365`), strictly greater, so it can plan at exactly 400 and
answers `PastAuctionEnd` from 401 on unless the pool sets `force_fill`. An
auction this bot first sees at `block_dif` 450 is therefore refused outright,
although filling it would cost nothing. Separately, `fill_delay` searches for
the *earliest* ledger at which the lot covers the bid plus the margin, which is
the right objective against stock and the wrong one here: it pays a real bid to
win a race the bot could instead win for free a few minutes later.

It is a race, though, and that is the whole trade-off. Waiting to 400
maximises the take and forfeits it entirely to anyone who fills at 250. The bot
cannot model its competition, so §4 makes the objective a configured choice
rather than a guess, defaulting to the free fill the briefing asks for.

### 2.6 Narrowed entry points and new refusals

| Call | Fork behaviour | Reference |
|---|---|---|
| `flash_loan` | panics 1200 | `pool/src/contract.rs:478-485` |
| `update_pool` | panics 1200 | `pool/src/contract.rs:392-394` |
| `set_emissions_config` | panics 1200 | `pool/src/contract.rs:522-524` |
| backstop `distribute`, `gulp_emissions`, `add_reward`, `remove_reward`, `claim`, `drop` | panic 1200 (ADR-0011) | `backstop/src/contract.rs:262-284` |
| `execute_initialize` with non-zero `bstop_rate` | panics 1201 | `pool/src/pool/config.rs:27-29` |
| `set_status(4)` | succeeds before any threshold check, and is irreversible | `pool/src/pool/status.rs:11-14,71-78` |

The bot calls none of these. `flash_loan` matters only as a closed door: the
design spec's "no flash loans" is now the contract's position too, and the seam
§11 keeps is dead on a fork pool.

One new refusal reaches the bot, and one only looks as though it does:

- **`RequestType::Withdraw` (1) now health-checks** when the same user owes
  anything in that reserve (`pool/src/pool/actions.rs:319-323`), where stock
  had no such line. **This bot never sends that request type**, so nothing
  about its behaviour changes: `RequestType::Withdraw` appears once in the
  crate, in the discriminant-ordering test at `src/chain/xdr/encode.rs:346`.
  Both unwind actions and every fill request build `WithdrawCollateral` (3),
  `Repay`, `SupplyCollateral` or `FillUserLiquidationAuction`
  (`src/executor.rs:224-281`), and `WithdrawCollateral` already forced the
  check on stock. It is recorded because an unwind that fails 1205 must not be
  misdiagnosed as this, and because a future request builder could walk into
  it.
- **Supply caps.** ADR-0008 caps each reserve's stress-priced `supply_cap` at
  25,000 USD and a pool's sum at 50,000 USD. `apply_supply` and
  `apply_supply_collateral` raise **1220 `ExceededSupplyCap`**, which this
  crate special-cases nowhere. `plan_fill`'s supply-escalation step and
  `min_primary_collateral` can both reach a cap that no stock mainnet pool
  presented.

### 2.7 Oracle

The fork requires the oracle to report exactly 7 decimals and panics 1210
otherwise (`pool/src/pool/pool.rs:105-110`), and it now rejects a
**future-dated** price as well as one over 24 hours old (`:126-136`). The
24-hour boundary itself is unchanged.

`PositionData.scalar` is therefore always `10^7` on a fork pool. CLAUDE.md's
"prices are in the oracle's own decimals — read it, don't assume it" stays the
right discipline for the codec, but on a fork pool the normalisation is a
no-op.

---

## 3. Corrections to the briefing

The briefing that prompted this document is right about the direction and wrong
in five specifics. Recording them so they are not re-introduced:

1. **`defaulted_debt` is a stock event, not a fork addition.** It is verbatim
   in stock at `pool/src/events.rs:170-173`, and this crate already decodes it
   correctly at `src/chain/xdr/events.rs:351-357`. Only **three** events are
   new: `debt_setoff`, `collateral_orphaned` and `orphan_settled`. The fork
   emits `defaulted_debt` on the user path as well as the backstop path.
2. **`del_auction` and the 500-block staleness rule are stock**, byte-identical
   and permissionless. The bot could have used them all along.
3. **The health-factor window is closed, not open.** The comparisons are
   `is_hf_over(1_1500000)` with `>` and `is_hf_under(1_0300000)` with `<`
   (`pool/src/pool/health_factor.rs:93,106`), so a post-liquidation health
   factor of exactly 1.15 or exactly 1.03 is **accepted** and the accepted set
   is the closed interval `[1.03, 1.15]`. Read as an open interval,
   "(1.03, 1.15)" names the accepted set's *interior* and silently drops its
   two endpoints, which is the one place the distinction changes an answer.
   The crate's constants are the right numbers; §4-J is where the crate still
   reads the endpoints the old way.
4. **`bad_debt` is declared but never emitted** on the fork — zero call sites
   (`pool/src/events.rs:157-160`). The briefing's rule "if this pool emits
   stock's `bad_debt`, the wrong wasm is deployed" is therefore sound, and
   cheap to enforce.
5. **Do not read the briefing's `avg_lf` as a published liability factor.** See
   §1.1.

One further gap: the briefing describes the default path and the fill window
but not that the two interact. §2.2 is the consequence, and it is the single
most important fact in this document.

### 3.1 Event shapes

Decoder-ready. Each topic symbol is `ScVal::Symbol`; each amount is
`ScVal::I128`; a tuple datum is an `ScVal::Vec` of that many elements in order.

| Event | Topics | Topic 1 | Topic 2 | Data | Source |
|---|---|---|---|---|---|
| `debt_setoff` | 2 | asset | — | `Vec[I128 b_tokens_burned, I128 d_tokens_repaid]` | `pool/src/events.rs:175-180` |
| `collateral_orphaned` | 3 | **user** | asset | `I128` b_tokens | `pool/src/events.rs:182-187` |
| `orphan_settled` | 2 | asset | — | `I128` b_tokens | `pool/src/events.rs:189-195` |
| `defaulted_debt` (stock) | 2 | asset | — | `I128` d_tokens | `pool/src/events.rs:162-173` |

**The topic counts differ and the user is not in a fixed position.**
`collateral_orphaned` carries the borrower in topic 1 and the asset in topic 2;
`debt_setoff` and `orphan_settled` carry no user at all. A decoder that assumes
"the asset is always topic 1" mis-reads `collateral_orphaned` silently, because
both are addresses.

---

## 4. What the bot must change

Each item names the decision, not just the defect. These are the scope of the
implementation phase; none of them is done.

**A. Decode the three new events** in `chain::xdr::events`, with
`affected_accounts` answering the borrower for `collateral_orphaned` and no one
for the other two. A modelled event with an unexpected shape stays an error,
per that module's existing rule.

**B. Treat a `bad_debt` event as a deployment alarm.** On a fork pool it can
never be emitted, so seeing one means the pool is running stock wasm and every
assumption in this document is void. Decode it as today, and raise a
high-severity notification rather than acting on it. This is a new
`NotificationKind`, which per CLAUDE.md is also a new metric label.

**C. Fix the `Decision::BadDebt` predicate.** The bot decides on
`liability_base > 0 && collateral_base == 0`
(`src/auctioneer.rs:583-585`); the contract gates on `collateral_raw != 0`.
`collateral_base` is c-factor weighted and `collateral_raw` is not, so a
borrower holding a zero-collateral-factor reserve has `collateral_base == 0`
and `collateral_raw > 0`: the bot proposes, the contract answers 1200.
`accept_bad_debt` simulates first, so today this costs a simulation and a flag
that must be moved forward — not a transaction. Decide on `collateral_raw`.

**D. Retire the bad-debt *auction* surface.** `AuctionType::BadDebt` and
`Interest` can no longer be created or filled. Keep the decode path, since the
enum still exists on chain and an old pool may hold such a row, but stop the
bot from ever building one. `CreationKind::BadDebt` stays — it names the
`bad_debt(user)` call, which is very much alive — so this is narrower than it
sounds, and the store's schema does not move.

**E. Model the `b_rate` haircut in `plan_fill`, or refuse the fill that causes
it.** §2.2. The bot already holds the borrower's positions and the reserve, so
the defaulted amount and the resulting `b_rate` loss are computable exactly;
modelling it restores the "project exactly" invariant rather than leaning on a
1205 and a halved re-plan. Where modelling is not worth its complexity, the
fallback is explicit: do not fill 100% of an auction whose borrower the fill
would leave with liabilities and `collateral_raw == 0`, which is exactly the
contract's own gate read the other way round
(`pool/src/pool/bad_debt.rs:56-58` returns early unless both hold).

That predicate is **necessary but not sufficient**, and the difference is worth
not losing: reaching the default path is not the same as defaulting. Step 1's
set-off can clear the debt entirely from the borrower's own supply in the debt
reserve, in which case `had_default` stays false, no `defaulted_debt` is
emitted and `b_rate` never moves. So the fallback declines some fills that were
in fact safe. That is the safe direction, and it is a cost, not a free choice.
Modelling is what avoids paying it.

Whichever is chosen, it must be a decision in `math::fill` with a test — and if
it is the modelling, the test must pin the rounding, since `b_rate_loss` is
`fixed_div_ceil(default_amount, b_supply)` and rounds **up**
(`pool/src/pool/user.rs:102-119`). Not an emergent behaviour of the executor.

**F. Make the fill objective a configured choice.** §2.5. Add a per-pool
setting selecting between the earliest profitable ledger — today's behaviour,
correct where competition is real — and the free-fill point at `block_dif =
400`. Default to the free fill, per the briefing. Remove the upper bound on the
fillable window rather than moving it: the contract has no fill cutoff at 400,
at 500 or anywhere, so `PastAuctionEnd` should stop being a refusal and 500
should become a *staleness warning* — past it anyone may delete the auction, so
a plan aimed past 500 is racing a deletion rather than a filler. `force_fill`
then loses its "fill past the end" meaning and keeps only its delay cap; that
is a narrowing of a flag, and its doc comment and CLAUDE.md entry both have to
move with it.

**G. Handle 1220 `ExceededSupplyCap`.** `refusal` maps every code but 1205 and
1224 to `ExecOutcome::Refused` (`src/executor.rs:407-414`), and the filler's
handler for that variant does **not** re-plan: it counts
`SkipLabel::ContractError` and leaves the auction for the next tick
(`src/filler.rs:1141-1150`). So the executor is already correct and already
"skips with a reason a `SkipLabel` names" — do not add a re-plan to `Refused`,
which would change behaviour for every unhandled code at once.

The defect is a tick further out. `Filler::due` re-plans the row on the
`REPLAN_LEDGERS` cadence, the planner has no notion of a supply cap, so it
rebuilds the same over-cap supply and earns the same 1220 for as long as the
auction stays open. The acceptance criterion is therefore about the *plan*:
`plan_fill`'s supply-escalation step must know the reserve's remaining cap
headroom and size the supply under it, or decline the escalation and say so
with its own `FillSkip`. A 1220 reaching the executor at all should be the
unexpected case.

**H. Add the pool's own contract address to the bot's own-address set.** It is
a `Positions` holder now. It cannot be liquidated (1211, stock) and its row is
deleted for having no liabilities, so this saves a chain read per scan rather
than fixing a bug.

**I. Point the documentation at the fork.** CLAUDE.md's contract-derived
gotchas, the design spec's "Invariants specific to Blend", and the crate-level
docs all cite stock behaviour.

**J. Reconcile `TARGET_HF`'s band with the closed window.** §3's third
correction is not a stock-versus-fork difference, so §4-I's documentation
sweep does not cover it,
and it is not documentation alone: `src/config.rs:176` **enforces** the old
reading, refusing to start when `TARGET_HF >= 1.15`, while the contract accepts
exactly 1.15. Three doc comments state the same thing — `src/config.rs:149-150`
and `:166-167` ("at or above `1.15`", "`[1.03, 1.15)` is exactly the set of
values that name an outcome the contract can accept") and
`src/auctioneer.rs:980-982`.

The bound itself may well stay. Aiming a liquidation at the ceiling leaves no
room for the drift between planning and fill that `TARGET_HF`'s default of 1.06
exists to absorb, so one notch inside the contract's band is a defensible
choice. What cannot stay is the *reason given for it*: "The bounds are the
contract's own and nothing narrower" is now false, and a knob whose refusal
message misstates the contract will be widened by the next person who checks.
Either widen the bound to the contract's real band or keep it and say plainly
that it is the bot's own margin — but not both readings in one crate.

---

## 5. What stays deliberately unimplemented

- **`gulp`.** Permissionless, always returns zero, and refuses while the
  reserve carries any debt — so it is unreachable in practice and pays the bot
  nothing when it is reachable. Recorded as a seam, not a feature.
- **Pool-wide custody reconciliation** (`Σ pool supply == custody`). The bot
  models its own wallet and its own position, never pool liquidity. §2.4.
- **Harvesting orphaned collateral.** It is the pool's, not the bot's.
- **Anything ADR-0011 touches.** Its own scope statement puts liquidation
  semantics out of scope, and it traps only backstop emissions exports the bot
  never calls. Its one second-order effect is useful: a fork pool can never
  have b-token emissions, which is precisely the precondition
  `require_no_b_token_emissions` needs for orphan custody to work at all.

---

## 6. Sandbox and fixtures

**The sandbox stays on stock pins for now, and this is not a deferral that can
be quietly forgotten.** `scripts/sandbox/versions.env` pins five wasm by URL
and SHA-256 from a stock GitHub release. The fork publishes no release and no
tag, so there is nothing to pin, and its PR says in its first line that it is
not ready for deployment. Until that changes the sandbox exercises stock
semantics, and every §4 behaviour it would otherwise cover is uncovered.

When the fork does publish, three routes exist, cheapest first:

1. **Mixed pin.** ADR-0008 requires the pool factory to be byte-identical to
   stock and the fork's own differential asserts it, so the factory, Comet and
   the oracle mock keep their existing pins. Only `pool.wasm` and
   `backstop.wasm` move.
2. **Pin a fork release** once the PR leaves draft.
3. **Build from source and pin the commit.** The fork's `Makefile` builds and
   optimises; `versions.env` would carry a commit SHA rather than a URL and a
   hash, and the tier's "verified download" property would become "reproducible
   build" — which the fork claims and this repository has never exercised.

Either way `deploy.sh` needs two changes before it can stand a fork pool up:
`bstop_rate` must be zero at initialize (1201 otherwise), and the oracle must
report exactly 7 decimals, which it already does.

`tests/fixtures/mainnet-fixed-v2.json` needs no change: §1 shows the modules it
attests are byte-identical. It remains a stock-mainnet fixture, which is what
it claims to be.

---

## 7. Open questions

- **Which commit the fork branched from.** The PR body names a different pair
  of commits than the branch head that was read, and says its historical
  commits "do not identify the repaired successor". Everything here is verified
  against `54afdae`. **If the branch is re-pushed, re-verify before trusting
  this document.**
- **Whether ADR-0008's supply caps bind the pools this bot will serve.** The
  caps are per-reserve deployment configuration, not contract code, so only the
  deployment record answers it. §4-G is worth doing regardless.
- **Whether the fork's optimised `pool.wasm` fits the contract size limit.**
  Not verifiable without a build; the PR quotes historical sizes and disclaims
  them for the successor.
- **Where a fork pool will actually be deployed** — testnet, mainnet, or
  neither yet. The design spec's §9 soak is written against stock testnet
  pools and needs a target before it can be planned.

## 8. Delivery

This document is a spec amendment, not a plan. The work in §4 is one phase,
sized like the others to land as a single reviewable pull request, and it
sequences **before** the design spec's Phase 8, because Phase 8 writes the
documentation set and the deployment contract — both of which would otherwise
describe stock semantics and have to be rewritten immediately.

The design spec's phase list therefore becomes:

8. The ADR-0008 fork reconciliation: §4 A through J.
9. Documentation (`README`, `docs/configuration.md`, `docs/deploy.md`,
   `docs/architecture.md`), `CHANGELOG`, the deployment contract, first release
   tag.

The sandbox retarget in §6 is not in either phase. It is gated on the fork
publishing an artifact, and it carries its own risk: the tier is the only place
in this repository that signs and sends a transaction.
