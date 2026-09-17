# Phase 7: The Sandbox Integration Tier Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An ignored integration test that stands up a private Stellar network with the Blend v2 contracts deployed, runs the real `liquidator` binary live against a borrower it has made liquidatable, and asserts that the bot created the auction, filled it, unwound to the wallet and recorded both — plus the nightly workflow that runs it and the dev-container additions the spec assigned to this phase.

**Architecture:** Shell does the chain setup, Rust does the assertions. `scripts/sandbox/` fetches pinned artefacts, starts a `stellar/quickstart --local` container, and deploys tokens, a Comet liquidity pool, the backstop, the pool factory, the pool, its two reserves and a mock SEP-40 oracle through the `stellar` CLI — the same sequence the contracts' own test fixture performs — writing every address and the sandbox keys to `target/sandbox/sandbox.env`. `tests/liquidation_sandbox.rs` (`#[ignore]`) reads that file, spawns the built binary armed against the sandbox, crashes the collateral price through the oracle, and polls the store, the chain and `/metrics` until the auction, the fill and the unwind have all happened. `.github/workflows/sandbox.yml` runs it nightly and on demand, outside `CI Summary`.

**Tech Stack:** `stellar` CLI 28.0.0 (prebuilt release binary), `stellar/quickstart` (pinned digest), Blend contracts v2.0.0 (prebuilt release wasm), `comet.wasm` from the same repository, `mock_sep_40_oracle.wasm` from `script3/sep-40-oracle` v1.2.0, bash + shellcheck, the existing crate (`reqwest`, `sqlx`, `tokio`, `serde_json` as dev-dependencies).

**Spec:** `docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md` — §9 "Integration", §12 phase 7, §2 process shape; CLAUDE.md's gotcha on the `stellar` CLI and the cgroup-aware build cap.

## Global Constraints

- **The sandbox never touches a public network.** Every script refuses to run unless the RPC's `getNetwork` answers the standalone passphrase `Standalone Network ; February 2017`; the test refuses to start the bot with any other `NETWORK_PASSPHRASE`. The bot is armed (`DRY_RUN=false`) only here, against a network that exists for the duration of the run.
- **Sandbox keys are throwaway and never printed.** `stellar keys generate` creates them per run; the filler's secret reaches the bot through `target/sandbox/sandbox.env` (mode `0600`, under the git-ignored `target/`) and the process environment, never argv, never a log line, never the workflow log (`::add-mask::` before any step that could echo it).
- **Every artefact is pinned and verified**: wasm files by SHA-256 in `scripts/sandbox/versions.env`, the CLI tarball by SHA-256, the quickstart image by digest. A mismatch is a hard failure, never a warning.
- **`CI Summary` is untouched.** `sandbox.yml` runs on a schedule and `workflow_dispatch` only, like `devcontainer.yml` it is deliberately outside the gate (CLAUDE.md: a skipped `ci.yml` job is a failure precisely because nothing in `ci.yml` is conditional). The PR gate stays fmt, clippy, unit tests, docs, deny, docker, invariants, shellcheck.
- **The integration test is `#[ignore]`** and is never run by `cargo test --lib --bins` or `make check`; `cargo test --test liquidation_sandbox -- --ignored` is the only way in, and it fails fast with a clear message when `target/sandbox/sandbox.env` is absent.
- Actions are SHA-pinned as in `ci.yml`; scripts pass `shellcheck --severity=error`; `set -euo pipefail` everywhere; no secret in any `echo`.
- clippy pedantic, `unwrap`/`expect` only in tests (the integration test is a test); doc comments state constraints.
- Test count: 522 unit tests at this plan's base (`890b577`); the integration test adds one `#[test]` that is ignored.

---

## Facts the plan is written against

### Contract facts (blend-contracts-v2 v2.0.0, commit `c19abee`)

- Prebuilt, optimised wasm is published per contract on the `_cli22.0.1` tags — the bytes the mainnet contracts were deployed from:
  - `https://github.com/blend-capital/blend-contracts-v2/releases/download/v2.0.0_pool_cli22.0.1/pool_v2.0.0.wasm` (57 328 bytes)
  - `https://github.com/blend-capital/blend-contracts-v2/releases/download/v2.0.0_backstop_cli22.0.1/backstop_v2.0.0.wasm` (30 772 bytes)
  - `https://github.com/blend-capital/blend-contracts-v2/releases/download/v2.0.0_pool-factory_cli22.0.1/pool-factory_v2.0.0.wasm` (3 230 bytes)
  - `https://raw.githubusercontent.com/blend-capital/blend-contracts-v2/c19abee5b9be4f49e0cda9057e87d343e5dcc095/comet.wasm` (29 046 bytes) — the BLND:USDC liquidity pool the backstop token is a share of
  - `https://raw.githubusercontent.com/script3/sep-40-oracle/v1.2.0/sep-40/src/testutils/mock_sep_40_oracle.wasm` (9 963 bytes) — the mock the contracts' test suite uses (`sep-40-oracle` 1.2.0, `testutils`)
