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
# `sqlx database drop` does the dropping — sqlx-cli is in both CI and the
# dev container, which psql is not — but it can only drop a database it
# is given the name of, and nothing in sqlx-cli lists them. psql, when
# there is one, lists; the compose Postgres has its own when the host
# does not; and with neither, the sweep says what it could not do rather
# than pretending it did.
sandbox-down: ## Tear the sandbox down and drop the sandbox_* databases failed runs kept
	./scripts/sandbox/down.sh
	@set -u; \
	base="$${DATABASE_URL%%\?*}"; \
	list="SELECT datname FROM pg_database WHERE datname ~ '^sandbox_[0-9]+$$'"; \
	if command -v psql >/dev/null 2>&1; then \
		names=$$(psql "$$base" -tAc "$$list") || names=""; \
	elif names=$$($(COMPOSE) exec -T postgres psql -U "$${POSTGRES_USER:-liquidator}" \
		-d "$${POSTGRES_DB:-liquidator}" -tAc "$$list" 2>/dev/null); then \
		:; \
	else \
		echo "no psql on PATH and no running compose postgres — cannot list the sandbox databases;"; \
		echo "drop one by name with: sqlx database drop -y -D $$base/sandbox_<stamp>"; \
		names=""; \
	fi; \
	if [ -z "$$names" ]; then \
		echo "no sandbox_* databases to drop"; \
	else \
		for name in $$names; do \
			echo "dropping database $$name"; \
			sqlx database drop -y --no-dotenv -D "$${base%/*}/$$name" \
				|| echo "could not drop $$name"; \
		done; \
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
