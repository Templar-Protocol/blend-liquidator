#!/usr/bin/env bash
# deploy.sh — stands the Blend v2 system up on the sandbox's local
# network and leaves one borrower a single price move away from being
# liquidatable. Writes target/sandbox/sandbox.env (mode 0600), which is
# the only thing the rest of the tier reads, and target/sandbox/sandbox.log,
# a transcript of every step and every address it produced.
#
# Runs exactly once per `up.sh`: it refuses to start when sandbox.env
# already exists, because a second run against the same network would
# deploy a second, unrelated pool and overwrite the env file that names
# the first. `down.sh` (which removes both the container and the env
# file) then `up.sh` is the only way back to a fresh one. Nothing here is
# idempotent and nothing here needs to be.
#
# Every `stellar contract invoke` goes through lib.sh's invoke()/
# invoke_view(), which log the function and the contract's role and never
# an argument; the deploy/upload/keys/tx calls the CLI has no invoke form
# for get the same treatment from the helpers below. The filler's secret
# key is read exactly once, at the very end, straight into sandbox.env —
# it is never echoed, never logged and never passed as an argument.
#
# Every CLI call here, wrapped or not, is handed lib.sh's
# "${sandbox_network_args[@]}" — the RPC URL require_standalone_network
# has just verified and the passphrase it verified it by. Nothing names a
# CLI network: the environment can redirect a `--network`, and this script
# generates keys, funds them and signs with them.
#
# ## Argument names and shapes
#
# Every name below was read from the wasm's own spec with
#   stellar contract info interface --wasm target/sandbox/wasm/<file>
# and is recorded at its call site. The non-obvious encodings:
#   - i128 is a decimal string ("1000000000000"); u32 is a bare number.
#   - A struct argument is a JSON object keyed by the Rust field names.
#   - An enum with a payload is {"Variant": payload}: the SEP-40 oracle's
#     Asset is {"Stellar":"C…"} or {"Other":"USD"}.
#   - BytesN<32> is 64 hex characters, unprefixed.
#   - soroban_sdk::String is passed bare (--name Sandbox).
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/sandbox/lib.sh
source "${script_dir}/lib.sh"
# shellcheck source=scripts/sandbox/versions.env
source "${VERSIONS_ENV:-${script_dir}/versions.env}"

: "${SANDBOX_PORT:=8000}"

sandbox_root="$(sandbox_dir)"
wasm_dir="${sandbox_root}/wasm"
env_file="${sandbox_root}/sandbox.env"
rpc_url="http://localhost:${SANDBOX_PORT}/rpc"

# The env-file refusal comes before require_standalone_network only
# because it touches no network at all — it is a `test -f` — and it is the
# failure an operator actually hits, so it deserves the better message.
# require_standalone_network is still the first thing that speaks to a
# node, which is what the invariant it enforces is about.
if [ -f "${env_file}" ]; then
	die "refusing to deploy: ${env_file} already exists, so this network has been deployed to already — run scripts/sandbox/down.sh and scripts/sandbox/up.sh for a fresh one"
fi

require_standalone_network "${rpc_url}"

# Unconditionally, every run: fetch-artifacts.sh is idempotent and
# re-verifies every hash, downloading only what is missing or no longer
# matches. Testing for the files here instead — which this did — accepts a
# present file whose bytes have since changed, which is the one thing the
# pinned hashes exist to catch, and costs nothing when they are all
# already there.
log "verifying the pinned contract artefacts"
"${script_dir}/fetch-artifacts.sh" >/dev/null || die "fetching artefacts: fetch-artifacts.sh failed"

mkdir -p "${sandbox_root}"
: >"${sandbox_root}/sandbox.log" || die "preparing the log: could not create ${sandbox_root}/sandbox.log"
# From here on every log() and die() line is also appended to sandbox.log.
export SANDBOX_LOG="${sandbox_root}/sandbox.log"

