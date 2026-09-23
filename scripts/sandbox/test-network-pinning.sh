#!/usr/bin/env bash
# test-network-pinning.sh — the guard on the one thing the whole tier rests
# on: every `stellar` call a sandbox script makes must take its network
# from lib.sh's own flags, never from the environment.
#
# The CLI resolves an ad-hoc network from STELLAR_RPC_URL and
# STELLAR_NETWORK_PASSPHRASE *ahead of* an explicit `--network`, so a
# sandbox that named a network would generate keys, fund them, deploy and
# sign on whatever an operator happened to have exported — while
# require_standalone_network went on confirming localhost, which is not
# the thing those calls were using. lib.sh answers that twice: it unsets
# the CLI's network and signing variables at source time, and every call
# passes "${sandbox_network_args[@]}" — the URL that was verified and the
# passphrase it was verified by. This is what proves the second half.
#
# Offline, and it needs no node: `stellar contract id wasm` derives a
# contract id from the source account, the salt and the **network
# passphrase** alone, which makes the id a cheap deterministic fingerprint
# of the network the CLI thinks it is on. The only requirement is the CLI
# itself. Run by hand and by .github/workflows/sandbox.yml, before it
# starts a network.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/sandbox/lib.sh
source "${script_dir}/lib.sh"

command -v stellar >/dev/null 2>&1 \
	|| die "test-network-pinning: the stellar CLI is not on PATH — .devcontainer/post-create.sh and .github/workflows/sandbox.yml install it"

# Never contacted: the derivation below is arithmetic over the account, the
# salt and the passphrase. Port 1 rather than the sandbox's own, so a
# future call that did reach for a node fails here instead of quietly
# borrowing a sandbox somebody left running.
sandbox_set_network "http://127.0.0.1:1/rpc"

# Both are inputs to the derived id, so all three derivations below must
# use the same pair for the comparison to mean anything. The account is a
# literal G… address (the same one the crate's unit tests use), never an
# identity name, so this test needs no keystore.
SALT=0000000000000000000000000000000000000000000000000000000000000001
ACCOUNT=GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE

# The public network a Soroban developer's shell profile plausibly points
# at — the CLI's own documented variables — and the one no sandbox call may
# follow.
POLLUTED_RPC_URL=https://soroban-testnet.stellar.org
POLLUTED_PASSPHRASE="Test SDF Network ; September 2015"

pass=0
fail=0

ok() {
	printf 'ok - %s\n' "$1"
	pass=$((pass + 1))
}

no() {
	printf 'FAIL - %s\n' "$1"
	fail=$((fail + 1))
}

# derive — the contract id the CLI derives through the sandbox's own flag
# array. Whichever network it resolved is in the answer.
#
# sandbox_require_network first, as every expansion of the array does: a
# call made before the gate would take the CLI's own default, which is a
# public network.
derive() {
	sandbox_require_network
	stellar contract id wasm "${sandbox_network_args[@]}" \
		--salt "${SALT}" --source-account "${ACCOUNT}"
}

pinned=$(derive) || die "test-network-pinning: the derivation in a clean environment failed"
[ -n "${pinned}" ] || die "test-network-pinning: the derivation in a clean environment printed nothing"
printf 'the sandbox flags derive %s\n' "${pinned}"

# The same call with the CLI's network variables exported. They are
# exported inside the command substitution's subshell, so nothing leaks
# into the derivations either side — and they are set here rather than
# inherited, because sourcing lib.sh unset them.
polluted=$(
	export STELLAR_RPC_URL="${POLLUTED_RPC_URL}"
	export STELLAR_NETWORK_PASSPHRASE="${POLLUTED_PASSPHRASE}"
	export STELLAR_NETWORK=testnet
	derive
) || die "test-network-pinning: the derivation under the polluted environment failed"

if [ "${polluted}" = "${pinned}" ]; then
	ok "the flags beat STELLAR_RPC_URL, STELLAR_NETWORK_PASSPHRASE and STELLAR_NETWORK"
else
	no "the environment moved the network: the flags alone derive ${pinned}, with those variables exported ${polluted}"
fi

# The negative control, in a clean environment: a different network must
# derive a different id, or the assertion above would hold however
# thoroughly the flags were ignored.
named=$(stellar contract id wasm --network testnet --salt "${SALT}" --source-account "${ACCOUNT}") \
	|| die "test-network-pinning: deriving against testnet failed"