- The contracts take **constructor arguments** at deploy (`register_at(id, WASM, (args…))` in the fixture): backstop `(backstop_token, emitter, blnd_token, usdc_token, pool_factory, drop_list: Vec<(Address, i128)>)`; pool factory `(PoolInitMeta { backstop, pool_hash: BytesN<32>, blnd_id })`. **They reference each other**, so one address is precomputed: deploy with `--salt`, and `stellar contract id wasm --salt … --source-account …` (or the CLI's `--build-only` + inspection) gives the id before the deploy. The emitter is not deployed: the backstop only stores its address, and nothing this test exercises calls it — pass the admin's address.
- Pool status: `initialize` leaves the pool in `6` (Setup); `queue_set_reserve` in Setup unlocks immediately (`unlock_time += SECONDS_PER_WEEK` only outside Setup), so `set_reserve` follows at once. Borrowing (request type 4) needs `status ≤ 1` (`require_action_allowed`), and the admin may `set_status(0)` — or `update_status` will compute `0` — only when the backstop threshold is met: `(blnd/1e7)^4 × (usdc/1e7) ≥ 1e25`, i.e. the backstop's share of the Comet pool must be worth about 100k BLND and 100k USDC in the product sense. The fixture reaches it by `lp.join_pool(50_000 shares, [500_001 BLND, 12_501 USDC], funder)` after the pool was initialised with 1 000 BLND / 25 USDC for 100 shares, then `backstop.deposit(funder, pool, 50_000 shares)`, then `set_status(3)`/`update_status()`.
- `factory.deploy(admin, name, salt: BytesN<32>, oracle, backstop_take_rate: u32, max_positions: u32, min_collateral: i128) -> Address`.
- `ReserveConfig { index, decimals, c_factor, l_factor, util, max_util, r_base, r_one, r_two, r_three, reactivity, supply_cap: i128, enabled }` — the values to use are `default_reserve_metadata()` in the contracts' `test-suites/src/pool.rs` (a copy is in the controller's scratchpad as `v2src/ts_pool.rs`; the implementer of Task 4 gets them verbatim in the dispatch), with `c_factor 0.75 / l_factor 0.75 / util 0.5` for XLM and `0.9 / 0.95 / 0.85` for USDC, as the fixture sets them.
- `submit(from, spender, to, requests: Vec<Request { request_type: u32, address, amount: i128 }>)`; types: 0 Supply, 2 SupplyCollateral, 4 Borrow, 5 Repay, 3 WithdrawCollateral.
- Mock oracle: `set_data(admin, base: Asset, assets: Vec<Asset>, decimals: u32, resolution: u32)`, `set_price_stable(prices: Vec<i128>)`; `Asset` is `{"Stellar": "C…"}` or `{"Other": "USD"}` in the CLI's JSON.
- Comet: `init(controller, tokens: Vec<Address>, weights: Vec<i128>, balances: Vec<i128>, swap_fee: i128)` as the fixture calls it with `[0.8, 0.2]`, `[1 000 BLND, 25 USDC]`, `0.003`; `join_pool(pool_amount_out: i128, max_amounts_in: Vec<i128>, user)`. Argument names come from the wasm's own spec: `stellar contract info interface --wasm comet.wasm`.
- A Stellar Asset Contract's issuer cannot hold its own asset, so the sandbox uses two keys: `issuer` (deploys and mints `USDC` and `BLND`) and `admin` (owns the Comet pool, the backstop deposit, the pool, the oracle). XLM is the native asset's SAC (`stellar contract id asset --asset native`); friendbot funds every key with XLM.

### Tooling facts

