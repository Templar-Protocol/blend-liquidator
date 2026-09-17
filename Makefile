# blend-liquidator

.PHONY: help build build-clean start stop restart logs logs-tail clean shell ps stats check \
	db-up db-down db-reset db-migrate sqlx-prepare \
	sandbox sandbox-up sandbox-fetch sandbox-deploy sandbox-test sandbox-down

.DEFAULT_GOAL := help

IMAGE := blend-liquidator
TAG := latest
COMPOSE := docker compose
ENV_FILE := .env

DATABASE_URL ?= postgres://liquidator:liquidator@127.0.0.1:55432/liquidator
export DATABASE_URL

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
	shellcheck --severity=error scripts/*.sh scripts/sandbox/*.sh .devcontainer/*.sh

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

sandbox-deploy: ## Deploy Blend v2 on the local network, with one borrower a price move from liquidation
	./scripts/sandbox/deploy.sh

sandbox-test: ## Run the end-to-end liquidation against the deployed sandbox (~5 min)
	cargo test --test liquidation_sandbox -- --ignored --nocapture

# The database sweep is here rather than in down.sh because it is not the
# network's: the test creates one `sandbox_<unix seconds>` database per
# run and drops it again when the run passes, so what this reclaims is
# what failed runs kept for inspection.
#
# target/sandbox/run-databases is the list, written by the test itself —
# appended when it creates the database and the line removed when it
# drops it. Nothing here enumerates: `sqlx database drop` does the
# dropping (sqlx-cli is in both CI and the dev container), it can only
# drop a name it is handed, nothing in sqlx-cli lists databases, and psql
# is in neither place. A database kept by a run from before that file
# existed is therefore dropped by hand, with the line printed below.
#
# A name that could not be dropped stays in the file: it is still on the
# server, and a sweep that forgot it would leave it there forever.
#
# The hint below prints the server with its userinfo stripped. DATABASE_URL
# carries a password — the committed local development one today, whatever
# an operator exported tomorrow — and a hint is not worth putting one in a
# terminal, a CI log or a pasted issue.
sandbox-down: ## Tear the sandbox down and drop the databases failed runs kept
	./scripts/sandbox/down.sh
	@set -u; \
	base="$${DATABASE_URL%%\?*}"; server="$${base%/*}"; \
	list=target/sandbox/run-databases; \
	if [ ! -s "$$list" ]; then \
		echo "no databases recorded in $$list — nothing to drop"; \
		echo "for one kept by a run from before that file: sqlx database drop -y -D $$(printf '%s' "$$server" | sed -E 's#//[^@]*@#//#')/sandbox_<stamp>"; \
		echo "  (that server has its userinfo stripped for this line — take the credentials from DATABASE_URL)"; \
	else \
		kept="$$list.kept"; : >"$$kept"; \
		while read -r name; do \
			[ -n "$$name" ] || continue; \
			echo "dropping database $$name"; \
			sqlx database drop -y --no-dotenv -D "$$server/$$name" \
				|| { echo "could not drop $$name — leaving it in $$list"; echo "$$name" >>"$$kept"; }; \
		done <"$$list"; \
		mv "$$kept" "$$list"; \
	fi

# up → fetch → deploy → test → down, with the teardown on the failure
# path too: a run that dies half way through still leaves a container and
# an armed key behind, and the next `sandbox-up` refuses to start until
# they are gone. SANDBOX_KEEP=1 skips it, for inspecting the network a
# run failed against.
#
# The failure path runs down.sh rather than the sandbox-down target,
# deliberately: the network goes, and this run's database stays, because
# it is what a failed run is diagnosed from. `make sandbox-down` is what
# reclaims it once it has been.
#
# sandbox-up is outside the teardown for its own reason: it refuses to
# start when a container is already there, and that refusal is usually a
# network somebody is still using (SANDBOX_KEEP=1 left it up). Tearing
# that down because this run could not start would destroy exactly what
# was being kept.
sandbox: ## up → fetch → deploy → test → down (SANDBOX_KEEP=1 leaves the sandbox up)
	@$(MAKE) sandbox-up || exit $$?; \
	status=0; \
	$(MAKE) sandbox-fetch && $(MAKE) sandbox-deploy && $(MAKE) sandbox-test \
		|| status=$$?; \
	if [ -n "$${SANDBOX_KEEP:-}" ]; then \
		echo 'SANDBOX_KEEP is set — leaving the sandbox up; make sandbox-down tears it down'; \
	elif [ "$$status" -eq 0 ]; then \
		$(MAKE) sandbox-down; \
	else \
		echo 'the run failed — tearing the network down and keeping its database;'; \
		echo 'make sandbox-down drops it once you are done with it'; \
		./scripts/sandbox/down.sh || true; \
	fi; \
	exit $$status

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
