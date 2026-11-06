# The local database from docker-compose.yml. Override by setting DATABASE_URL (or creating a .env file).
-include .env
DATABASE_URL ?= postgres://eschess:eschess@127.0.0.1:5440/eschess
export DATABASE_URL

.PHONY: run worker try test test-integration fmt lint check dev dev-down migrate psql

run:   ## start the API on 127.0.0.1:8080
	cargo run -p eschess-api

worker:  ## start the worker: credits Stellar deposits and sends withdrawals (run it next to `make run`)
	cargo run -p eschess-worker

try:   ## start the API and try every endpoint end to end (starts Postgres too)
	./scripts/try-api.sh

test:  ## unit tests: no database or other services needed
	cargo test --workspace

test-integration:  ## tests that need Postgres (run `make dev` first)
	cargo test --workspace -- --ignored

fmt:   ## format the code
	cargo fmt --all

lint:  ## the same checks CI runs
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

check: lint test

dev:   ## start local services (Postgres) and wait until they are ready
	@# If a stopped container will not restart (this happens in some Codespaces after a restart), recreate it.
	@# The local database holds only throwaway development data, so nothing of value is lost.
	docker compose up -d --wait || { echo "Postgres would not start; recreating it (local dev data is discarded)"; docker compose down -v && docker compose up -d --wait; }

dev-down:  ## stop local services (add -v to also delete the data)
	docker compose down

migrate:  ## apply database migrations to the local database
	cargo run -q -p eschess-migrate

psql:  ## open a SQL prompt on the local database
	docker compose exec postgres psql -U eschess -d eschess
