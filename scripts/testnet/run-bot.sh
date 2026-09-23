#!/usr/bin/env bash
# run-bot.sh [--armed] — the only path by which DRY_RUN=false ever reaches
# the liquidator binary on testnet.
#
# Two modes:
#
#   (no argument)  Dry run against target/testnet/pools.toml — the observe
#                  stage's own pools file, naming Blend's testnet pool.
#                  Reads that file; never writes it. Port 18081, database
#                  testnet_soak, log target/testnet/dry-run.log. No signing
#                  key reaches the binary: both key variables are unset
#                  below and neither is set again, and DRY_RUN is always
#                  true here.
#
#   --armed        DRY_RUN=false against target/testnet/pools.armed.toml,
#                  which this script (re)generates every run from
#                  target/testnet/testnet.env — the pool deploy.sh stood
#                  up. Port 18082, database testnet_armed, log
#                  target/testnet/armed.log. Requires testnet.env to exist
#                  and reads TESTNET_FILLER_SECRET_KEY from it into the
#                  child's environment only: never an argument (argv is
#                  world-readable), never echoed, never logged.
#
# The gate verifies exactly the URL the binary is then handed. In armed
# mode that URL is testnet.env's TESTNET_RPC_URL — the one deploy.sh
# verified and recorded, which sourcing the file puts in place of whatever
# the environment or lib.sh's default said — so the file is sourced, and its
# passphrase compared with the pin, before the gate runs, exactly as
# crash.sh orders it. In dry-run mode it is TESTNET_RPC_URL from the
# environment, or lib.sh's default. Either way the gate is
# require_testnet_network, and the binary's RPC_URL is the SANDBOX_RPC_URL
# that gate exported: the verified string itself, not a second reading of a
# variable something could have changed since.
#
# The binary's environment is this script's, not the operator's shell:
# every setting the bot reads that this script does not set on purpose is
# unset before the exports below, and the comment there says why each group
# is.
#
# Execs the binary as its very last act (after redirecting output to this
# run's log file), so signals — SIGINT, SIGTERM — reach it directly rather
# than a shell wrapper that would have to relay them, and the pid this
# script started with is the bot's own: `kill -TERM <pid>` stops it.
#
# TESTNET_RUN_PORT and TESTNET_RUN_DATABASE, together or not at all,
# override the mode's port and database name, for exercising this script
# beside a live dry run. A run with them set logs to
# target/testnet/run-<database>.log, a name no mode's own transcript can
# take, so nothing it does lands in the live run's port, database or log.
# Dry run only: armed, a second bot would sign with the live run's key. They are
# not a way to run a second instance of a mode on that mode's own database
# — two bots on one store (and, armed, one key) is the deployment
# contract's overlapping-instance case, noise a soak must not measure — so
# TESTNET_RUN_DATABASE refuses either mode's own database name. The
# database must already exist, as either mode's must (docs/testnet-soak.md,
# "The database").
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/testnet/lib.sh
source "${script_dir}/lib.sh"

if [ "$#" -gt 1 ]; then
	die "run-bot: too many arguments — usage: run-bot.sh [--armed]"
fi
armed=false
case "${1:-}" in
"") ;;
--armed) armed=true ;;
*) die "run-bot: unknown argument '$1' — usage: run-bot.sh [--armed]" ;;
esac

run_port="${TESTNET_RUN_PORT:-}"
run_database="${TESTNET_RUN_DATABASE:-}"
if [ -n "${run_port}" ] || [ -n "${run_database}" ]; then
	if [ -z "${run_port}" ] || [ -z "${run_database}" ]; then
		die "run-bot: TESTNET_RUN_PORT and TESTNET_RUN_DATABASE come together — a run beside a live one needs its own port and its own database (see this script's header)"
	fi
	# Dry run only. Armed, a second bot would sign with the live run's own
	# key against the live run's own pool, from another store: the
	# deployment contract's overlapping-instance case on one key, which a
	# separate port and database do nothing to prevent.
	[ "${armed}" = false ] \
		|| die "run-bot: TESTNET_RUN_PORT and TESTNET_RUN_DATABASE are for a dry run beside a live one — with --armed they would start a second bot signing with the live run's key"
	case "${run_database}" in
	testnet_soak | testnet_armed)
		die "run-bot: TESTNET_RUN_DATABASE names ${run_database}, a mode's own database — it is for a run beside that mode's live one, never a second instance on the same store"
		;;
	[!a-z_]* | *[!a-z0-9_]*)
		die "run-bot: TESTNET_RUN_DATABASE must be a plain lower-case database name ([a-z_][a-z0-9_]*), got '${run_database}'"
		;;
	esac