########################################################################
# Constants
#
# Amounts are integers in each asset's own decimals; every asset here has
# 7, so 1 unit is 10_000_000. The oracle also reports 7 decimals, so a
# price of 1000000 is $0.10.
########################################################################

# Salts are fixed literals, not random: the factory's address has to be
# predicted before the backstop that references it can be deployed, and a
# fixed salt makes the prediction reproducible. It is still unique per
# run, because a contract id is derived from the deploying account too and
# every run generates fresh keys.
FACTORY_SALT=0000000000000000000000000000000000000000000000000000000000000001
POOL_SALT=0000000000000000000000000000000000000000000000000000000000000002

MINT_BLND_ADMIN=10000000000000      # 1_000_000_0000000 — 1,000,000 BLND
MINT_USDC_ADMIN=20000000000000      # 2_000_000_0000000 — 2,000,000 USDC
MINT_USDC_FILLER=100000000000       #    10_000_0000000 —     10,000 USDC

PRICE_XLM=1000000                   # $0.10 at 7 decimals
PRICE_USDC=10000000                 # $1.00 at 7 decimals

COMET_BLND_BALANCE=10000000000      # 1_000_0000000 — 1,000 BLND
COMET_USDC_BALANCE=250000000        #    25_0000000 —    25 USDC
COMET_BLND_WEIGHT=8000000           # 0.8 at 7 decimals
COMET_USDC_WEIGHT=2000000           # 0.2
COMET_SWAP_FEE=30000                # 0.3%

# 50,000 comet shares, and the most BLND/USDC the join may cost. The
# contracts' own fixture uses exactly these: the comet pool holds 1,000
# BLND / 25 USDC for 100 shares, so 50,000 more shares cost ~500,000 BLND
# and ~12,500 USDC, and the maxima are those plus one unit of slack. This is
# what carries the backstop over the pool's activation threshold,
# (blnd/1e7)^4 × (usdc/1e7) ≥ 1e25.
BACKSTOP_SHARES=500000000000        # 50_000_0000000
JOIN_MAX_BLND=5000010000000         # 500_001_0000000
JOIN_MAX_USDC=125010000000          #  12_501_0000000

ADMIN_USDC_SUPPLY=1000000000000     # 100_000_0000000 — the pool's lendable USDC
BORROWER_XLM_COLLATERAL=50000000000 #   5_000_0000000 — 5,000 XLM
BORROWER_USDC_DEBT=3000000000       #     300_0000000 —   300 USDC

# An i64::MAX allowance, live until ledger 500,000 — an absolute ledger
# number, not a span, and one a sandbox that starts from ledger 1 will not
# reach. Comet's join_pool moves the caller's tokens with transfer_from,
# which checks a real allowance even where the transaction's own source
# account satisfies the auth, so the admin has to approve comet explicitly.
ALLOWANCE=9223372036854775807
ALLOWANCE_UNTIL_LEDGER=500000

########################################################################
# Helpers the CLI has no `contract invoke` form for
########################################################################

# generate_key NAME — creates (or replaces) the CLI identity NAME and
# funds it from friendbot with 10,000 XLM. --overwrite so a second
# sandbox reuses the name rather than failing on it: the previous
# network's key is dead the moment that network is gone.
generate_key() {
	local name=$1
	log "generating and funding ${name}"
	stellar keys generate "${name}" "${sandbox_network_args[@]}" --fund --overwrite \
		|| die "step 1 keys: stellar keys generate ${name} failed"
}

