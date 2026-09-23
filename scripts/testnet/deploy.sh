#!/usr/bin/env bash
# deploy.sh — stands Blend v2 up on Stellar testnet for the soak's armed
# stage: our own tokens, mock oracle, backstop, factory and pool, with one
# borrower a single price move from being liquidatable. Writes
# target/testnet/testnet.env (mode 0600) and target/testnet/deploy.log, a
# transcript of every step and every address it produced.
#
# This is scripts/sandbox/deploy.sh's own ten steps, run against testnet
# instead of a throwaway local network — read that file's comments for the
# reasoning behind each one; this header only says where testnet differs:
#
#   - the native asset's Stellar Asset Contract already exists on testnet
#     (every network but a brand new standalone one has it), so step 2
#     derives its id with `stellar contract id asset --asset native`
#     instead of deploying one;
#   - funding is friendbot's: require_funded_testnet (scripts/testnet/
#     lib.sh) polls testnet Horizon and re-requests testnet's friendbot,
#     never a local quickstart instance;
#   - the pool wasm is uploaded **before anything else** that touches the
#     chain, as its own preflight step, and this script stops there —
#     naming the reason — if testnet's protocol 28 refuses bytes pinned
#     against soroban-sdk 22. Every step after it reuses that upload's
#     hash rather than uploading a second time;
#   - a Soroban token approval's live-until is a ledger *number*, not a
#     span, and testnet's is already in the millions — nowhere near the
#     sandbox's fixed 500,000. Step 4 below reads the chain's own current
#     ledger first and adds the same margin to *that*;
#   - there is no down.sh here and this script's own env-file guard is the
#     only cleanup this tier has. Testnet is not this repository's network
#     to reset: Stellar wipes every contract and account on it itself, 2 to
#     4 times a year, and deploy.sh is how the soak is rebuilt after one —
#     see docs/testnet-soak.md.
#
# Refuses to start when target/testnet/testnet.env already exists, for the
# same reason the sandbox refuses: a second run would deploy a second,
# unrelated pool and overwrite the env file naming the first. There is no
# down.sh to clear it — `rm target/testnet/testnet.env` and re-run this
# script for a fresh deploy; the old contracts are simply abandoned, which
# costs nothing on a network this repo does not pay upkeep for.
#
# Every `stellar contract invoke` goes through lib.sh's invoke()/
# invoke_view(), which log the function and the contract's role and never
# an argument; the deploy/upload/keys calls the CLI has no invoke form for
# get the same treatment from the helpers below. The filler's secret key is
# read exactly once, at the very end, straight into testnet.env — never
# echoed, never logged, never an argument.
#
# Every CLI call here, wrapped or not, is handed lib.sh's
# "${sandbox_network_args[@]}" — the flags require_testnet_network built
# from the URL it just verified. Nothing names a CLI network: the
# environment can redirect a `--network`, and this script generates keys,
# funds them and signs with them.
#
# Argument names and shapes are exactly scripts/sandbox/deploy.sh's own —
# same contracts, same wasm — see that file's header for the encoding notes
# (i128 as a decimal string, an enum with a payload as {"Variant": payload},
# BytesN<32> as unprefixed hex, and so on).
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/testnet/lib.sh
source "${script_dir}/lib.sh"

testnet_root="$(testnet_dir)"
wasm_dir="$(sandbox_dir)/wasm"
env_file="${testnet_root}/testnet.env"

# The env-file refusal comes before require_testnet_network only because it
# touches no network at all — it is a `test -f` — and it is the failure an
# operator actually hits, so it deserves the better message.
# require_testnet_network is still the first thing that speaks to a node.
if [ -f "${env_file}" ]; then
	die "refusing to deploy: ${env_file} already exists, so testnet has been deployed to already — remove it by hand for a fresh deploy (there is no down.sh; see this script's own header)"
fi

require_testnet_network

# Unconditionally, every run: fetch-artifacts.sh is idempotent and
# re-verifies every hash, downloading only what is missing or no longer
# matches. The wasm is content-hash keyed, not network keyed, so the
# sandbox's own target/sandbox/wasm/ is exactly what this reuses.
log "verifying the pinned contract artefacts"
"${script_dir}/../sandbox/fetch-artifacts.sh" >/dev/null || die "fetching artefacts: fetch-artifacts.sh failed"

