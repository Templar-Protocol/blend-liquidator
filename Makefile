# blend-liquidator

.PHONY: help build build-clean start stop restart logs logs-tail clean shell ps stats check

.DEFAULT_GOAL := help

IMAGE := blend-liquidator
TAG := latest
COMPOSE := docker compose
ENV_FILE := .env

help: ## Show available commands
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

check: ## Run everything CI runs (fmt, clippy, test, doc, invariants, shellcheck)
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
