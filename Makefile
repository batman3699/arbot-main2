.PHONY: build test fmt lint panic-check placeholder-endpoint-check fork-dry-run check-contract-size

build:
	cargo build
	forge build

test:
	cargo test
	forge test

fmt:
	cargo fmt
	forge fmt

lint:
	cargo clippy --all-targets -- -D warnings
	./scripts/ci/check_placeholder_endpoints.sh

panic-check:
	./scripts/ci/no_runtime_panics.sh

fork-dry-run:
	./scripts/fork/run_integration_dry_run.sh

check-contract-size:
	./scripts/ci/check_executor_size.sh

placeholder-endpoint-check:
	./scripts/ci/check_placeholder_endpoints.sh