mkdir -p "${testnet_root}"
: >"${testnet_root}/deploy.log" || die "preparing the log: could not create ${testnet_root}/deploy.log"
# From here on every log() and die() line is also appended to deploy.log.
export SANDBOX_LOG="${testnet_root}/deploy.log"

########################################################################
# Constants
#
# Amounts are integers in each asset's own decimals; every asset here has
# 7, so 1 unit is 10_000_000. The oracle also reports 7 decimals, so a
# price of 1000000 is $0.10. Identical to scripts/sandbox/deploy.sh's own —
# "keep the sandbox's scenario numbers" — except ALLOWANCE_UNTIL_LEDGER,
# which step 4 below computes from testnet's own current ledger rather than
# using the sandbox's fixed literal (see this file's header).
########################################################################

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

# 50,000 comet shares, and the most BLND/USDC the join may cost — what
# carries the backstop over the pool's activation threshold. See
# scripts/sandbox/deploy.sh's own comment for the arithmetic.
BACKSTOP_SHARES=500000000000        # 50_000_0000000
JOIN_MAX_BLND=5000010000000         # 500_001_0000000
JOIN_MAX_USDC=125010000000          #  12_501_0000000

ADMIN_USDC_SUPPLY=1000000000000     # 100_000_0000000 — the pool's lendable USDC
BORROWER_XLM_COLLATERAL=50000000000 #   5_000_0000000 — 5,000 XLM
BORROWER_USDC_DEBT=3000000000       #     300_0000000 —   300 USDC

# i64::MAX, the allowance amount — see scripts/sandbox/deploy.sh's own
# comment on why Comet's join_pool needs an explicit approval at all. The
# *ledger* it is valid until is computed in step 4, below, from testnet's
# own current ledger, not a fixed literal.
ALLOWANCE=9223372036854775807

########################################################################
# Helpers the CLI has no `contract invoke` form for — same shapes as
# scripts/sandbox/deploy.sh's own, calling require_funded_testnet in place
# of its require_funded and sandbox_require_network before every expansion
# of sandbox_network_args, for the same reason that file's header gives.
########################################################################

# generate_key NAME — creates (or replaces) the CLI identity NAME and
# funds it from testnet's friendbot. --overwrite so a re-deploy after
# removing testnet.env reuses the name rather than failing on it: the
# previous deploy's key is abandoned the moment testnet.env naming it is
# gone. --fund is best-effort here exactly as it is in the sandbox;
# require_funded_testnet below is what actually guarantees funding.
generate_key() {
	local name=$1
	sandbox_require_network
	log "generating and funding ${name}"
	stellar keys generate "${name}" "${sandbox_network_args[@]}" --fund --overwrite \
		|| die "step 1 keys: stellar keys generate ${name} failed"
}

# deploy_wasm ROLE KEY WASM [-- constructor args…] — deploys WASM as KEY and
# prints the new contract id. Registering the role is the **caller's** job,
# on the line after the capture: every use here is `X=$(deploy_wasm …)`, a
# subshell, so a role written inside it would die with the subshell.
deploy_wasm() {
	local role=$1 key=$2 wasm=$3 id
	shift 3
	sandbox_require_network
	log "deploying ${role} from $(basename "${wasm}") as ${key}"
	id=$(stellar contract deploy "${sandbox_network_args[@]}" --source-account "${key}" \
		--wasm "${wasm}" "$@") || die "deploying ${role}: stellar contract deploy failed"
	[ -n "${id}" ] || die "deploying ${role}: stellar contract deploy printed no contract id"
	printf '%s\n' "${id}"
}

# deploy_sac ROLE KEY ASSET — deploys the Stellar Asset Contract for a
# **classic asset this deploy just issued** ("USDC:G…", "BLND:G…") and
# prints its id; the caller registers the role. Never called for "native"
# on testnet — see native_asset_id below.
deploy_sac() {
	local role=$1 key=$2 asset=$3 id
	sandbox_require_network
	log "deploying ${role} as the Stellar Asset Contract for ${asset}"
	id=$(stellar contract asset deploy "${sandbox_network_args[@]}" --source-account "${key}" \
		--asset "${asset}") || die "deploying ${role}: stellar contract asset deploy ${asset} failed"
	[ -n "${id}" ] || die "deploying ${role}: stellar contract asset deploy ${asset} printed no contract id"
	printf '%s\n' "${id}"
}

