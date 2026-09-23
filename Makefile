# blend-liquidator

.PHONY: help build build-clean start stop restart logs logs-tail clean shell ps stats check \
	db-up db-down db-reset db-migrate sqlx-prepare \
	sandbox sandbox-up sandbox-fetch sandbox-deploy sandbox-test sandbox-down \
	testnet-deploy testnet-crash testnet-run testnet-run-armed

.DEFAULT_GOAL := help

IMAGE := blend-liquidator
TAG := latest
COMPOSE := docker compose
ENV_FILE := .env

DATABASE_URL ?= postgres://liquidator:liquidator@127.0.0.1:55432/liquidator
export DATABASE_URL

# The sandbox tier's five scenarios. The same five names are written in
# scripts/sandbox/deploy.sh, tests/sandbox_harness/mod.rs's SCENARIOS,
# .github/workflows/sandbox.yml's matrix and tests/liquidation_sandbox.rs's
# #[ignore]d test fns, and scripts/check-repo-invariants.sh fails unless
# all five name the same set. SANDBOX_SCENARIO picks the one
# `sandbox-deploy` and `sandbox-test` act on; SANDBOX_SCENARIOS is what
# `sandbox` loops over when no single SANDBOX_SCENARIO is given.
SANDBOX_SCENARIO ?= liquidation
SANDBOX_SCENARIOS ?= liquidation check_config dry_run unwind_repay restart_adopt

# testnet-crash's own oracle price, in the oracle's 7-decimal units. Empty
# by default, which leaves crash.sh to use its own default (750000,
# $0.075); PRICE=<n> overrides it.
PRICE ?=

help: ## Show available commands
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

db-up: ## Start Postgres and wait for it
	$(COMPOSE) up -d postgres
	@user=$${POSTGRES_USER:-liquidator}; port=$${POSTGRES_PORT:-55432}; \
	for _ in $$(seq 1 60); do \
		if $(COMPOSE) exec -T postgres pg_isready -U "$$user" >/dev/null 2>&1; then \
			echo "postgres ready on 127.0.0.1:$$port"; exit 0; \
		fi; \
		sleep 1; \
	done; \
	echo "postgres did not become ready in 60s"; $(COMPOSE) logs postgres; exit 1

db-down: ## Stop Postgres, keeping its data
	$(COMPOSE) stop postgres

db-reset: ## Take the stack down and delete the database volume
	$(COMPOSE) down -v --remove-orphans

db-migrate: ## Apply migrations to the local database
	sqlx migrate run

sqlx-prepare: ## Regenerate the committed offline query metadata (.sqlx)
	cargo sqlx prepare -- --lib --bins

