# Every target runs inside Docker so the same commands work from any host.
COMPOSE ?= docker compose
DEV     := $(COMPOSE) run --rm dev

.PHONY: help image build test check fmt shell

help:
	@echo "Targets:"
	@echo "  image   build the development image"
	@echo "  build   cargo build --workspace"
	@echo "  test    cargo test --workspace"
	@echo "  check   fmt --check + clippy -D warnings + tests"
	@echo "  fmt     format the workspace"
	@echo "  shell   interactive shell in the dev container"
	@echo "  e2e     end-to-end tests against Samba and NFS servers"
	@echo "  package build .deb, .rpm and .tar.gz packages into dist/"

image:
	$(COMPOSE) build dev

build:
	$(DEV) cargo build --workspace

test:
	$(DEV) cargo test --workspace

check:
	$(DEV) ./scripts/check.sh

fmt:
	$(DEV) cargo fmt --all

shell:
	$(DEV) bash

.PHONY: e2e
# End-to-end tests against real Samba and NFS servers (E2E_FILES=1000000 for a big share).
e2e:
	$(COMPOSE) --profile test build samba nfs
	$(COMPOSE) --profile test run --rm e2e
	$(COMPOSE) --profile test down

.PHONY: package deb rpm tarball
# Packages for Debian/Ubuntu, Fedora/openSUSE and a generic tarball, in dist/.
package: image
	$(COMPOSE) --profile package build package
	$(COMPOSE) --profile package run --rm package all

deb rpm tarball: image
	$(COMPOSE) --profile package build package
	$(COMPOSE) --profile package run --rm package $@