# native_asset_id — prints the id of the native asset's already-deployed
# Stellar Asset Contract. Unlike a fresh standalone network, testnet has
# carried this contract since long before this deploy, so `stellar contract
# asset deploy --asset native` would fail here; `contract id asset` derives
# the same id it always resolves to, purely from the network passphrase, no
# source account or transaction involved.
native_asset_id() {
	local id
	sandbox_require_network
	log "deriving the native asset's Stellar Asset Contract id (already deployed on testnet)"
	id=$(stellar contract id asset "${sandbox_network_args[@]}" --asset native) \
		|| die "deriving XLM: stellar contract id asset --asset native failed"
	[ -n "${id}" ] || die "deriving XLM: stellar contract id asset printed no id"
	printf '%s\n' "${id}"
}

# current_ledger_sequence — prints testnet's current ledger sequence via a
# raw getLatestLedger call. Used only to give step 4's token approval a
# live-until ledger comfortably ahead of "now" — see this file's header on
# why the sandbox's own fixed literal cannot be reused here.
current_ledger_sequence() {
	local body seq
	body=$(curl -fsS --max-time 10 -H 'Content-Type: application/json' \
		-d '{"jsonrpc":"2.0","id":1,"method":"getLatestLedger"}' \
		"${TESTNET_RPC_URL}" 2>/dev/null) || body=""
	seq=$(printf '%s' "${body}" | jq -r '.result.sequence // empty' 2>/dev/null) || seq=""
	[ -n "${seq}" ] || die "reading testnet's current ledger: getLatestLedger answered nothing usable: ${body}"
	printf '%s\n' "${seq}"
}

# trust KEY ASSET — opens KEY's trustline to ASSET. A classic asset's SAC
# mints into a trustline, so an account with none is refused with
# "trustline entry is missing"; contracts need no trustline, only G
# accounts do.
trust() {
	local key=$1 asset=$2
	sandbox_require_network
	log "opening ${key}'s trustline to ${asset%%:*}"
	stellar tx new change-trust "${sandbox_network_args[@]}" --source-account "${key}" --line "${asset}" >/dev/null \
		|| die "opening trustlines: change-trust ${asset%%:*} for ${key} failed"
}

# reserve_metadata INDEX C_FACTOR L_FACTOR UTIL — the pool's ReserveConfig
# as JSON. Everything but the four arguments is the contracts' own
# `default_reserve_metadata`; factors and utilisation are 7-decimal, and
# supply_cap is an i128 so it is a string.
reserve_metadata() {
	printf '{"c_factor":%s,"decimals":7,"enabled":true,"index":%s,"l_factor":%s,"max_util":9500000,"r_base":100000,"r_one":500000,"r_three":15000000,"r_two":5000000,"reactivity":20,"supply_cap":"1000000000000000000","util":%s}' \
		"$2" "$1" "$3" "$4"
}

# json_field JSON PATH STEP — the value jq's PATH selects in JSON, or a die
# naming STEP.
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
for key in "${TESTNET_KEY_ISSUER}" "${TESTNET_KEY_ADMIN}" "${TESTNET_KEY_BORROWER}" "${TESTNET_KEY_FILLER}"; do
	generate_key "${key}"
done

ISSUER=$(stellar keys address "${TESTNET_KEY_ISSUER}") || die "step 1 keys: reading ${TESTNET_KEY_ISSUER}'s address failed"
ADMIN=$(stellar keys address "${TESTNET_KEY_ADMIN}") || die "step 1 keys: reading ${TESTNET_KEY_ADMIN}'s address failed"
BORROWER=$(stellar keys address "${TESTNET_KEY_BORROWER}") || die "step 1 keys: reading ${TESTNET_KEY_BORROWER}'s address failed"
FILLER=$(stellar keys address "${TESTNET_KEY_FILLER}") || die "step 1 keys: reading ${TESTNET_KEY_FILLER}'s address failed"
log "issuer ${ISSUER}"
log "admin ${ADMIN}"
log "borrower ${BORROWER}"
log "filler ${FILLER}"

require_funded_testnet "${TESTNET_KEY_ISSUER}" "${ISSUER}"
require_funded_testnet "${TESTNET_KEY_ADMIN}" "${ADMIN}"
require_funded_testnet "${TESTNET_KEY_BORROWER}" "${BORROWER}"
require_funded_testnet "${TESTNET_KEY_FILLER}" "${FILLER}"