fi

testnet_root="$(testnet_dir)"

if [ "${armed}" = true ]; then
	pool_env="${testnet_root}/testnet.env"
	[ -f "${pool_env}" ] \
		|| die "run-bot: --armed requires ${pool_env} — run scripts/testnet/deploy.sh first"
	# The comparison passphrase stays lib.sh's own. testnet.env sets
	# TESTNET_PASSPHRASE too, and sourcing it overwrites lib.sh's pin —
	# which require_testnet_network then compares getNetwork's answer
	# against — so an env file naming a network the pins do not is refused
	# here, rather than quietly adopted as the thing the gate checks for.
	pinned_passphrase="${TESTNET_PASSPHRASE}"
	# Both unset before the source, so the check below reads testnet.env's
	# own values: lib.sh has already set each (the pinned passphrase, and a
	# URL defaulted or taken from the environment), and a file missing
	# either would otherwise pass as if it named them.
	unset TESTNET_RPC_URL TESTNET_PASSPHRASE
	# shellcheck source=/dev/null
	source "${pool_env}"
	for key in TESTNET_RPC_URL TESTNET_PASSPHRASE TESTNET_POOL TESTNET_XLM TESTNET_USDC TESTNET_BORROWER TESTNET_FILLER_SECRET_KEY; do
		[ -n "${!key:-}" ] || die "run-bot: ${pool_env} does not define ${key}"
	done
	[ "${TESTNET_PASSPHRASE}" = "${pinned_passphrase}" ] \
		|| die "run-bot: ${pool_env} names the passphrase '${TESTNET_PASSPHRASE}', not testnet's pinned '${pinned_passphrase}' — refusing to touch a network that is not testnet's own"
fi

# The gate, on the URL the binary is about to be handed (see the header).
# Nothing below talks to a network, and nothing is exec'd, until this has
# passed.
require_testnet_network

repo_root="$(cd "${script_dir}/../.." && pwd)"
binary="${repo_root}/target/debug/liquidator"
[ -x "${binary}" ] || die "run-bot: ${binary} does not exist or is not executable — build it first (cargo build)"

: "${DATABASE_URL:?run-bot: DATABASE_URL must be set in the environment}"

mkdir -p "${testnet_root}"

# Swaps DATABASE_URL's own database name for this mode's, keeping whatever
# else the URL carries (host, credentials, a query string) untouched.
db_no_query="${DATABASE_URL%%\?*}"
db_query="${DATABASE_URL#"${db_no_query}"}"
db_server="${db_no_query%/*}"

if [ "${armed}" = true ]; then
	# Regenerated every armed run from the addresses testnet.env names —
	# never target/testnet/pools.toml, which the observe stage owns and
	# this script never opens for writing in either mode.
	pools_file="${testnet_root}/pools.armed.toml"
	cat >"${pools_file}" <<EOF
# Generated by scripts/testnet/run-bot.sh --armed from ${pool_env}.
# Regenerated on every armed run — never hand-edit; re-run deploy.sh and
# this script instead.
[[pools]]
address = "${TESTNET_POOL}"
primary_asset = "${TESTNET_XLM}"
min_primary_collateral = "1000000000"
min_health_factor = 1.5
default_profit_bps = 100
fill_objective = "earliest-profitable"
supported_bid = ["${TESTNET_USDC}"]
supported_lot = ["*"]
EOF

	# The borrower deploy.sh funded took its position before this bot
	# existed, so no event of the range this run polls ever names it and
	# the tracker would follow nobody. A seed file is how an account that
	# is already in a pool becomes one the bot values — the sandbox test
	# writes the same one for the same reason.
	seed_file="${testnet_root}/seed.armed.toml"
	cat >"${seed_file}" <<EOF
