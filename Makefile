# Every target runs inside Docker so the same commands work from any host.
COMPOSE ?= docker compose
DEV     := $(COMPOSE) run --rm dev

.PHONY: help image build test fmt shell

help:
	@echo "Targets:"
	@echo "  image   build the development image"
	@echo "  build   cargo build --workspace"
	@echo "  test    cargo test --workspace"
	@echo "  fmt     format the workspace"
	@echo "  shell   interactive shell in the dev container"

image:
	$(COMPOSE) build dev

build:
	$(DEV) cargo build --workspace

test:
	$(DEV) cargo test --workspace

fmt:
	$(DEV) cargo fmt --all

shell:
	$(DEV) bash
