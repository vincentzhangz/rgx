SHELL := /usr/bin/env bash

.PHONY: build test nextest clippy fmt fmt-check coverage bench install release release-dry-run clean

build:
	cargo build

test:
	cargo test --workspace

nextest:
	cargo nextest run --workspace

clippy:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

coverage:
	./scripts/coverage.sh

bench:
	./scripts/bench.sh

install:
	cargo install --path crates/rgx

release-dry-run:
	cargo release 0.1.0 --workspace --dry-run

release:
	cargo release 0.1.0 --workspace

clean:
	cargo clean
	rm -rf target/coverage target/coverage.lcov