# Generated by scripts/testnet/run-bot.sh --armed from ${pool_env}.
[accounts]
"${TESTNET_POOL}" = ["${TESTNET_BORROWER}"]
EOF

	mode=armed
	pool_address="${TESTNET_POOL}"
	port=18082
	db_name=testnet_armed
	log_file="${testnet_root}/armed.log"
else
	pools_file="${testnet_root}/pools.toml"
	[ -f "${pools_file}" ] \
		|| die "run-bot: ${pools_file} does not exist — the observe stage's pools file must exist first (see docs/testnet-soak.md)"

	# Optional here, unlike the armed mode: the observe stage follows a
	# pool whose borrowers are other people's, so the accounts worth
	# valuing are whatever `cargo run --example scan_borrowers` found in
	# the pool's own recent events. Without it the bot still runs and
	# follows every account that acts while it watches.
	seed_file="${testnet_root}/seed.toml"
	[ -f "${seed_file}" ] || seed_file=""

	mode=dry-run
	pool_address="$(sed -n 's/^address = "\(.*\)"/\1/p' "${pools_file}" | head -n1)"
	[ -n "${pool_address}" ] || die "run-bot: ${pools_file} names no pool address"
	port=18081
	db_name=testnet_soak
	log_file="${testnet_root}/dry-run.log"
fi

if [ -n "${run_database}" ]; then
	port="${run_port}"
	db_name="${run_database}"
	log_file="${testnet_root}/run-${run_database}.log"
fi

bot_database_url="${db_server}/${db_name}${db_query}"

log "mode: ${mode}"
log "network: testnet (${SANDBOX_RPC_URL})"
log "pool: ${pool_address}"
log "port: ${port}, database: ${db_name}"
log "log: ${log_file}"

# The binary's environment is this script's, not the operator's shell.
# Every setting the bot reads — src/config.rs's clap `env =` names, and the
# ones read straight from the environment (the two signing keys in
# src/main.rs; DATABASE_URL, RPC_API_KEY and TELEGRAM_BOT_TOKEN in
# Args::service and Args::chain) — that this script does not set on purpose
# below is unset here, in four groups:
#
# - The signing keys. AUCTIONEER_SECRET_KEY is never this tier's: armed,
#   the auctioneer would sign its creations on testnet with whatever key
#   the shell held, plausibly a mainnet one. FILLER_SECRET_KEY is set again
#   below in armed mode only, from testnet.env, so a dry run is handed no
#   key at all rather than merely not asked to use one.
# - Other deployments' credentials and channels. RPC_API_KEY and
#   RPC_API_KEY_HEADER would send another provider's credential, as a
#   header, to SDF's public testnet RPC; TELEGRAM_BOT_TOKEN and
#   TELEGRAM_CHAT_ID would send testnet alerts to another deployment's chat.
#   With the Telegram pair gone no bot token can reach this process, which
#   is what makes an inherited RUST_LOG harmless to one — see RUST_LOG below
#   for the one secret that remains, the armed mode's key.
# - Which network, which pools, which run. NETWORK_PASSPHRASE conflicts
#   with the NETWORK=testnet set below, and POOLS_TOML with POOLS_FILE —
#   even an empty POOLS_TOML= — so clap refuses either pair at parse and an
#   inherited one would stop the run rather than change it; they are cleared
#   so that a shell exported for another deployment starts this run instead
#   of failing it. SEED_FILE is set below only when this mode has one, so an
#   inherited one would seed another pool's accounts into this run. RUN_MODE,
#   HTTP_PORT and HTTP_BIND_ADDR are this script's too: check-config would
#   validate and exit, PORT wins over HTTP_PORT anyway, and a 0.0.0.0 bind
#   would expose the unauthenticated endpoints.
# - The tuning knobs. A soak measures the defaults docs/configuration.md and
#   docs/testnet-soak.md describe; a shell carrying another deployment's
#   settings would otherwise retune a testnet run without a word.
#
# A setting added to src/config.rs belongs in this list as well, or in the
# exports below in every mode. src/config.rs's
# the_testnet_runner_clears_or_sets_every_real_setting fails otherwise — an
# export in one arm of an if does not cover the other — and fails on a name
# here or exported below that the bot does not read, since unset or export
# of a misspelt name clears or sets nothing.
unset \
	AUCTIONEER_SECRET_KEY FILLER_SECRET_KEY \
	RPC_API_KEY RPC_API_KEY_HEADER TELEGRAM_BOT_TOKEN TELEGRAM_CHAT_ID \
	NETWORK_PASSPHRASE POOLS_TOML SEED_FILE RUN_MODE HTTP_PORT HTTP_BIND_ADDR \
	BASE_FEE HIGH_FEE TX_POLL_LEDGERS DATABASE_MAX_CONNECTIONS \
	USER_REFRESH_LEDGERS REFRESH_BATCH FULL_SCAN_LEDGERS SCAN_HF_THRESHOLD \
	LIQ_HF_THRESHOLD TARGET_HF ORACLE_SCAN_LEDGERS PRICE_DELTA_BPS \
	PLAN_ITERATIONS STARTUP_DELAY_LEDGERS HF_SAFETY_MULTIPLIER \
	REPLAN_LEDGERS REPLAN_NEAR_LEDGERS XLM_FEE_RESERVE \
	HIGH_FEE_PROFIT_THRESHOLD INVENTORY_REFRESH_SECS \
	FAILURE_NOTIFICATION_COOLDOWN_HOURS SEED_HF_MAX HEALTH_MAX_LAG_LEDGERS

