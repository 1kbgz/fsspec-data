.PHONY: requirements develop build
requirements:  ## install required dev dependencies
	rustup component add rustfmt
	rustup component add clippy
	cargo install --locked cargo-nextest
	cargo install --locked cargo-llvm-cov

develop: requirements  ## install required dev dependencies

build:  ## build release
<<<<<<< before updating
	cargo build --release --all-features --package fsspec_data

.PHONY: lint lints fix format
lint:  ## run Clippy for linting, rustfmt for autoformat checks
	cargo clippy --workspace --all-targets --all-features -- -D warnings
=======
	cargo build --release --workspace --all-features

.PHONY: lint lints fix format
lint:  ## run Clippy for linting, rustfmt for autoformat checks
	cargo clippy --workspace --all-features
>>>>>>> after updating
	cargo fmt --all -- --check

# alias
lints: lint

fix:  ## fix code with rustfmt
	cargo fmt --all

# alias
format: fix

.PHONY: check checks
check:
	cargo check --workspace --all-features
<<<<<<< before updating

=======
>>>>>>> after updating
# alias
checks: check

.PHONY: test tests
test:  ## run the tests
	cargo llvm-cov nextest --workspace --cobertura --output-path coverage.xml

# alias
tests: test

<<<<<<< before updating
coverage:  ## collect Rust test coverage
	cargo llvm-cov nextest --workspace --all-features --cobertura --output-path coverage.xml
=======
coverage:
	cargo llvm-cov nextest --workspace --lcov --output-path coverage
>>>>>>> after updating

.PHONY: dist publish
dist:  ## validate crate packaging
	cargo publish --workspace --dry-run --allow-dirty

publish:  ## publish crates
	cargo publish --workspace

# Thanks to Francoise at marmelab.com for this
.DEFAULT_GOAL := help
help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

print-%:
	@echo '$*=$($*)'