if [ "${named}" != "${pinned}" ]; then
	ok "a different network derives a different id (testnet: ${named}), so this test can fail"
else
	no "testnet derived the same id ${named} — the comparison above proves nothing"
fi

# The ordering guard. Without flags the CLI does not fail: it resolves its
# own default, which is a *public* network — `stellar contract id wasm`
# with a cleared environment and no flags derives testnet's id, the same
# one the third case above proves is a different network. So a call made
# before require_standalone_network has run has to die, and this is the
# case that says it does.
#
# A child process, because lib.sh has already been sourced and the gate
# already run in this shell: a fresh source is what "before any gate"
# means.
#
# And SANDBOX_RPC_URL is *exported into* that child, deliberately: the
# guard must key on the flag array sandbox_set_network builds, never on a
# variable an operator's shell can supply. An inherited SANDBOX_RPC_URL
# that satisfied the guard would let a flagless call through — onto the
# CLI's own default, a public network — which is the one thing the guard
# exists to stop. lib.sh answers it twice, by unsetting the name at source
# time and by testing the array itself; either alone would pass this case,
# and both together are what the file promises.
GUARD_MESSAGE="sandbox network not verified: call require_standalone_network first"

before=$(
	SANDBOX_RPC_URL=http://localhost:8000/rpc bash -c '
		set -euo pipefail
		source "$1/lib.sh"
		sandbox_require_network
		echo "the guard let a call through before the gate"
	' guard "${script_dir}" 2>&1
) && before_status=0 || before_status=$?

case "${before}" in
*"${GUARD_MESSAGE}"*) guard_said_so=yes ;;
*) guard_said_so=no ;;
esac

if [ "${before_status}" -ne 0 ] && [ "${guard_said_so}" = yes ]; then
	ok "a CLI call before the gate dies, naming require_standalone_network, even with SANDBOX_RPC_URL exported"
else
	no "a CLI call before the gate exited ${before_status} saying: ${before}"
fi

# And the same guard after the gate, which this shell has already passed:
# a subshell so that a guard that wrongly died ends the subshell rather
# than this script, and is reported as a failure like any other.
if (sandbox_require_network); then
	ok "the guard passes once require_standalone_network has built the flag array"
else
	no "the guard refused a network this shell has already verified"
fi

# The pin file itself cannot be redirected. versions.env supplies
# SANDBOX_PASSPHRASE — the single literal require_standalone_network
# compares getNetwork's answer against — as well as every wasm URL and the
# SHA-256 it is verified by, so an environment override of which file is
# read would hand an attacker both halves at once: a gate that passes on a
# public network, and artefacts that verify against whatever hashes that
# same file names. lib.sh therefore resolves it from its own directory and
# nothing else; VERSIONS_ENV names nothing.
#
# /dev/null is the sharpest form of the override: it parses, it is
# readable, and it defines nothing at all, so a lib.sh that honoured it
# would leave SANDBOX_PASSPHRASE empty rather than fail.
STANDALONE_PASSPHRASE="Standalone Network ; February 2017"

overridden=$(
	VERSIONS_ENV=/dev/null bash -c '
		set -euo pipefail
		source "$1/lib.sh"
		printf "%s" "${SANDBOX_PASSPHRASE:-}"
	' pin "${script_dir}" 2>&1
) || overridden="sourcing lib.sh under VERSIONS_ENV=/dev/null failed: ${overridden}"

if [ "${overridden}" = "${STANDALONE_PASSPHRASE}" ]; then
	ok "VERSIONS_ENV cannot redirect the pin file: SANDBOX_PASSPHRASE is still the standalone literal"
else
	no "VERSIONS_ENV=/dev/null changed the pins: SANDBOX_PASSPHRASE came back as '${overridden}', expected '${STANDALONE_PASSPHRASE}'"
fi

# ---- require_network_passphrase -------------------------------------------
#
# The helper require_standalone_network is now built from, tested directly
# rather than only through the standalone wrapper above. No live node here
# either: _sandbox_rpc_call is the one function that would reach for one,
# and it is private to lib.sh, so a case that defines a same-named function
# replaces it for anything that calls it afterwards — bash resolves a
# function call at call time, never at definition time. Each case below
# runs its stub and its call to require_network_passphrase inside one
# `$( … )` command substitution, which is already a subshell, so the stub
# never leaks into a later case or into anything above.
STUB_LABEL=stub
STUB_EXPECTED="Stub Network ; A"
STUB_OTHER="Stub Network ; B"

