# The four merge gates; see rust-testing.md. Keep in sync with .cargo/config.toml.

# Run all four gates in order, stopping at the first failure.
ci: fmt-check lint test deny

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# nextest, falling back to cargo test, plus doc-tests (nextest skips them).
test:
    if command -v cargo-nextest >/dev/null; then cargo nextest run --workspace --no-tests=pass; else cargo test --workspace --lib --bins --tests; fi
    cargo test --workspace --doc

deny:
    cargo deny check