# require_funded NAME ADDRESS — dies unless the network actually holds
# ADDRESS as an account.
#
# `stellar keys generate --fund` exits 0 whether or not friendbot answered:
# funding is best-effort to it, so a friendbot that failed leaves a
# perfectly valid key with no account behind it and the first symptom is a
# deploy or an invoke failing two steps later with "account not found",
# under the wrong step's name. This is that failure, named where it
# happened.
#
# Horizon's /accounts/<G> is the check because friendbot is the same
# service on the same port: if Horizon cannot answer, nothing funded
# anything. Retried for a few seconds only — friendbot returns once its
# transaction is in a closed ledger, so a miss here is Horizon's ingestion
# lagging by a ledger, never a slow account.
require_funded() {
	local name=$1 address=$2 deadline body
	deadline=$(($(date +%s) + 30))
	while :; do
		body=$(curl -fsS --max-time 5 "http://localhost:${SANDBOX_PORT}/accounts/${address}" 2>/dev/null) || body=""
		if [ "$(printf '%s' "${body}" | jq -r '.id // empty' 2>/dev/null)" = "${address}" ]; then
			log "${name} is funded"
			return 0
		fi
		[ "$(date +%s)" -lt "${deadline}" ] || break
		sleep 1
	done
	die "step 1 keys: friendbot did not fund ${name} (${address}) — http://localhost:${SANDBOX_PORT}/accounts/${address} holds no such account after 30s"
}

# deploy_wasm ROLE KEY WASM [-- constructor args…] — deploys WASM as KEY
# and prints the new contract id.
#
# Registering the role is the **caller's** job, on the line after the
# capture: every use here is `X=$(deploy_wasm …)`, and a command
# substitution is a subshell, so a SANDBOX_ROLES entry written in here
# would die with it and every later invoke() would log "unregistered
# contract" instead of naming what it was talking to.
deploy_wasm() {
	local role=$1 key=$2 wasm=$3 id
	shift 3
	log "deploying ${role} from $(basename "${wasm}") as ${key}"
	id=$(stellar contract deploy "${sandbox_network_args[@]}" --source-account "${key}" \
		--wasm "${wasm}" "$@") || die "deploying ${role}: stellar contract deploy failed"
	[ -n "${id}" ] || die "deploying ${role}: stellar contract deploy printed no contract id"
	printf '%s\n' "${id}"
}

# deploy_sac ROLE KEY ASSET — deploys the Stellar Asset Contract for
# ASSET ("native", "USDC:G…") and prints its id; the caller registers the
# role, for the reason deploy_wasm gives.
#
# The native SAC is *not* pre-deployed on a fresh standalone network: it
# has to be created here like any other, or every XLM read answers
# "Contract not found".
deploy_sac() {
	local role=$1 key=$2 asset=$3 id
	log "deploying ${role} as the Stellar Asset Contract for ${asset}"
	id=$(stellar contract asset deploy "${sandbox_network_args[@]}" --source-account "${key}" \
		--asset "${asset}") || die "deploying ${role}: stellar contract asset deploy ${asset} failed"
	[ -n "${id}" ] || die "deploying ${role}: stellar contract asset deploy ${asset} printed no contract id"
	printf '%s\n' "${id}"
}

# trust KEY ASSET — opens KEY's trustline to ASSET. A classic asset's SAC
# mints into a trustline, so an account with none is refused with
# "trustline entry is missing"; contracts need no trustline, only G
# accounts do.
trust() {
	local key=$1 asset=$2
	log "opening ${key}'s trustline to ${asset%%:*}"
	stellar tx new change-trust "${sandbox_network_args[@]}" --source-account "${key}" --line "${asset}" >/dev/null \
		|| die "opening trustlines: change-trust ${asset%%:*} for ${key} failed"
}

# reserve_metadata INDEX C_FACTOR L_FACTOR UTIL — the pool's ReserveConfig
# as JSON. Everything but the four arguments is the contracts' own
# `default_reserve_metadata`; factors and utilisation are 7-decimal, and
# supply_cap is an i128 so it is a string. The printf arguments are given
# out of order because ReserveConfig's fields are alphabetical (c_factor
# before index) while this signature reads index first.
reserve_metadata() {
	printf '{"c_factor":%s,"decimals":7,"enabled":true,"index":%s,"l_factor":%s,"max_util":9500000,"r_base":100000,"r_one":500000,"r_three":15000000,"r_two":5000000,"reactivity":20,"supply_cap":"1000000000000000000","util":%s}' \
		"$2" "$1" "$3" "$4"
}

