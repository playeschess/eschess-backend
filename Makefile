.PHONY: run test fmt lint check

run:   ## start the API on 127.0.0.1:8080
	cargo run -p eschess-api

test:  ## run every test
	cargo test --workspace

fmt:   ## format the code
	cargo fmt --all

lint:  ## the same checks CI runs
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

check: lint test