########################################################################
# Preflight: the pool wasm, uploaded on its own, before anything else
# touches the chain past funding.
#
# versions.env's five wasm files were built against soroban-sdk 22; testnet
# runs protocol 28. Blend's own testnet deployment runs these exact
# contracts, so this is expected to succeed — but if protocol 28 refuses
# them, every later step would fail for the same reason under a more
# confusing name, so this is where that question gets answered, and where
# this script stops if the answer is no. POOL_HASH is reused by step 5
# rather than uploaded a second time.
########################################################################

log "=== preflight: pool wasm upload ==="
log "the pin is built against soroban-sdk 22; testnet runs protocol 28 — if this is rejected, stop here rather than guessing at a workaround"
sandbox_require_network
# Only stdout is captured, exactly as deploy_wasm() above does: the CLI's
# own progress/info lines (an already-installed "skipping" notice among
# them) go to stderr and must not end up mixed into POOL_HASH. On a
# rejection the CLI's own error text still reaches the terminal directly,
# uncaptured; die() here only has to say what step failed and why, not
# repeat text that is already on screen.
if ! POOL_HASH=$(stellar contract upload "${sandbox_network_args[@]}" \
	--source-account "${TESTNET_KEY_ADMIN}" --wasm "${wasm_dir}/pool_v2.0.0.wasm"); then
	die "preflight: uploading pool_v2.0.0.wasm was rejected by testnet — see the CLI's own error above; the pin is built against soroban-sdk 22, testnet runs protocol 28, and this is not something to work around by re-pinning"
fi
[ -n "${POOL_HASH}" ] || die "preflight: stellar contract upload printed no hash"
log "pool wasm accepted, hash ${POOL_HASH}"

########################################################################
# 2. Tokens
#
# SAC: mint --to Address --amount i128, signed by the issuer.
########################################################################

log "=== step 2: tokens ==="
USDC=$(deploy_sac usdc "${TESTNET_KEY_ISSUER}" "USDC:${ISSUER}")
sandbox_register_role "${USDC}" usdc
BLND=$(deploy_sac blnd "${TESTNET_KEY_ISSUER}" "BLND:${ISSUER}")
sandbox_register_role "${BLND}" blnd
XLM=$(native_asset_id)
sandbox_register_role "${XLM}" xlm

trust "${TESTNET_KEY_ADMIN}" "USDC:${ISSUER}"
trust "${TESTNET_KEY_ADMIN}" "BLND:${ISSUER}"
trust "${TESTNET_KEY_FILLER}" "USDC:${ISSUER}"
# The borrower is paid its borrowed USDC by the pool, so it needs the
# trustline even though it is never minted to.
trust "${TESTNET_KEY_BORROWER}" "USDC:${ISSUER}"

invoke "${TESTNET_KEY_ISSUER}" "${BLND}" mint --to "${ADMIN}" --amount "${MINT_BLND_ADMIN}" >/dev/null
invoke "${TESTNET_KEY_ISSUER}" "${USDC}" mint --to "${ADMIN}" --amount "${MINT_USDC_ADMIN}" >/dev/null
invoke "${TESTNET_KEY_ISSUER}" "${USDC}" mint --to "${FILLER}" --amount "${MINT_USDC_FILLER}" >/dev/null

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
ORACLE=$(deploy_wasm oracle "${TESTNET_KEY_ADMIN}" "${wasm_dir}/mock_sep_40_oracle.wasm")
sandbox_register_role "${ORACLE}" oracle
invoke "${TESTNET_KEY_ADMIN}" "${ORACLE}" set_data \
	--admin "${ADMIN}" \
	--base '{"Other":"USD"}' \
	--assets "[{\"Stellar\":\"${XLM}\"},{\"Stellar\":\"${USDC}\"}]" \
	--decimals 7 \
	--resolution 300 >/dev/null
invoke "${TESTNET_KEY_ADMIN}" "${ORACLE}" set_price_stable \
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
COMET=$(deploy_wasm comet "${TESTNET_KEY_ADMIN}" "${wasm_dir}/comet.wasm")
sandbox_register_role "${COMET}" comet

current_ledger=$(current_ledger_sequence)
# A margin of 500,000 ledgers past testnet's own current one — about 29
# days at ~5s ledgers, comfortably past any plausible soak — rather than
# the sandbox's fixed 500,000 *absolute*, which testnet (already in the
# millions) would have passed before this script ever ran.
ALLOWANCE_UNTIL_LEDGER=$((current_ledger + 500000))
log "testnet's current ledger is ${current_ledger}; approving until ${ALLOWANCE_UNTIL_LEDGER}"