# json_field JSON PATH STEP — the value jq's PATH selects in JSON, or a
# die naming STEP. Every contract answer this script tests goes through
# it, so "the CLI printed something jq cannot read" is never mistaken for
# "the contract answered something unexpected".
json_field() {
	local json=$1 path=$2 step=$3 value
	value=$(printf '%s' "${json}" | jq -r "${path}") \
		|| die "${step}: could not parse the contract's answer"
	printf '%s\n' "${value}"
}

########################################################################
# 1. Keys
#
# issuer holds nothing: a Stellar asset's issuer cannot hold its own
# asset, so it only mints. admin owns every contract and every pool-side
# balance; borrower is the position the tier liquidates; filler is the
# bot's own key.
########################################################################

log "=== step 1: keys ==="
for key in "${SANDBOX_KEY_ISSUER}" "${SANDBOX_KEY_ADMIN}" "${SANDBOX_KEY_BORROWER}" "${SANDBOX_KEY_FILLER}"; do
	generate_key "${key}"
done

ISSUER=$(stellar keys address "${SANDBOX_KEY_ISSUER}") || die "step 1 keys: reading ${SANDBOX_KEY_ISSUER}'s address failed"
ADMIN=$(stellar keys address "${SANDBOX_KEY_ADMIN}") || die "step 1 keys: reading ${SANDBOX_KEY_ADMIN}'s address failed"
BORROWER=$(stellar keys address "${SANDBOX_KEY_BORROWER}") || die "step 1 keys: reading ${SANDBOX_KEY_BORROWER}'s address failed"
FILLER=$(stellar keys address "${SANDBOX_KEY_FILLER}") || die "step 1 keys: reading ${SANDBOX_KEY_FILLER}'s address failed"
log "issuer ${ISSUER}"
log "admin ${ADMIN}"
log "borrower ${BORROWER}"
log "filler ${FILLER}"

require_funded "${SANDBOX_KEY_ISSUER}" "${ISSUER}"
require_funded "${SANDBOX_KEY_ADMIN}" "${ADMIN}"
require_funded "${SANDBOX_KEY_BORROWER}" "${BORROWER}"
require_funded "${SANDBOX_KEY_FILLER}" "${FILLER}"

########################################################################
# 2. Tokens
#
# SAC: mint --to Address --amount i128, signed by the issuer.
########################################################################

log "=== step 2: tokens ==="
USDC=$(deploy_sac usdc "${SANDBOX_KEY_ISSUER}" "USDC:${ISSUER}")
sandbox_register_role "${USDC}" usdc
BLND=$(deploy_sac blnd "${SANDBOX_KEY_ISSUER}" "BLND:${ISSUER}")
sandbox_register_role "${BLND}" blnd
XLM=$(deploy_sac xlm "${SANDBOX_KEY_ADMIN}" native)
sandbox_register_role "${XLM}" xlm

trust "${SANDBOX_KEY_ADMIN}" "USDC:${ISSUER}"
trust "${SANDBOX_KEY_ADMIN}" "BLND:${ISSUER}"
trust "${SANDBOX_KEY_FILLER}" "USDC:${ISSUER}"
# The borrower is paid its borrowed USDC by the pool, so it needs the
# trustline even though it is never minted to.
trust "${SANDBOX_KEY_BORROWER}" "USDC:${ISSUER}"

invoke "${SANDBOX_KEY_ISSUER}" "${BLND}" mint --to "${ADMIN}" --amount "${MINT_BLND_ADMIN}" >/dev/null
invoke "${SANDBOX_KEY_ISSUER}" "${USDC}" mint --to "${ADMIN}" --amount "${MINT_USDC_ADMIN}" >/dev/null
invoke "${SANDBOX_KEY_ISSUER}" "${USDC}" mint --to "${FILLER}" --amount "${MINT_USDC_FILLER}" >/dev/null