export NETWORK=testnet
export RPC_URL="${SANDBOX_RPC_URL}"
export POLL_INTERVAL_MS=5000
export LOG_FORMAT=json
# docs/deploy.md §6: never run a deployment that holds a secret at
# RUST_LOG=trace, since logging there — dependencies' included — is not
# audited for secrets. Both modes hold one: the armed bot the filler's key,
# and either bot DATABASE_URL, whose password is exactly the kind of value
# this repository keeps off argv. So a RUST_LOG naming `trace` is refused
# outright in both modes. The armed bot always runs at this script's own
# filter besides; a dry run keeps any other RUST_LOG an operator set.
default_filter="info,blend_liquidator=debug"
case "${RUST_LOG:-}" in
*[Tt][Rr][Aa][Cc][Ee]*)
	die "run-bot: RUST_LOG='${RUST_LOG}' names trace, and this run holds DATABASE_URL (and, armed, a signing key) — logging at trace is not audited for secrets (docs/deploy.md §6)"
	;;
esac
if [ "${armed}" = true ]; then
	if [ -n "${RUST_LOG:-}" ] && [ "${RUST_LOG}" != "${default_filter}" ]; then
		log "armed: ignoring the inherited RUST_LOG — an armed run always logs at ${default_filter}"
	fi
	export RUST_LOG="${default_filter}"
else
	export RUST_LOG="${RUST_LOG:-${default_filter}}"
fi
export PORT="${port}"
export DATABASE_URL="${bot_database_url}"
export POOLS_FILE="${pools_file}"
# Always empty, never the default: SEED_URL's own default is the public
# analytics API, which answers for mainnet pools. A testnet run that asked
# it would seed mainnet accounts into a testnet pool.
export SEED_URL=""
if [ -n "${seed_file}" ]; then
	export SEED_FILE="${seed_file}"
	log "seed: ${seed_file}"
fi

if [ "${armed}" = true ]; then
	export DRY_RUN=false
	export FILLER_SECRET_KEY="${TESTNET_FILLER_SECRET_KEY}"
else
	export DRY_RUN=true
fi

# Everything from here on — the binary's own stdout/stderr — goes to the
# log file this run announced above, appended rather than truncated: two
# runs of the same mode leave one growing transcript rather than either
# silently discarding the other's.
#
# One run per database at a time. Two bots on one store — and, armed, one
# key — is the deployment contract's overlapping-instance case, which the
# port clash does not reliably prevent: the bot binds its HTTP port after it
# may already be polling. The lock is taken on a descriptor the final exec
# hands to the binary, so it is held for exactly as long as the bot runs and
# released the moment it exits, however it exits.
exec 9>"${testnet_root}/${db_name}.lock"
flock -n 9 \
	|| die "run-bot: another run already holds ${testnet_root}/${db_name}.lock — one run per database at a time (stop it by pid first; see docs/testnet-soak.md, \"Stopping the bots\")"
exec >>"${log_file}" 2>&1
exec "${binary}"