for token in "${BLND}" "${USDC}"; do
	invoke "${TESTNET_KEY_ADMIN}" "${token}" approve \
		--from "${ADMIN}" --spender "${COMET}" \
		--amount "${ALLOWANCE}" --live_until_ledger "${ALLOWANCE_UNTIL_LEDGER}" >/dev/null
done
invoke "${TESTNET_KEY_ADMIN}" "${COMET}" init \
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
# factory is then deployed with that same salt. The two must agree, and the
# check below is the whole reason the prediction is safe: a mismatch means
# the backstop trusts a factory that does not exist.
#
#   backstop __constructor(backstop_token, emitter, blnd_token,
#                          usdc_token, pool_factory,
#                          drop_list: Vec<(Address, i128)>)
#   pool-factory __constructor(pool_init_meta: PoolInitMeta
#                              { backstop, blnd_id, pool_hash: BytesN<32> })
#
# The emitter is not deployed: it only matters to BLND emissions, which
# this soak never starts, and the backstop never calls into the address it
# is given unless drop()/distribute() is invoked. The admin's own address
# stands in.
########################################################################

log "=== step 5: backstop and factory ==="
sandbox_require_network
FACTORY=$(stellar contract id wasm "${sandbox_network_args[@]}" \
	--salt "${FACTORY_SALT}" --source-account "${TESTNET_KEY_ADMIN}") \
	|| die "step 5 factory: predicting the factory's address failed"
[ -n "${FACTORY}" ] || die "step 5 factory: stellar contract id wasm printed no address"
sandbox_register_role "${FACTORY}" "pool factory (predicted)"

BACKSTOP=$(deploy_wasm backstop "${TESTNET_KEY_ADMIN}" "${wasm_dir}/backstop_v2.0.0.wasm" -- \
	--backstop_token "${COMET}" \
	--emitter "${ADMIN}" \
	--blnd_token "${BLND}" \
	--usdc_token "${USDC}" \
	--pool_factory "${FACTORY}" \
	--drop_list '[]')
sandbox_register_role "${BACKSTOP}" backstop

