.PHONY: build check fmt clippy lint test test-js validate check-naming release ci deploy-serve setup clean

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

## Check artifact filenames conform to <slug>-<8chars-uuid>.yml convention
check-naming:
	./company/scripts/check-artifact-naming.sh .

## Build release binaries
release:
	cargo build --release --workspace

## Full CI pipeline
ci: lint test test-js validate check-naming

## Promote MCP server binaries into target/serve/ (atomic deploy, RFC 18011bfc).
## Explicit act ONLY — never invoked by ci/test/build. This is the deliberate
## "update the served server now" step; the controlled restart it triggers is
## absorbed by the proxy buffer.
deploy-serve:
	./company/scripts/deploy-serve.sh

## Clean build artifacts
clean:
	cargo clean
	rm -f company/data/orchestrator.db company/data/orchestrator.db-shm company/data/orchestrator.db-wal

## Super clean: clean + wipe all generated artifacts, preserving system config.
##
## Wipes (generated artifacts):
##   - projects/*/  ........ ALL project-scoped kinds (task-requests, design-docs,
##                           implementation-plans, review-reports, diagnostic-reports,
##                           config), covered recursively by the single rm -rf.
##   - company/lessons/ .... lesson-learned
##   - company/rfcs/ ....... rfc
##   - company/roadmaps/ ... roadmap
##   - company/agent-messages/ ... agent-message (TRANSITIONAL, see below)
##
## Preserves (never touched): company/personas, company/config, company/schemas,
##   company/plugins, company/scripts, company/data (the DB is wiped separately by
##   the `clean` target this depends on).
##
## Keep in sync with spec.rules.file_placement in company/config/shared-rules.yml.
## When a new GLOBAL kind is added there, add its company/<folder>/ rm line here.
## Project kinds need NO new line: rm -rf projects/*/ already covers them.
##
## PILLAR D (RFC cdbfee72): after clean-company the orchestrator index
## (company/data/orchestrator.db, already wiped by `clean`) is incoherent until the
## next boot, which rebuilds it deterministically from the remaining YAML (logically
## empty on the generated-artifact side). This is the intended design, not a bug.
clean-company: clean
	rm -rf projects/*/
	rm -f company/lessons/*.yml
	rm -f company/rfcs/*.yml
	rm -f company/roadmaps/*.yml
	rm -f company/agent-messages/*.yml   # TRANSITIONAL: removed by RFC 3d9f0b0c (agent-message kind retirement)
	@echo "All artifacts wiped. Personas/config/schemas preserved. Run 'make setup' to rebuild."

## Full setup: git hooks + release build + CI + serve promotion (run once after clone)
setup: release ci deploy-serve
	git config core.hooksPath .githooks
	@echo "Setup complete: git hooks configured, release binaries built, CI passed, served binaries promoted to target/serve/."
