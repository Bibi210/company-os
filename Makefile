.PHONY: build check fmt clippy lint test test-js validate release ci setup clean

## Default target
all: setup

## Build all crates
build:
	cargo build --workspace

## Check (fast, no codegen)
check:
	cargo check --workspace --all-targets

## Format check
fmt:
	cargo fmt --all -- --check

## Lint
clippy:
	cargo clippy --workspace --all-features --all-targets -- -D warnings

## Format + lint
lint: fmt clippy

## Run tests
test:
	cargo test --workspace

## Run JS tests
test-js:
	node --test company/plugins/tests/*.test.mjs

## Validate all YAML artifacts against schemas
validate:
	cargo run -p companyos-yaml-validator -- --batch company/

## Build release binaries
release:
	cargo build --release --workspace

## Full CI pipeline
ci: lint test test-js validate

## Clean build artifacts
clean:
	cargo clean
	rm -f company/data/orchestrator.db company/data/orchestrator.db-shm company/data/orchestrator.db-wal

## Super clean: clean + wipe all generated artifacts (projects, lessons, rfcs, diagnostics)
## Preserves: personas, config, schemas, plugins
clean-company: clean
	rm -rf projects/*/
	rm -f company/lessons/*.yml
	rm -f company/rfcs/*.yml
	rm -f company/diagnostics/*.yml
	rm -f company/design-docs/*.yml
	@echo "All artifacts wiped. Personas/config/schemas preserved. Run 'make setup' to rebuild."

## Full setup: git hooks + release build + CI (run once after clone)
setup: release ci
	git config core.hooksPath .githooks
	@echo "Setup complete: git hooks configured, release binaries built, CI passed."
