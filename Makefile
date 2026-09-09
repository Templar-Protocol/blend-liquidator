# blend-liquidator

.PHONY: help build build-clean start stop restart logs logs-tail clean shell ps stats check \
	db-up db-down db-reset db-migrate sqlx-prepare

.DEFAULT_GOAL := help

IMAGE := blend-liquidator
TAG := latest
COMPOSE := docker compose
ENV_FILE := .env

DATABASE_URL ?= postgres://liquidator:liquidator@127.0.0.1:55432/liquidator
export DATABASE_URL

help: ## Show available commands
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

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
	RUSTDOCFLAGS='-D warnings' cargo doc --no-deps
	./scripts/check-repo-invariants.sh
	shellcheck --severity=error scripts/*.sh .devcontainer/*.sh

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
