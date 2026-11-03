# eschess-backend

Rust backend for eSchess. The architecture is in [eschess-docs](https://github.com/playeschess/eschess-docs).

```
crates/domain   pure rules: money, clock, chess, rating (no I/O)
crates/ledger   double-entry postings and idempotency keys (pure, no database)
crates/storage  PostgreSQL access: migrations and the ledger repository
bins/api        REST API
bins/migrate    applies database migrations
migrations/     forward-only SQL migrations
```

More crates and binaries (ledger, storage, game, realtime, worker ...) are added step by step, following the
build order in the architecture document.

## Everyday commands

```
make dev               # start Postgres in Docker (needs Docker)
make migrate           # create/update the database tables
make run               # start the API, then: curl localhost:8080/health
make test              # unit tests, no database needed
make test-integration  # tests that use the local Postgres
make lint              # formatting and clippy, exactly what CI checks
make psql              # a SQL prompt on the local database
make dev-down          # stop Postgres (its data is kept)
```

From an empty machine: `make dev && make migrate && make test-integration`.