log "deploying the pool factory at the predicted address"
sandbox_require_network
FACTORY_DEPLOYED=$(stellar contract deploy "${sandbox_network_args[@]}" \
	--source-account "${TESTNET_KEY_ADMIN}" --wasm "${wasm_dir}/pool-factory_v2.0.0.wasm" \
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
POOL=$(invoke "${TESTNET_KEY_ADMIN}" "${FACTORY}" deploy \
	--admin "${ADMIN}" \
	--name TestnetSoak \
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
config=$(invoke_view "${TESTNET_KEY_ADMIN}" "${POOL}" get_config)
status=$(json_field "${config}" '.status' "step 7 reserves")
[ "${status}" = "6" ] || die "step 7 reserves: the pool reports status ${status}, not 6 (Setup) — reserves can only be set without a timelock during Setup: ${config}"

invoke "${TESTNET_KEY_ADMIN}" "${POOL}" queue_set_reserve \
	--asset "${XLM}" --metadata "$(reserve_metadata 0 7500000 7500000 5000000)" >/dev/null
XLM_INDEX=$(invoke "${TESTNET_KEY_ADMIN}" "${POOL}" set_reserve --asset "${XLM}")
[ "${XLM_INDEX}" = "0" ] || die "step 7 reserves: XLM was given reserve index ${XLM_INDEX}, expected 0"

invoke "${TESTNET_KEY_ADMIN}" "${POOL}" queue_set_reserve \
	--asset "${USDC}" --metadata "$(reserve_metadata 1 9000000 9500000 8500000)" >/dev/null
USDC_INDEX=$(invoke "${TESTNET_KEY_ADMIN}" "${POOL}" set_reserve --asset "${USDC}")
[ "${USDC_INDEX}" = "1" ] || die "step 7 reserves: USDC was given reserve index ${USDC_INDEX}, expected 1"

reserve_list=$(invoke_view "${TESTNET_KEY_ADMIN}" "${POOL}" get_reserve_list)
reserve_count=$(json_field "${reserve_list}" 'length' "step 7 reserves")
[ "${reserve_count}" = "2" ] || die "step 7 reserves: the pool lists ${reserve_count} reserves, expected 2: ${reserve_list}"
log "reserves: ${reserve_list}"

########################################################################
# 8. Backstop funding and activation
#
#   backstop deposit(from: Address, pool_address: Address, amount: i128)
#   pool set_status(pool_status: u32)
#
# execute_set_pool_status(0) is the way out of Setup, not update_status() —
# see scripts/sandbox/deploy.sh's own comment for why, verified against
# this exact wasm. 0 (Admin Active) and 1 (Active) are the same thing to
# everything this bot does; get_config below proves the backstop threshold
# was met, whichever of the two the pool reports.
########################################################################

log "=== step 8: backstop funding ==="
invoke "${TESTNET_KEY_ADMIN}" "${COMET}" join_pool \
	--pool_amount_out "${BACKSTOP_SHARES}" \
	--max_amounts_in "[\"${JOIN_MAX_BLND}\",\"${JOIN_MAX_USDC}\"]" \
	--user "${ADMIN}" >/dev/null
invoke "${TESTNET_KEY_ADMIN}" "${BACKSTOP}" deposit \
	--from "${ADMIN}" --pool_address "${POOL}" --amount "${BACKSTOP_SHARES}" >/dev/null
invoke "${TESTNET_KEY_ADMIN}" "${POOL}" set_status --pool_status 0 >/dev/null

config=$(invoke_view "${TESTNET_KEY_ADMIN}" "${POOL}" get_config)
status=$(json_field "${config}" '.status' "step 8 activation")
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
invoke "${TESTNET_KEY_ADMIN}" "${POOL}" submit \
	--from "${ADMIN}" --spender "${ADMIN}" --to "${ADMIN}" \
	--requests "[{\"request_type\":0,\"address\":\"${USDC}\",\"amount\":\"${ADMIN_USDC_SUPPLY}\"}]" >/dev/null

invoke "${TESTNET_KEY_BORROWER}" "${POOL}" submit \
	--from "${BORROWER}" --spender "${BORROWER}" --to "${BORROWER}" \
	--requests "[{\"request_type\":2,\"address\":\"${XLM}\",\"amount\":\"${BORROWER_XLM_COLLATERAL}\"},{\"request_type\":4,\"address\":\"${USDC}\",\"amount\":\"${BORROWER_USDC_DEBT}\"}]" >/dev/null

positions=$(invoke_view "${TESTNET_KEY_ADMIN}" "${POOL}" get_positions --address "${BORROWER}")
collateral_count=$(json_field "${positions}" '.collateral | length' "step 9 borrower")
liability_count=$(json_field "${positions}" '.liabilities | length' "step 9 borrower")
if [ "${collateral_count}" != "1" ] || [ "${liability_count}" != "1" ]; then
	die "step 9 borrower: expected one collateral and one liability, got ${collateral_count} and ${liability_count}: ${positions}"
fi
log "borrower positions: ${positions}"

########################################################################
# 10. testnet.env
#
# The filler's secret is read here and nowhere else, straight into the file
# env_write has already restricted to mode 0600. It is never assigned to a
# shell variable that outlives this heredoc, never passed as an argument
# and never logged — which is why this is the last step and why the log
# line below names the file rather than its contents. See
# scripts/sandbox/deploy.sh's own comment for why the heredoc itself leaves
# no separate durable copy.
#
# TESTNET_RPC_URL is written from the variable require_testnet_network
# exported (via sandbox_set_network), not from a second reconstruction of
# the URL: what the node was checked on is what run-bot.sh and crash.sh are
# pointed at.
########################################################################

log "=== step 10: testnet.env ==="
env_write "${env_file}" <<EOF
TESTNET_RPC_URL="${SANDBOX_RPC_URL}"
TESTNET_PASSPHRASE="${TESTNET_PASSPHRASE}"
TESTNET_POOL="${POOL}"
TESTNET_XLM="${XLM}"
TESTNET_USDC="${USDC}"
TESTNET_BLND="${BLND}"
TESTNET_ORACLE="${ORACLE}"
TESTNET_BORROWER="${BORROWER}"
TESTNET_FILLER="${FILLER}"
TESTNET_ADMIN="${ADMIN}"
TESTNET_FILLER_SECRET_KEY="$(stellar keys secret "${TESTNET_KEY_FILLER}")"
EOF
grep -q '^TESTNET_FILLER_SECRET_KEY="S' "${env_file}" \
	|| die "step 10 testnet.env: the filler's secret key did not reach ${env_file}"

log "wrote ${env_file} (mode 0600)"
log "deploy complete"
printf '%s\n' "${env_file}"