########################################################################
# 3. Oracle
#
# mock_sep_40_oracle: no constructor.
#   set_data(admin: Address, base: Asset, assets: Vec<Asset>,
#            decimals: u32, resolution: u32)
#   set_price_stable(prices: Vec<i128>)   — positional, in `assets` order
#   lastprice(asset: Asset) -> Option<PriceData { price, timestamp }>
########################################################################

log "=== step 3: oracle ==="
ORACLE=$(deploy_wasm oracle "${SANDBOX_KEY_ADMIN}" "${wasm_dir}/mock_sep_40_oracle.wasm")
sandbox_register_role "${ORACLE}" oracle
invoke "${SANDBOX_KEY_ADMIN}" "${ORACLE}" set_data \
	--admin "${ADMIN}" \
	--base '{"Other":"USD"}' \
	--assets "[{\"Stellar\":\"${XLM}\"},{\"Stellar\":\"${USDC}\"}]" \
	--decimals 7 \
	--resolution 300 >/dev/null
invoke "${SANDBOX_KEY_ADMIN}" "${ORACLE}" set_price_stable \
	--prices "[\"${PRICE_XLM}\",\"${PRICE_USDC}\"]" >/dev/null

########################################################################
# 4. Comet — the BLND:USDC pool whose shares are the backstop token
#
# comet.wasm: no constructor.
#   init(controller: Address, tokens: Vec<Address>, weights: Vec<i128>,
#        balances: Vec<i128>, swap_fee: i128)
#   join_pool(pool_amount_out: i128, max_amounts_in: Vec<i128>,
#             user: Address)
########################################################################

log "=== step 4: comet ==="
COMET=$(deploy_wasm comet "${SANDBOX_KEY_ADMIN}" "${wasm_dir}/comet.wasm")
sandbox_register_role "${COMET}" comet
for token in "${BLND}" "${USDC}"; do
	invoke "${SANDBOX_KEY_ADMIN}" "${token}" approve \
		--from "${ADMIN}" --spender "${COMET}" \
		--amount "${ALLOWANCE}" --live_until_ledger "${ALLOWANCE_UNTIL_LEDGER}" >/dev/null
done
invoke "${SANDBOX_KEY_ADMIN}" "${COMET}" init \
	--controller "${ADMIN}" \
	--tokens "[\"${BLND}\",\"${USDC}\"]" \
	--weights "[\"${COMET_BLND_WEIGHT}\",\"${COMET_USDC_WEIGHT}\"]" \
	--balances "[\"${COMET_BLND_BALANCE}\",\"${COMET_USDC_BALANCE}\"]" \
	--swap_fee "${COMET_SWAP_FEE}" >/dev/null

########################################################################
# 5. Backstop and pool factory
#
# These two reference each other: the backstop's constructor names the
# factory (it asks is_pool before accepting a deposit), and the factory's
# names the backstop. The factory's address is therefore predicted from a
# salt first — `stellar contract id wasm --salt … --source-account …`
# derives it from the account, the salt and the network passphrase, not
# from the wasm — the backstop is deployed against the prediction, and the
# factory is then deployed with that same salt. The two must agree, and
# the check below is the whole reason the prediction is safe: a mismatch
# means the backstop trusts a factory that does not exist.
#
#   backstop __constructor(backstop_token, emitter, blnd_token,
#                          usdc_token, pool_factory,
#                          drop_list: Vec<(Address, i128)>)
#   pool-factory __constructor(pool_init_meta: PoolInitMeta
#                              { backstop, blnd_id, pool_hash: BytesN<32> })
#
# The emitter is not deployed: it only matters to BLND emissions, which
# this sandbox never starts, and the backstop never calls into the address
# it is given unless drop()/distribute() is invoked. The admin's own
# address stands in.
########################################################################

log "=== step 5: backstop and factory ==="
FACTORY=$(stellar contract id wasm "${sandbox_network_args[@]}" \
	--salt "${FACTORY_SALT}" --source-account "${SANDBOX_KEY_ADMIN}") \
	|| die "step 5 factory: predicting the factory's address failed"