- `stellar` CLI 28.0.0: `https://github.com/stellar/stellar-cli/releases/download/v28.0.0/stellar-cli-28.0.0-x86_64-unknown-linux-gnu.tar.gz` (and the `aarch64` twin); no checksum asset is published, so the SHA-256 is computed once and pinned in `versions.env` (Task 1 records it).
- `stellar/quickstart --local`: RPC at `http://localhost:8000/rpc`, friendbot at `http://localhost:8000/friendbot?addr=G…` (runs whenever `horizon` runs, which `rpc` implies), passphrase `Standalone Network ; February 2017`; the CLI's own `container start local` uses `docker.io/stellar/quickstart:latest` and `--enable rpc,horizon,lab`. The sandbox pins a versioned tag by digest (Task 3 resolves it with `docker buildx imagetools inspect`) and enables `core,rpc,horizon`.
- The dev container has docker-in-docker (`devcontainer.json` features), so the sandbox runs locally; `ci.yml`'s `shellcheck` job runs `shellcheck --severity=error scripts/*.sh .devcontainer/*.sh` — the glob must grow to `scripts/**/*.sh` (Task 6).
- This crate: `Args` accepts `NETWORK_PASSPHRASE` (or `NETWORK=mainnet|testnet`), `POOLS_TOML` inline, `SEED_FILE`, `DRY_RUN=false` requires `FILLER_SECRET_KEY`; the auctioneer falls back to the filler's key (one queue); `STARTUP_DELAY_LEDGERS`, `FULL_SCAN_LEDGERS`, `ORACLE_SCAN_LEDGERS`, `POLL_INTERVAL_MS`, `PORT` exist; `/healthz`, `/metrics` exist; the store has `creations` and `fills` tables with `tx_hash`; `PoolReader` reads positions. `env!("CARGO_BIN_EXE_liquidator")` is the built binary's path inside an integration test.

## File structure

- `scripts/sandbox/versions.env` — every pin (URLs, SHA-256s, the CLI version and tarball SHA-256s, the quickstart tag and digest). Sourced by every script and by `post-create.sh`.
- `scripts/sandbox/lib.sh` — shared helpers: `require_standalone_network`, `sha256_check`, `wait_for_rpc`, `sandbox_dir` (`target/sandbox`).
- `scripts/sandbox/fetch-artifacts.sh` — downloads the five wasm files into `target/sandbox/wasm/`, verifying each.
- `scripts/sandbox/up.sh` / `down.sh` — the quickstart container.
- `scripts/sandbox/deploy.sh` — the chain setup; writes `target/sandbox/sandbox.env`.
- `scripts/sandbox/crash.sh` — moves the oracle's XLM price; used by the test.
- `scripts/cargo-jobs.sh` — prints the cgroup-aware job count.
- `.devcontainer/post-create.sh` — the CLI install and the build-jobs config.
- `tests/liquidation_sandbox.rs` — the ignored end-to-end test.
- `.github/workflows/sandbox.yml` — nightly + dispatch.
- `Makefile` — `sandbox`, `sandbox-up`, `sandbox-deploy`, `sandbox-test`, `sandbox-down`.
- `CLAUDE.md`, `README.md`, `CHANGELOG.md` — docs.

## Rulings taken before execution

