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
	@echo "  e2e     every end-to-end suite, including real Samba and NFS servers"
	@echo "  e2e-local  the search, read-only and desktop-client suites (no servers)"
	@echo "  ui-test    the desktop-client suite alone"
	@echo "  package build .deb, .rpm, .pkg.tar.zst and .tar.gz packages into dist/"
	@echo "  run     run the daemon (indexing this host) and a browser GUI in containers"
	@echo "  stop    stop them again"

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

.PHONY: e2e e2e-local ui-test
# Every end-to-end suite: search features, read-only guarantees, the desktop
# client and real Samba and NFS servers (E2E_FILES=1000000 for a big share).
e2e:
	$(COMPOSE) --profile test build samba nfs
	$(COMPOSE) --profile test run --rm e2e
	$(COMPOSE) --profile test down

# The suites that need no servers.
e2e-local: image
	$(DEV) bash tests/e2e/run.sh features readonly ui

# The desktop client, driven through the accessibility bus on Xvfb.
ui-test: image
	$(DEV) bash tests/e2e/run.sh ui

.PHONY: package deb rpm tarball arch
# Packages for Debian/Ubuntu, Fedora/openSUSE, Arch and a generic tarball, in dist/.
package: image arch
	$(COMPOSE) --profile package build package
	$(COMPOSE) --profile package run --rm package all

arch:
	$(COMPOSE) --profile package run --rm package-arch

deb rpm tarball: image
	$(COMPOSE) --profile package build package
	$(COMPOSE) --profile package run --rm package $@

.PHONY: run stop
# Try EverythingOS without installing it (Linux hosts): the daemon runs in a
# container but indexes the host; the GUI is served on http://localhost:8080.
run:
	$(COMPOSE) --profile run up -d --build
	@echo "GUI: http://localhost:8080   CLI: docker compose exec gui evos <query>"

stop:
	$(COMPOSE) --profile run down