[ -n "${FACTORY}" ] || die "step 5 factory: stellar contract id wasm printed no address"
sandbox_register_role "${FACTORY}" "pool factory (predicted)"

BACKSTOP=$(deploy_wasm backstop "${SANDBOX_KEY_ADMIN}" "${wasm_dir}/backstop_v2.0.0.wasm" -- \
	--backstop_token "${COMET}" \
	--emitter "${ADMIN}" \
	--blnd_token "${BLND}" \
	--usdc_token "${USDC}" \
	--pool_factory "${FACTORY}" \
	--drop_list '[]')
sandbox_register_role "${BACKSTOP}" backstop

log "uploading the pool wasm"
POOL_HASH=$(stellar contract upload "${sandbox_network_args[@]}" \
	--source-account "${SANDBOX_KEY_ADMIN}" --wasm "${wasm_dir}/pool_v2.0.0.wasm") \
	|| die "step 5 pool wasm: stellar contract upload failed"
[ -n "${POOL_HASH}" ] || die "step 5 pool wasm: stellar contract upload printed no hash"
log "pool wasm hash ${POOL_HASH}"

log "deploying the pool factory at the predicted address"
FACTORY_DEPLOYED=$(stellar contract deploy "${sandbox_network_args[@]}" \
	--source-account "${SANDBOX_KEY_ADMIN}" --wasm "${wasm_dir}/pool-factory_v2.0.0.wasm" \
	--salt "${FACTORY_SALT}" -- \
	--pool_init_meta "{\"backstop\":\"${BACKSTOP}\",\"blnd_id\":\"${BLND}\",\"pool_hash\":\"${POOL_HASH}\"}") \
	|| die "step 5 factory: deploying the pool factory failed"
if [ "${FACTORY_DEPLOYED}" != "${FACTORY}" ]; then
	die "step 5 factory: the deployed factory is ${FACTORY_DEPLOYED} but the backstop was built against ${FACTORY} — the backstop would refuse every deposit, so this deployment is unusable"
fi
sandbox_register_role "${FACTORY}" "pool factory"

########################################################################
# 6. Pool
#
#   deploy(admin: Address, name: String, salt: BytesN<32>, oracle: Address,
#          backstop_take_rate: u32, max_positions: u32,
#          min_collateral: i128) -> Address
#
# The CLI prints a returned Address as a quoted JSON string.
########################################################################

log "=== step 6: pool ==="
POOL=$(invoke "${SANDBOX_KEY_ADMIN}" "${FACTORY}" deploy \
	--admin "${ADMIN}" \
	--name Sandbox \
	--salt "${POOL_SALT}" \
	--oracle "${ORACLE}" \
	--backstop_take_rate 1000000 \
	--max_positions 4 \
	--min_collateral 0)
POOL=$(json_field "${POOL}" '.' "step 6 pool")
[ -n "${POOL}" ] || die "step 6 pool: the factory returned no pool address"
sandbox_register_role "${POOL}" "pool"

########################################################################
# 7. Reserves
#
#   queue_set_reserve(asset: Address, metadata: ReserveConfig)
#   set_reserve(asset: Address) -> u32   (the reserve's index)
#
# Both must happen while the pool is in Setup (status 6): outside Setup,
# queue_set_reserve imposes a timelock and set_reserve would be refused
# until it expired.
########################################################################

log "=== step 7: reserves ==="
config=$(invoke_view "${SANDBOX_KEY_ADMIN}" "${POOL}" get_config)
status=$(json_field "${config}" '.status' "step 7 reserves")
[ "${status}" = "6" ] || die "step 7 reserves: the pool reports status ${status}, not 6 (Setup) — reserves can only be set without a timelock during Setup: ${config}"

invoke "${SANDBOX_KEY_ADMIN}" "${POOL}" queue_set_reserve \
	--asset "${XLM}" --metadata "$(reserve_metadata 0 7500000 7500000 5000000)" >/dev/null