1. **Prebuilt release wasm, not a source build.** The `_cli22.0.1` release assets are the deployed bytes; building from source would need the contracts' pinned Rust 1.81 toolchain and `stellar contract optimize` and buy nothing but drift. Pinned by SHA-256.
2. **The emitter is not deployed.** The backstop stores the address it is given and nothing in this scenario calls it; the admin's address stands in. Cost if wrong: one more deploy.
3. **Two reserves: XLM (collateral, primary) and USDC (borrowed).** Spec §9 says two; these are the pair the example pool file already names, and XLM needs no minting.
4. **The scenario**: admin supplies 100 000 USDC; the borrower supplies 5 000 XLM as collateral at $0.10 and borrows 300 USDC (health factor ≈ 1.19 with XLM `c_factor` 0.75 and USDC `l_factor` 0.95); the crash moves XLM to $0.075 (health factor ≈ 0.89 < `LIQ_HF_THRESHOLD` 0.998). The filler holds 10 000 USDC (minted) and 10 000 XLM (friendbot) with `min_primary_collateral` 100 XLM and `default_profit_bps` 100, so the fill's repay clears the taken debt and the unwind withdraws the lot down to 100 XLM.
5. **The bot runs as a subprocess of the test**, the built binary with the sandbox environment — the same code path an operator runs — stopped with `SIGTERM`, exit code asserted `0`. The library is used only for reads (positions) and the store (assertions).
6. **The crash happens after the bot is up and healthy**, so the run exercises seeding a healthy borrower, the oracle scan flagging the move, the decision, the creation, the fill after the profit delay, and the unwind — with `ORACLE_SCAN_LEDGERS=5`, `FULL_SCAN_LEDGERS=10`, `STARTUP_DELAY_LEDGERS=0`, `POLL_INTERVAL_MS=500`.
7. **Assertions, in order, each with its own timeout**: `/healthz` 200 → a `creations` row for the borrower with a `tx_hash` → a `fills` row with a `tx_hash` → the filler's positions on chain hold no liabilities and at most `min_primary_collateral` (plus 1 %) of XLM collateral → `/metrics` shows `creations_total{result="succeeded"} 1`, `fills_total{result="succeeded"} 1`, `unwind_passes_total ≥ 1` → `SIGTERM` → exit 0. Five minutes overall.
8. **The sandbox keys are generated per run** by the script, live under `target/sandbox/`, and are refused by every script unless the RPC is the standalone network. This is the one place the repository ever generates or funds a signing key, and it is the spec's own requirement for this tier.
9. **The nightly workflow is not in `CI Summary`**, and a red run is a notification to the maintainers (the workflow's failure e-mail), not a PR blocker.
10. **The cgroup-aware cap** is `min(nproc, max(1, floor(memory_limit / 2 GiB)))`, written to `~/.cargo/config.toml` as `[build] jobs` by `post-create.sh` only when no such key exists; `CARGO_BUILD_JOBS` in the environment still overrides it, which is what the existing gotcha's one-liners do.

---

## Task 1: pins and the artefact fetch

**Files:**
- Create: `scripts/sandbox/versions.env`, `scripts/sandbox/lib.sh`, `scripts/sandbox/fetch-artifacts.sh`
- Modify: `.gitignore` only if `target/` is not already ignored (it is; verify)

**Interfaces:**
- Produces `versions.env` (sourced, `KEY=value`, no expansion tricks):
  ```bash
  BLEND_CONTRACTS_TAG=v2.0.0
  BLEND_CONTRACTS_COMMIT=c19abee5b9be4f49e0cda9057e87d343e5dcc095
  POOL_WASM_URL=… POOL_WASM_SHA256=…
  BACKSTOP_WASM_URL=… BACKSTOP_WASM_SHA256=…
  POOL_FACTORY_WASM_URL=… POOL_FACTORY_WASM_SHA256=…
  COMET_WASM_URL=… COMET_WASM_SHA256=…
  MOCK_ORACLE_WASM_URL=… MOCK_ORACLE_WASM_SHA256=…
  STELLAR_CLI_VERSION=28.0.0
  STELLAR_CLI_SHA256_X86_64=… STELLAR_CLI_SHA256_AARCH64=…
  QUICKSTART_IMAGE=docker.io/stellar/quickstart
  QUICKSTART_TAG=<versioned tag>  QUICKSTART_DIGEST=sha256:…
  SANDBOX_PASSPHRASE="Standalone Network ; February 2017"
  ```
  (the quickstart pins are filled by Task 3; Task 1 leaves them as `QUICKSTART_TAG=` / `QUICKSTART_DIGEST=` with a comment)
- `lib.sh`: `sandbox_dir()` → `<repo>/target/sandbox`; `sha256_check FILE EXPECTED` (fails with both hashes printed); `fetch URL DEST EXPECTED_SHA` (curl `-fsSL --retry 3`, then check); `log`, `die`.
- `fetch-artifacts.sh`: downloads the five wasm files into `$(sandbox_dir)/wasm/` — skipping any whose SHA-256 already matches — and prints their sizes.

- [ ] **Step 1:** Compute the pins: download each artefact once with `curl -fsSL` and `sha256sum` it; the byte sizes must match the ones in "Facts" (a size mismatch means a wrong URL, not a new hash). Download both CLI tarballs and hash them.
- [ ] **Step 2:** Write the three files. `fetch-artifacts.sh` is idempotent and offline-safe once the files are present.
- [ ] **Step 3:** `shellcheck --severity=error scripts/sandbox/*.sh`; run `scripts/sandbox/fetch-artifacts.sh` twice (second run downloads nothing); corrupt one file and confirm the script re-downloads and verifies; write a wrong hash into a copy of `versions.env` and confirm a hard failure.
- [ ] **Step 4:** Commit `feat(sandbox): pin the contract artefacts and fetch them verified`.

---

## Task 2: the dev container — the `stellar` CLI and the cgroup-aware build cap

**Files:**
- Create: `scripts/cargo-jobs.sh`
- Modify: `.devcontainer/post-create.sh` (two new steps after the toolchain step)

**Interfaces:**
- `scripts/cargo-jobs.sh`: prints an integer. Reads `CARGO_JOBS_NPROC` (default `nproc`) and `CARGO_JOBS_MEM_BYTES` (default: `/sys/fs/cgroup/memory.max` if it is a number, else `/sys/fs/cgroup/memory/memory.limit_in_bytes` if it is below `2^62`, else `/proc/meminfo`'s `MemTotal`) and prints `min(nproc, max(1, mem / 2 GiB))`. Pure shell, no bc (`$(( ))` on bytes fits in 64 bits).
- `post-create.sh` step 5: install the CLI — `source scripts/sandbox/versions.env`, pick the tarball by `uname -m` (`x86_64` → `x86_64-unknown-linux-gnu`, `aarch64` → `aarch64-unknown-linux-gnu`), download to a temp dir, `sha256_check`, extract the `stellar` binary to `~/.local/bin/stellar` (on `PATH` in this image), skip when `stellar --version` already reports `STELLAR_CLI_VERSION`. Non-fatal on failure (warn), like the other post-toolchain steps.
- step 6: `jobs=$(scripts/cargo-jobs.sh)`; if `~/.cargo/config.toml` has no `[build]` `jobs` key, append `[build]\njobs = <n>` with a comment naming the script; print the number.

- [ ] **Step 1:** A shell test for `cargo-jobs.sh`, `scripts/sandbox/test-cargo-jobs.sh` (run by hand and by Task 6's workflow): `CARGO_JOBS_NPROC=16 CARGO_JOBS_MEM_BYTES=$((8 * 1024**3))` → `4`; `…MEM_BYTES=$((1024**3))` → `1`; `CARGO_JOBS_NPROC=2 …MEM_BYTES=$((64 * 1024**3))` → `2`.
- [ ] **Step 2:** Implement both scripts; run `post-create.sh`'s two new steps in this container (they must be idempotent — run twice); `stellar --version` prints 28.0.0; `cat ~/.cargo/config.toml` shows the cap once.
- [ ] **Step 3:** `shellcheck --severity=error .devcontainer/*.sh scripts/*.sh scripts/sandbox/*.sh`; commit `feat(devcontainer): the stellar CLI, verified, and a cgroup-aware cargo job cap`.

---

## Task 3: the quickstart container — `up.sh` / `down.sh`

**Files:**
- Create: `scripts/sandbox/up.sh`, `scripts/sandbox/down.sh`
- Modify: `scripts/sandbox/versions.env` (the quickstart tag and digest), `scripts/sandbox/lib.sh` (`wait_for_rpc`, `require_standalone_network`)

**Interfaces:**
- `up.sh`: `docker run -d --name "${SANDBOX_CONTAINER:-blend-sandbox}" -p "${SANDBOX_PORT:-8000}:8000" "$QUICKSTART_IMAGE@$QUICKSTART_DIGEST" --local --enable core,rpc,horizon`; then `wait_for_rpc http://localhost:8000/rpc` — polls `getHealth` until `status == "healthy"` and `getLatestLedger` advances twice (ledgers close), with a 180 s budget; then `require_standalone_network` (`getNetwork` → passphrase equals `SANDBOX_PASSPHRASE`); then `stellar network add --global local --rpc-url http://localhost:8000/rpc --network-passphrase "$SANDBOX_PASSPHRASE"` (idempotent: `stellar network ls` first). Prints the RPC URL. Refuses to start if a container of that name exists (say `down.sh`).
- `down.sh`: `docker rm -f` the container; removes `target/sandbox/sandbox.env` (its keys are dead with the network); keeps the wasm cache.
- `lib.sh` gains the two helpers; `require_standalone_network URL` is what every later script calls first.

- [ ] **Step 1:** Resolve the pin: pick the newest non-nightly `v…-latest` tag of `stellar/quickstart` (the `latest` channel), `docker buildx imagetools inspect docker.io/stellar/quickstart:<tag>` → the manifest-list digest; write both into `versions.env` with a comment on how to bump.
- [ ] **Step 2:** Implement; run `up.sh` in this container (docker-in-docker): the RPC answers healthy within the budget, `stellar network ls` shows `local`, `curl "http://localhost:8000/friendbot?addr=$(stellar keys generate --no-fund probe >/dev/null; stellar keys address probe)"` funds an account; `down.sh` removes it; `up.sh` twice in a row refuses the second time with the hint.
- [ ] **Step 3:** shellcheck; commit `feat(sandbox): a pinned quickstart local network, up and down`.

---

## Task 4: `deploy.sh` and `crash.sh` — the Blend deployment

**Files:**
- Create: `scripts/sandbox/deploy.sh`, `scripts/sandbox/crash.sh`
- Modify: `scripts/sandbox/lib.sh` (`invoke` wrapper, `env_write`)

**Interfaces:**
- `deploy.sh` (idempotency is not required — it runs once per `up.sh`; it refuses to run if `sandbox.env` exists): sequence, every `stellar` call through `invoke KEY CONTRACT FN ARGS…` (`stellar contract invoke --network local --source-account KEY --id CONTRACT -- FN ARGS…`, with `--send=yes`; the wrapper logs the function name, never the args, and dies on failure):
  1. Keys: `stellar keys generate --network local --fund issuer|admin|borrower|filler` (each funded by friendbot with 10 000 XLM); `stellar keys address` for the G-addresses; the filler's secret via `stellar keys secret filler` **only** into the env file.
  2. Tokens: `USDC=$(stellar contract asset deploy --network local --source-account issuer --asset USDC:$ISSUER)`, `BLND` likewise; `XLM=$(stellar contract id asset --network local --asset native)`. Mint (SAC `mint`, source `issuer`): 1 000 000 BLND and 2 000 000 USDC to `admin`; 10 000 USDC to `filler` (`1_000_000_0000000`, `2_000_000_0000000`, `10_000_0000000` as integer strings).
  3. Oracle: `ORACLE=$(stellar contract deploy … --wasm target/sandbox/wasm/mock_sep_40_oracle.wasm)`; `set_data --admin $ADMIN --base '{"Other":"USD"}' --assets '[{"Stellar":"XLM"},{"Stellar":"USDC"}]' --decimals 7 --resolution 300`; `set_price_stable --prices '["1000000","10000000"]'`.
  4. Comet: deploy `comet.wasm`; `init` with controller `admin`, tokens `[BLND, USDC]`, weights `["8000000","2000000"]`, balances `["10000000000","250000000"]`, swap fee `"30000"` — argument names from `stellar contract info interface --wasm comet.wasm`.
  5. Backstop and factory: `FACTORY=$(stellar contract id wasm … --wasm pool-factory_v2.0.0.wasm --salt "$FACTORY_SALT" --source-account admin)` (or whichever CLI path yields the deterministic id before deploying — verify against the address the deploy later prints; the two must agree or the script dies); `BACKSTOP=$(stellar contract deploy … --wasm backstop_v2.0.0.wasm -- --backstop_token $COMET --emitter $ADMIN --blnd_token $BLND --usdc_token $USDC --pool_factory $FACTORY --drop_list '[]')`; `POOL_HASH=$(stellar contract upload … --wasm pool_v2.0.0.wasm)`; `stellar contract deploy … --wasm pool-factory_v2.0.0.wasm --salt "$FACTORY_SALT" -- --pool_init_meta '{"backstop":"…","pool_hash":"<hex>","blnd_id":"…"}'` — the printed address must equal `$FACTORY`.
  6. Pool: `POOL=$(invoke admin $FACTORY deploy --admin $ADMIN --name Sandbox --salt <64 hex> --oracle $ORACLE --backstop_take_rate 1000000 --max_positions 4 --min_collateral 0)` (the CLI prints the returned address).
  7. Reserves: `queue_set_reserve --asset $XLM --metadata '{…}'` then `set_reserve --asset $XLM` (index 0); USDC (index 1). `get_config` must report `status: 6` before and reserves listed by `get_reserve_list`.
  8. Backstop funding: comet `join_pool --pool_amount_out "500000000000" --max_amounts_in '["5000010000000","125010000000"]' --user $ADMIN`; `invoke admin $BACKSTOP deposit --from $ADMIN --pool_address $POOL --amount "500000000000"`; `invoke admin $POOL update_status` → must return `0` (Active); die otherwise, printing `get_config`.
  9. Liquidity and the borrower: `submit` as `admin`: `[{"request_type":0,"address":"USDC","amount":"1000000000000"}]`; as `borrower`: `[{"request_type":2,"address":"XLM","amount":"50000000000"},{"request_type":4,"address":"USDC","amount":"3000000000"}]`. Then `get_positions --address $BORROWER` shows one collateral and one liability.
  10. Write `target/sandbox/sandbox.env` (mode 0600): `SANDBOX_RPC_URL`, `SANDBOX_PASSPHRASE`, `SANDBOX_POOL`, `SANDBOX_XLM`, `SANDBOX_USDC`, `SANDBOX_BLND`, `SANDBOX_ORACLE`, `SANDBOX_BORROWER`, `SANDBOX_FILLER`, `SANDBOX_FILLER_SECRET_KEY`, `SANDBOX_ADMIN`; and a `sandbox.log` of every step's name and printed address (no secrets).
- `crash.sh [PRICE]`: `require_standalone_network`, sources `sandbox.env`, `set_price_stable --prices '["<PRICE, default 750000>","10000000"]'`, then `lastprice --asset '{"Stellar":"XLM"}'` must echo it.

- [ ] **Step 1:** Work against a live sandbox (`up.sh`), step by step, keeping every exact CLI invocation that worked in the script — argument names and JSON shapes are discovered from `stellar contract info interface --wasm <file>` and recorded in comments.
- [ ] **Step 2:** Verify the end state with the CLI: `get_config` status `0`; `get_reserve_list` two assets; the borrower's positions; `stellar contract invoke … $ORACLE -- lastprice --asset '{"Stellar":"$XLM"}'` `1000000`; after `crash.sh`, `750000`.
- [ ] **Step 3:** `down.sh`, `up.sh`, `deploy.sh` again from scratch — the whole sequence must be reproducible in under five minutes; shellcheck; commit `feat(sandbox): deploy Blend v2 on the local network and make a borrower liquidatable`.

---

## Task 5: `tests/liquidation_sandbox.rs`

**Files:**
- Create: `tests/liquidation_sandbox.rs`
- Modify: `Cargo.toml` (`[dev-dependencies]`: nothing new expected — `reqwest`, `tokio`, `sqlx`, `serde_json` are regular dependencies and usable from tests; add `[[test]] name = "liquidation_sandbox"` only if needed for the harness)

**Interfaces:**
- One `#[tokio::test] #[ignore] async fn liquidation_end_to_end()`. Reads `target/sandbox/sandbox.env` (relative to `CARGO_MANIFEST_DIR`); if absent, panics with `run scripts/sandbox/up.sh and scripts/sandbox/deploy.sh first`. Refuses unless `SANDBOX_PASSPHRASE` is the standalone one. Reads `DATABASE_URL` from the environment (as the store tests do) and **creates its own database** for the run (`sandbox_<unix time>`, via a `CREATE DATABASE` on the maintenance connection) so a rerun never sees old rows.
- Spawns `env!("CARGO_BIN_EXE_liquidator")` with `env_clear()` and exactly: `DATABASE_URL=<the run's>`, `NETWORK_PASSPHRASE`, `RPC_URL`, `DRY_RUN=false`, `FILLER_SECRET_KEY`, `POOLS_TOML` (inline: `[[pools]] address, primary_asset = XLM, min_primary_collateral = "1000000000", min_health_factor = 1.5, default_profit_bps = 100, supported_bid = [USDC], supported_lot = ["*"]`), `SEED_FILE` (a temp file naming the borrower under the pool), `SEED_URL=` (empty disables the analytics API), `POLL_INTERVAL_MS=500`, `STARTUP_DELAY_LEDGERS=0`, `FULL_SCAN_LEDGERS=10`, `ORACLE_SCAN_LEDGERS=5`, `XLM_FEE_RESERVE=50`, `PORT=18080`, `HTTP_BIND_ADDR=127.0.0.1`, `LOG_FORMAT=json`, `RUST_LOG=info,blend_liquidator=debug`, `PATH`; stdout/stderr piped to `target/sandbox/bot.log`.
- Then, each with its own timeout (`tokio::time::timeout`) and a 500 ms poll: `/healthz` → 200 (60 s); run `scripts/sandbox/crash.sh` as a subprocess; `SELECT tx_hash FROM creations WHERE account = $borrower AND tx_hash IS NOT NULL` (90 s); `SELECT tx_hash FROM fills WHERE account = $borrower AND tx_hash IS NOT NULL` (120 s); the filler's positions via `blend_liquidator::chain::pool::PoolReader` (`snapshot(&[filler]).positions`): no liabilities and XLM collateral `≤ 1_010_000_000` (100 XLM + 1 %) in b-tokens converted through the reserve — or, simpler and exact, assert `liabilities` empty and the collateral b-token amount below the b-token equivalent of 101 XLM using the snapshot's `accrued_reserves` (120 s); `/metrics` body contains `creations_total{result="succeeded"} 1`, `fills_total{result="succeeded"} 1`, and an `unwind_passes_total` ≥ 1; `SIGTERM` (`nix`-free: `Command::new("kill").args(["-TERM", pid])`); `wait` → exit status 0 (30 s). On any failure, the test prints the last 100 lines of `bot.log` before panicking.
- Every timeout is a named constant; every poll logs what it is waiting for so a hung run is diagnosable from `--nocapture`.

- [ ] **Step 1:** Write the test; run it against the live sandbox: `cargo test --test liquidation_sandbox -- --ignored --nocapture`. Iterate until it passes; keep the passing `bot.log` for the docs task (copy it to the controller's scratchpad as `phase-7-sandbox-bot.log`).
- [ ] **Step 2:** Negative check: run it without `sandbox.env` → the clear panic; run it with a wrong passphrase in a copied env file → refuses before spawning.
- [ ] **Step 3:** `make check` (the ignored test still compiles under `cargo test --lib --bins`? — integration tests are built by `cargo test --test`, so `cargo clippy --all-targets` is what compiles it; it must be clippy-clean); commit `test(sandbox): the end-to-end liquidation against the local network`.

---

## Task 6: `make sandbox` and `sandbox.yml`

**Files:**
- Modify: `Makefile`, `.github/workflows/ci.yml` (the shellcheck glob only), `scripts/check-repo-invariants.sh` (a check that `post-create.sh` and `sandbox.yml` install the CLI version `versions.env` pins — the same shape as the three-way Rust pin)
- Create: `.github/workflows/sandbox.yml`

**Interfaces:**
- Makefile: `sandbox-up`, `sandbox-fetch`, `sandbox-deploy`, `sandbox-test` (`cargo test --test liquidation_sandbox -- --ignored --nocapture`), `sandbox-down`, and `sandbox` = up → fetch → deploy → test → down (down runs even on failure via a trap or `||`; `SANDBOX_KEEP=1` skips down). Each documented in `make help`.
- `sandbox.yml`: `on: schedule: - cron: "17 4 * * *"` and `workflow_dispatch`; `concurrency` group; one job `sandbox` on `ubuntu-latest`, `timeout-minutes: 45`; `services: postgres` as in `ci.yml`; steps: checkout (same pinned SHA as `ci.yml`), the same toolchain action, install the CLI from `versions.env` (download + `sha256sum -c`), `scripts/sandbox/up.sh`, `fetch-artifacts.sh`, `deploy.sh` (with `::add-mask::` of the filler secret read from `sandbox.env` before any later step), `cargo build --bin liquidator`, `make sandbox-test` with `DATABASE_URL`, `always()`: upload `target/sandbox/*.log` as an artifact (never `sandbox.env`), `down.sh`. Not referenced by `ci-summary`.
- `ci.yml`: `shellcheck --severity=error scripts/*.sh scripts/sandbox/*.sh .devcontainer/*.sh`.
- `check-repo-invariants.sh`: `STELLAR_CLI_VERSION` in `versions.env` must appear in `sandbox.yml`'s install step (or the step must source `versions.env` — then the check is that the step sources it); fail with a message naming both files.

- [ ] **Step 1:** Write the targets and the workflow; `actionlint` is not in the toolchain — validate YAML with `python3 -c 'import yaml…'` (PyYAML present?) or `ruby -ryaml`; if neither, `gh workflow view` after push is the check and the report says so.
- [ ] **Step 2:** `make sandbox` locally end to end (this is the demonstration for the PR — keep the full output as `phase-7-sandbox-run.log` in the scratchpad); `make check`; commit `ci(sandbox): a nightly end-to-end run outside the PR gate, and make sandbox`.

---

## Task 7: documentation

**Files:**
- Modify: `CLAUDE.md`, `README.md`, `CHANGELOG.md`

- [ ] **Step 1: CLAUDE.md** — Status: Phase 7 landed the sandbox tier; what remains is Phase 8 (docs set, deployment contract, first release) and the testnet soak. Module map: `tests/liquidation_sandbox.rs`, `scripts/sandbox/`, `scripts/cargo-jobs.sh`, `.github/workflows/sandbox.yml` (outside `CI Summary`, and why). The `stellar` CLI gotcha rewritten: it is installed by `post-create.sh` from `versions.env`, verified; bumping it means the three places (`versions.env`, `sandbox.yml`, `post-create.sh`) — the invariants script guards it. The OOM gotcha gains the cap: `cargo-jobs.sh` writes `[build] jobs` once; the env override still wins. Gotchas: the sandbox keys are the only keys the repo ever generates and they die with the container; `deploy.sh` refuses a non-standalone RPC; the salted-address trick for the backstop/factory cycle; the emitter is not deployed; the borrower's numbers and why the crash price is what it is; the test creates its own database per run.
- [ ] **Step 2: README** — a "Testing" section: the three tiers (unit, the scripted RPC server, the sandbox), `make sandbox` and its prerequisites (docker, the CLI, ~5 min), what the nightly run asserts. **CHANGELOG** `[Unreleased]`: the sandbox tier, the dev-container additions, the workflow.
- [ ] **Step 3:** `make check`; commit `docs(phase-7): the sandbox tier`. The demonstration is Task 6's `make sandbox` run log plus the bot's own `bot.log` from Task 5.

---

## Self-review

**Spec coverage.** §9 Integration: quickstart container (T3), Postgres (existing compose / CI service), Blend v2 pool + backstop + mock SEP-40 oracle at a pinned revision (T1, T4), a pool with two reserves (T4), a funded borrower (T4), the price crash (T4 `crash.sh`, driven by T5), the bot run live (T5), assertions on creation, fill at the expected ledger (T5 asserts the fill happened and landed; the *exact* ledger is the planner's and is asserted by the unit tests — the sandbox asserts the fill's `tx_hash` and the metrics), unwind to the wallet (T5, positions), both recorded in the store (T5); nightly in CI, `#[ignore]` (T5, T6); the dev container gains the `stellar` CLI and the cgroup-aware build cap (T2). §12 phase 7 "the sandbox integration tier and the dev-container additions" — all. CLAUDE.md's standing note about the CLI — T2, T7.

**Placeholder scan.** Every task names files, the exact commands or their discovery method (`stellar contract info interface`), and the checks that gate its commit. T4's argument names are discovered from the wasm specs by design and recorded in the script's comments — that is the deliverable, not a placeholder.

**Type consistency.** `sandbox.env`'s keys (T4) are the ones T5 reads and T6 masks; `versions.env` (T1) is what T2, T3, T6 source; `lib.sh`'s helpers (T1, T3, T4) are named the same throughout; the Makefile targets (T6) match the README (T7).