# 1. A matching passphrase passes and sets the flags: SANDBOX_RPC_URL
# becomes the URL just verified, and sandbox_require_network — the same
# guard invoke()/invoke_view() call before every `stellar` command — accepts
# the array require_network_passphrase built through sandbox_set_network.
match_result=$(
	_sandbox_rpc_call() { printf '{"result":{"passphrase":"%s"}}' "${STUB_EXPECTED}"; }
	require_network_passphrase "http://stub-match/rpc" "${STUB_EXPECTED}" "${STUB_LABEL}"
	sandbox_require_network
	printf 'url=%s args=%d' "${SANDBOX_RPC_URL}" "${#sandbox_network_args[@]}"
) && match_status=0 || match_status=$?

if [ "${match_status}" -eq 0 ] && [ "${match_result}" = "url=http://stub-match/rpc args=4" ]; then
	ok "require_network_passphrase passes on a matching passphrase and sets the flags (${match_result})"
else
	no "require_network_passphrase on a match: exit ${match_status}, got '${match_result}'"
fi

# 2. A mismatch dies, naming the label, the URL and both passphrases — the
# actual answer and what was expected.
mismatch_result=$(
	_sandbox_rpc_call() { printf '{"result":{"passphrase":"%s"}}' "${STUB_OTHER}"; }
	require_network_passphrase "http://stub-mismatch/rpc" "${STUB_EXPECTED}" "${STUB_LABEL}" 2>&1
) && mismatch_status=0 || mismatch_status=$?

case "${mismatch_result}" in
*"http://stub-mismatch/rpc"*"${STUB_OTHER}"*"${STUB_LABEL}"*"${STUB_EXPECTED}"*) mismatch_named=yes ;;
*) mismatch_named=no ;;
esac

if [ "${mismatch_status}" -ne 0 ] && [ "${mismatch_named}" = yes ]; then
	ok "require_network_passphrase dies on a mismatch, naming the label, the URL and both passphrases"
else
	no "require_network_passphrase on a mismatch: exit ${mismatch_status}, said: ${mismatch_result}"
fi

# 3. The public passphrase dies with its own message — distinct from case 2
# above — even when it is itself what was asked for: the degenerate case
# where EXPECTED is mainnet's own passphrase, so a comparison against
# EXPECTED alone would wrongly "pass" a call onto mainnet.
PUBLIC_PASSPHRASE="Public Global Stellar Network ; September 2015"

public_result=$(
	_sandbox_rpc_call() { printf '{"result":{"passphrase":"%s"}}' "${PUBLIC_PASSPHRASE}"; }
	require_network_passphrase "http://stub-public/rpc" "${PUBLIC_PASSPHRASE}" "${STUB_LABEL}" 2>&1
) && public_status=0 || public_status=$?

case "${public_result}" in
*"${PUBLIC_PASSPHRASE}"*"refusing to touch mainnet"*) public_named=yes ;;
*) public_named=no ;;
esac

if [ "${public_status}" -ne 0 ] && [ "${public_named}" = yes ]; then
	ok "require_network_passphrase refuses the public passphrase by its own message, even when EXPECTED is the public passphrase too"
else
	no "require_network_passphrase on the public passphrase (EXPECTED = public too): exit ${public_status}, said: ${public_result}"
fi

# 4. A node that answers nothing dies, naming the label and the URL.
noanswer_result=$(
	_sandbox_rpc_call() { printf ''; }
	require_network_passphrase "http://stub-noanswer/rpc" "${STUB_EXPECTED}" "${STUB_LABEL}" 2>&1
) && noanswer_status=0 || noanswer_status=$?

case "${noanswer_result}" in
*"http://stub-noanswer/rpc"*"${STUB_LABEL}"*) noanswer_named=yes ;;
*) noanswer_named=no ;;
esac

if [ "${noanswer_status}" -ne 0 ] && [ "${noanswer_named}" = yes ]; then
	ok "require_network_passphrase dies when the node answers nothing, naming the label and the URL"
else
	no "require_network_passphrase on no answer: exit ${noanswer_status}, said: ${noanswer_result}"
fi

printf '%d passed, %d failed\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