XLM_INDEX=$(invoke "${SANDBOX_KEY_ADMIN}" "${POOL}" set_reserve --asset "${XLM}")
[ "${XLM_INDEX}" = "0" ] || die "step 7 reserves: XLM was given reserve index ${XLM_INDEX}, expected 0"

invoke "${SANDBOX_KEY_ADMIN}" "${POOL}" queue_set_reserve \
	--asset "${USDC}" --metadata "$(reserve_metadata 1 9000000 9500000 8500000)" >/dev/null
USDC_INDEX=$(invoke "${SANDBOX_KEY_ADMIN}" "${POOL}" set_reserve --asset "${USDC}")
[ "${USDC_INDEX}" = "1" ] || die "step 7 reserves: USDC was given reserve index ${USDC_INDEX}, expected 1"

reserve_list=$(invoke_view "${SANDBOX_KEY_ADMIN}" "${POOL}" get_reserve_list)
reserve_count=$(json_field "${reserve_list}" 'length' "step 7 reserves")
[ "${reserve_count}" = "2" ] || die "step 7 reserves: the pool lists ${reserve_count} reserves, expected 2: ${reserve_list}"
log "reserves: ${reserve_list}"

########################################################################
# 8. Backstop funding and activation
#
#   backstop deposit(from: Address, pool_address: Address, amount: i128)
#   pool set_status(pool_status: u32)
#
# **Deviation from the brief.** The brief's step 8 ends with
# `update_status` returning 0. It cannot: the pool's own
# execute_update_pool_status panics with StatusNotAllowed (1204) whenever
# the current status is 6, and verified so here against this exact wasm.
# The contracts' own fixture leaves Setup with set_status(3) followed by
# update_status(), which lands on 1 (Active); execute_set_pool_status(0)
# reaches 0 (Admin Active) in one call while enforcing the identical
# condition — it panics with the same 1204 unless the backstop threshold
# is met and queued withdrawals are under 50%. So set_status 0 it is, and
# get_config below is what proves the threshold was met.
#
# 0 rather than the fixture's 1 costs nothing: the contract's
# require_action_allowed treats 0 and 1 identically for everything this
# tier exercises, and the bot's own validate accepts both (see
# src/service.rs).
########################################################################

log "=== step 8: backstop funding ==="
invoke "${SANDBOX_KEY_ADMIN}" "${COMET}" join_pool \
	--pool_amount_out "${BACKSTOP_SHARES}" \
	--max_amounts_in "[\"${JOIN_MAX_BLND}\",\"${JOIN_MAX_USDC}\"]" \
	--user "${ADMIN}" >/dev/null
invoke "${SANDBOX_KEY_ADMIN}" "${BACKSTOP}" deposit \
	--from "${ADMIN}" --pool_address "${POOL}" --amount "${BACKSTOP_SHARES}" >/dev/null
invoke "${SANDBOX_KEY_ADMIN}" "${POOL}" set_status --pool_status 0 >/dev/null

config=$(invoke_view "${SANDBOX_KEY_ADMIN}" "${POOL}" get_config)
status=$(json_field "${config}" '.status' "step 8 activation")
# 0 or 1, not 0 alone: this calls set_status(0), so 0 is what it expects,
# but 0 (Admin Active) and 1 (Active) are the same thing to everything the
# bot does — the contract's require_action_allowed refuses borrow and
# cancel only above 1 — and the assertion here is that the pool left Setup
# at all, which either answers. A `case`, not `-le`, because a non-numeric
# answer must fail as a bad status rather than as an arithmetic error.
case "${status}" in
0 | 1) ;;
*) die "step 8 activation: the pool reports status ${status}, not 0 (Admin Active) or 1 (Active) — the backstop threshold was not met: ${config}" ;;
esac
log "pool is active (status ${status}): ${config}"