check: ## Run everything CI runs (needs `make db-up` first)
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	cargo test --lib --bins
	cargo sqlx prepare --check -- --lib --bins
	RUSTDOCFLAGS='-D warnings' cargo doc --no-deps
	./scripts/check-repo-invariants.sh
	shellcheck --severity=error scripts/*.sh scripts/sandbox/*.sh scripts/testnet/*.sh .devcontainer/*.sh

# ── The sandbox integration tier ────────────────────
#
# A throwaway Stellar network in Docker, Blend v2 deployed on it, and the
# real binary run against it **armed**. Nothing here runs in CI's PR gate:
# `make sandbox` takes minutes and starts containers, so it is a nightly
# workflow (.github/workflows/sandbox.yml) and a thing you run by hand.
#
# Each step is its own target because each fails for its own reason — a
# pinned image that moved, a wasm whose hash no longer matches, a
# contract that changed — and because a developer debugging one wants to
# repeat it without paying for the others.

sandbox-up: ## Start the pinned local Stellar network in Docker
	./scripts/sandbox/up.sh

sandbox-fetch: ## Download and verify the pinned Blend v2 wasm artefacts
	./scripts/sandbox/fetch-artifacts.sh

sandbox-deploy: ## Deploy Blend v2 on the local network for SANDBOX_SCENARIO (default liquidation)
	SANDBOX_SCENARIO=$(SANDBOX_SCENARIO) ./scripts/sandbox/deploy.sh

# `--list` first, and exactly one match required, because libtest exits 0
# having run nothing: a name that matches no test fn reports `0 passed; N
# filtered out`, and so would a matching fn that had lost its #[ignore]
# under `--ignored`. `--include-ignored` runs the one match whether or not
# it is still ignored — check-repo-invariants.sh is what holds every
# scenario fn #[ignore]d — so the only way through this target is the named
# scenario actually running. The list is captured before it is counted, and
# counted by `grep -c`, which reads to the end of its input: no consumer
# here exits early on a producer still writing. It needs no network, so a
# mistyped SANDBOX_SCENARIO is refused before anything is asked of one.
sandbox-test: ## Run SANDBOX_SCENARIO's end-to-end test against the deployed sandbox (default liquidation; refuses a name that is not exactly one test fn)
	@scenario='$(SANDBOX_SCENARIO)'; \
	listed=$$(cargo test --test liquidation_sandbox -- --include-ignored --exact --list "$$scenario") \
		|| { status=$$?; echo "sandbox-test: could not list tests/liquidation_sandbox.rs's tests"; exit $$status; }; \
	count=$$(printf '%s\n' "$$listed" | grep -c ': test$$' || true); \
	if [ "$$count" != 1 ]; then \
		found=$$(printf '%s\n' "$$listed" | sed -n 's/: test$$//p' | paste -sd' ' -); \
		echo "sandbox-test: SANDBOX_SCENARIO='$$scenario' must name exactly one test fn in tests/liquidation_sandbox.rs, but --list found $$count: $${found:-none}"; \
		exit 1; \
	fi; \
	cargo test --test liquidation_sandbox -- --include-ignored --exact --nocapture "$$scenario"

# The database sweep is here rather than in down.sh because it is not the
# network's: each scenario creates its own databases per run —
# `sandbox_<unix seconds>`, or `sandbox_<unix seconds>_1` and `_2` for
# restart_adopt's two bots — and drops them again when the run passes, so
# what this reclaims is what failed runs kept for inspection.
#
# target/sandbox/run-databases is the list, written by the test itself —
# appended when it creates the database and the line removed when it
# drops it. Nothing here enumerates: `sqlx database drop` does the
# dropping, it can only drop a name it is handed, nothing in sqlx-cli
# lists databases, and psql is in neither CI nor the dev container. A
# database kept by a run from before that file existed is therefore
# dropped by hand, with the line printed below.
#
# sqlx-cli is installed by .github/workflows/ci.yml and by
# .devcontainer/post-create.sh, both from the SQLX_CLI_VERSION pinned in
# scripts/sandbox/versions.env — but neither is a guarantee (post-create's
# step only warns on failure, and this target is run outside the dev
# container too), so the sweep checks for the tool once and names it,
# rather than reporting "could not drop" for every entry and never saying
# why.
#
# A name that could not be dropped stays in the file: it is still on the
# server, and a sweep that forgot it would leave it there forever.
#
# Neither the sweep nor the hint puts a URL on a command line. DATABASE_URL
# carries a password — the committed local development one today, whatever
# an operator exported tomorrow — and argv is world-readable through `ps`
# and /proc/<pid>/cmdline, which is the convention CLAUDE.md states for
# every secret this repo handles. sqlx-cli reads DATABASE_URL from the
# environment, so the per-command assignment below is the whole fix; the
# printed hint is in that same form so an operator following it does not
# reintroduce what this target avoids, and prints the server with its
# userinfo stripped besides.
sandbox-down: ## Tear the sandbox down and drop the databases failed runs kept
	./scripts/sandbox/down.sh
	@set -u; \
	base="$${DATABASE_URL%%\?*}"; server="$${base%/*}"; \
	list=target/sandbox/run-databases; \
	if [ ! -s "$$list" ]; then \
		echo "no databases recorded in $$list — nothing to drop"; \
		echo "for one kept by a run from before that file: DATABASE_URL=$$(printf '%s' "$$server" | sed -E 's#//[^@]*@#//#')/sandbox_<stamp> sqlx database drop -y --no-dotenv"; \
		echo "  (that server has its userinfo stripped for this line — take the credentials from DATABASE_URL)"; \
	else \
		command -v sqlx >/dev/null 2>&1 \
			|| { echo "sqlx-cli is not installed, so the databases in $$list cannot be dropped"; echo "  install it with: cargo install sqlx-cli --version $$(grep '^SQLX_CLI_VERSION=' scripts/sandbox/versions.env | cut -d= -f2-) --no-default-features --features postgres,rustls --locked"; exit 1; }; \
		kept="$$list.kept"; : >"$$kept"; \
		while read -r name; do \
			[ -n "$$name" ] || continue; \
			echo "dropping database $$name"; \
			DATABASE_URL="$$server/$$name" sqlx database drop -y --no-dotenv \
				|| { echo "could not drop $$name — leaving it in $$list"; echo "$$name" >>"$$kept"; }; \
		done <"$$list"; \
		mv "$$kept" "$$list"; \
	fi

# fetch → (up → deploy → test → down) per scenario, with the teardown on
# the failure path too: a run that dies half way through still leaves a
# container and an armed key behind, and the next `sandbox-up` refuses to
# start until they are gone. Fetching once, ahead of the loop, is enough —
# the wasm artefacts are keyed by content hash, not by scenario, and
# deploy.sh re-verifies them itself regardless.
#
# SANDBOX_SCENARIOS is the list each of its own network, database and log
# file is made for, defaulting to all five this tier has; SANDBOX_SCENARIO=x
# runs the one x instead. The loop stops at the first scenario that fails
# and names it, because a developer running this by hand wants that
# failure's state: its kept database and its logs, sandbox.log above all,
# which the next scenario's deploy would rewrite. The nightly workflow
# makes the opposite choice for its own reason — each matrix job runs one
# scenario on its own runner, fail-fast: false, because there every
# scenario's result stands on its own.
#
# SANDBOX_KEEP=1 skips the scenario's teardown, for inspecting the network
# it ran against, and so needs exactly one scenario selected: a container
# already up is what the next `sandbox-up` refuses to start on, so a kept
# network can only ever be one scenario's. With more than one selected it
# refuses before anything starts, naming SANDBOX_SCENARIO=x as the way to
# pick one.
#
# An empty selection (`SANDBOX_SCENARIOS=` with no SANDBOX_SCENARIO) is
# refused the same way, before anything starts: the loop would otherwise
# run no scenario and report every one passed — the vacuous pass
# sandbox-test's own match check exists to rule out, one level up.
#
# The failure path runs down.sh rather than the sandbox-down target,
# deliberately: the network goes, and the run's database stays, because it
# is what a failed run is diagnosed from. `make sandbox-down` is what
# reclaims it once it has been.
#
# sandbox-up is outside every teardown for its own reason: it refuses to
# start when a container is already there, and that refusal is usually a
# network somebody is still using (SANDBOX_KEEP=1 left it up). Tearing that
# down because this run could not start would destroy exactly what was
# being kept.
#
# A sandbox-down that fails after a green test fails the whole target: it
# removes the container and sweeps the run databases, so its failure is a
# container still holding port 8000 and databases still on the server —
# precisely what the next run refuses on, and reporting success would hide
# it until then.
sandbox: ## fetch, then up → deploy → test → down for each of SANDBOX_SCENARIOS (default all five; SANDBOX_SCENARIO=x runs just x; SANDBOX_KEEP=1, with one scenario selected, leaves its sandbox up)
	@scenarios="$${SANDBOX_SCENARIO:-$(SANDBOX_SCENARIOS)}"; \
	set -- $$scenarios; \
	if [ "$$#" -eq 0 ]; then \
		echo "sandbox: no scenarios selected — set SANDBOX_SCENARIO=x, or leave SANDBOX_SCENARIOS at its default"; \
		exit 2; \
	fi; \
	if [ -n "$${SANDBOX_KEEP:-}" ] && [ "$$#" -ne 1 ]; then \
		echo "sandbox: SANDBOX_KEEP=1 keeps one scenario's network up, but $$# are selected ($$scenarios) — pick one with SANDBOX_SCENARIO=x"; \
		exit 2; \
	fi; \
	$(MAKE) sandbox-fetch || exit $$?; \
	for scenario in $$scenarios; do \
		echo "=== sandbox: $$scenario ==="; \
		$(MAKE) sandbox-up || exit $$?; \
		status=0; \
		$(MAKE) SANDBOX_SCENARIO=$$scenario sandbox-deploy \
			&& $(MAKE) SANDBOX_SCENARIO=$$scenario sandbox-test \
			|| status=$$?; \
		if [ -n "$${SANDBOX_KEEP:-}" ]; then \
			echo "SANDBOX_KEEP is set — leaving $$scenario's sandbox up; make sandbox-down tears it down"; \
			[ "$$status" -eq 0 ] || echo "sandbox: $$scenario failed"; \
			exit $$status; \
		elif [ "$$status" -eq 0 ]; then \
			$(MAKE) sandbox-down || exit $$?; \
		else \
			echo 'the run failed — tearing the network down and keeping its database;'; \
			echo 'make sandbox-down drops it once you are done with it'; \
			./scripts/sandbox/down.sh || true; \
			echo "sandbox: $$scenario failed"; \
			exit $$status; \
		fi; \
	done; \
	echo 'sandbox: every scenario passed'

# ── The testnet soak tier ───────────────────────────
#
# scripts/testnet/*.sh, a sibling of the sandbox tier above rather than a
# mode of it: the same protocol stood up on public Stellar testnet, with
# friendbot's testnet XLM as the capital, for the design spec's soak (see
# docs/testnet-soak.md). Nothing here runs in CI — it is a thing you run by
# hand, against a network this repo does not control and cannot reset or
# tear down; testnet.env, once deploy.sh writes it, is what every other
# target here reads.

testnet-deploy: ## Stand Blend v2 up on testnet for the soak's armed stage (writes target/testnet/testnet.env; refuses if it already exists)
	./scripts/testnet/deploy.sh

testnet-crash: ## Move the testnet soak's oracle price (default $0.075; PRICE=<n> in the oracle's 7-decimal units for another)
	./scripts/testnet/crash.sh $(PRICE)

testnet-run: ## Run the bot against testnet in dry run (the observe stage's own pool; no key, nothing ever submitted)
	./scripts/testnet/run-bot.sh

testnet-run-armed: ## Run the bot against testnet ARMED (DRY_RUN=false, our own deployed pool) — the only target that runs the bot armed on testnet
	./scripts/testnet/run-bot.sh --armed

build: ## Build Docker image
	docker build -t $(IMAGE):$(TAG) -f Dockerfile .

build-clean: ## Build without cache
	docker build --no-cache -t $(IMAGE):$(TAG) -f Dockerfile .

start: ## Start in dry-run mode (default; set DRY_RUN=false in .env to go live)
	$(COMPOSE) --env-file $(ENV_FILE) up -d

stop: ## Stop all containers
	$(COMPOSE) down

restart: stop start ## Restart

logs: ## Show logs
	$(COMPOSE) logs

logs-tail: ## Follow logs
	$(COMPOSE) logs -f

ps: ## Show container status
	$(COMPOSE) ps

stats: ## Show resource usage
	docker stats --no-stream

shell: ## Open a shell in the running container
	$(COMPOSE) exec liquidator /bin/bash

clean: ## Remove containers and the built image
	$(COMPOSE) down --rmi local --volumes --remove-orphans