########################################################################
# 9. Liquidity and the borrower
#
#   submit(from: Address, spender: Address, to: Address,
#          requests: Vec<Request { request_type: u32, address: Address,
#                                  amount: i128 }>) -> Positions
#   request_type: 0 Supply, 2 SupplyCollateral, 4 Borrow
#
# At $0.10/XLM the borrower's 5,000 XLM of collateral is worth $500, which
# a 0.75 collateral factor values at $375, against 300 USDC of debt a 0.95
# liability factor values at ~$315.8 — a health factor of ~1.19. crash.sh's
# $0.075 takes the collateral to $281.25 and the health factor to ~0.89.
########################################################################

log "=== step 9: liquidity and the borrower ==="
invoke "${SANDBOX_KEY_ADMIN}" "${POOL}" submit \
	--from "${ADMIN}" --spender "${ADMIN}" --to "${ADMIN}" \
	--requests "[{\"request_type\":0,\"address\":\"${USDC}\",\"amount\":\"${ADMIN_USDC_SUPPLY}\"}]" >/dev/null

invoke "${SANDBOX_KEY_BORROWER}" "${POOL}" submit \
	--from "${BORROWER}" --spender "${BORROWER}" --to "${BORROWER}" \
	--requests "[{\"request_type\":2,\"address\":\"${XLM}\",\"amount\":\"${BORROWER_XLM_COLLATERAL}\"},{\"request_type\":4,\"address\":\"${USDC}\",\"amount\":\"${BORROWER_USDC_DEBT}\"}]" >/dev/null

positions=$(invoke_view "${SANDBOX_KEY_ADMIN}" "${POOL}" get_positions --address "${BORROWER}")
collateral_count=$(json_field "${positions}" '.collateral | length' "step 9 borrower")
liability_count=$(json_field "${positions}" '.liabilities | length' "step 9 borrower")
if [ "${collateral_count}" != "1" ] || [ "${liability_count}" != "1" ]; then
	die "step 9 borrower: expected one collateral and one liability, got ${collateral_count} and ${liability_count}: ${positions}"
fi
log "borrower positions: ${positions}"

########################################################################
# 10. sandbox.env
#
# The filler's secret is read here and nowhere else, straight into the
# file env_write has already restricted to mode 0600. It is never
# assigned to a shell variable that outlives this heredoc, never passed as
# an argument and never logged — which is why this is the last step and
# why the log line below names the file rather than its contents.
#
# Where the heredoc itself lives is the rest of that claim: bash
# materialises one either as a pipe (5.1 and later, for a document this
# small) or as a temp file created 0600 and unlinked immediately, so the
# only on-disk copy is short-lived, owner-only and already nameless. The
# durable copy is sandbox.env, which env_write restricted before the
# first byte reached it.
#
# SANDBOX_RPC_URL is written from the variable require_standalone_network
# exported, not from a second reconstruction of the URL: what the node was
# checked on is what the bot is pointed at.
########################################################################

log "=== step 10: sandbox.env ==="
env_write "${env_file}" <<EOF
SANDBOX_RPC_URL="${SANDBOX_RPC_URL}"
SANDBOX_PASSPHRASE="${SANDBOX_PASSPHRASE}"
SANDBOX_POOL="${POOL}"
SANDBOX_XLM="${XLM}"
SANDBOX_USDC="${USDC}"
SANDBOX_BLND="${BLND}"
SANDBOX_ORACLE="${ORACLE}"
SANDBOX_BORROWER="${BORROWER}"
SANDBOX_FILLER="${FILLER}"
SANDBOX_FILLER_SECRET_KEY="$(stellar keys secret "${SANDBOX_KEY_FILLER}")"
SANDBOX_ADMIN="${ADMIN}"
EOF
grep -q '^SANDBOX_FILLER_SECRET_KEY="S' "${env_file}" \
	|| die "step 10 sandbox.env: the filler's secret key did not reach ${env_file}"

log "wrote ${env_file} (mode 0600)"
log "deploy complete"
printf '%s\n' "${env_file}"
