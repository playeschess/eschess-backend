# eschess-backend

Rust backend for eSchess. The architecture is in [eschess-docs](https://github.com/playeschess/eschess-docs).

```
crates/domain   pure rules: money, chess, game clock (rating to come); no I/O
crates/notification  sending email (log, in-memory for tests, or any SMTP provider)
crates/auth     password hashing, tokens, input validation (no database)
crates/ledger   double-entry postings and idempotency keys (pure, no database)
crates/storage  PostgreSQL access: migrations, ledger, identity
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
make try               # start everything and try every endpoint end to end, reporting what passed
make test              # unit tests, no database needed
make test-integration  # tests that use the local Postgres
make lint              # formatting and clippy, exactly what CI checks
make psql              # a SQL prompt on the local database
make dev-down          # stop Postgres (its data is kept)
```

From an empty machine: `make dev && make migrate && make test-integration`.

## API so far

```
POST   /v1/auth/signup          create a player and their wallet, and sign in (email, username, password)
POST   /v1/auth/login           sign in
POST   /v1/auth/email/verify    confirm an email address with the emailed token
POST   /v1/auth/email/resend    send the confirmation email again (signed in, once a minute)
POST   /v1/auth/password/forgot ask for a password-reset email (always answers the same)
POST   /v1/auth/password/reset  choose a new password with the emailed token; signs out every device
POST   /v1/auth/refresh         trade a refresh token for new tokens (each refresh token works once)
POST   /v1/auth/logout          end this session
GET    /v1/auth/sessions        list your signed-in devices
DELETE /v1/auth/sessions/{id}   sign one of your devices out
GET    /v1/me                   your profile and wallet balances
GET    /health
```

A signup with an email or username that is already taken is refused with a clear error (`email_taken` or
`username_taken`, HTTP 409), so the player can fix it.

**Email.** Signing up sends a confirmation email; the player can use the app straight away but is marked
`email_verified: false` until they confirm, and endpoints that involve money are meant to call
`AuthUser::require_verified`. "Forgot password" answers the same whether or not the address has an account. A reset
link works once, expires after 30 minutes, signs out every device, and lifts any lockout. Tokens are stored only as
hashes. Set `SMTP_URL` and `MAIL_FROM` to use a real provider (see `.env.example`).

Try it locally without a mail server: set `DEV_PRINT_EMAIL_TOKENS=true` in `.env`, run `make dev && make migrate && make run`,
sign up, and read the token from the server log.

## Chess rules (crates/domain)

`chess::Game` plays moves in UCI notation (`e2e4`, `e7e8q`), refuses illegal ones, and reports how a game ended by the
rules alone: checkmate, stalemate, insufficient material, fivefold repetition, the 75-move rule, plus the draws a player
may claim (threefold repetition, the 50-move rule). Resignation, flag fall and agreed draws belong to the game service.

Correctness is checked three ways: the published "perft" counts for six famous tricky positions (about 11 million
positions), thousands of random positions compared one by one against the independent `python-chess` library
(`crates/domain/tests/data/`, regenerate with the script there), and ordinary unit tests for each rule.

## Game clock (crates/domain)

`clock::Clock` is a Fischer clock (base time plus an increment per move) written as pure arithmetic on millisecond
timestamps the caller supplies, so it can be rebuilt from stored values after a restart (`Clock::state` and
`Clock::from_state`). **Lag credit:** the server measures a player's network lag and refunds part of it, but never more
than the time actually spent, never more than a per-move cap, and only from a small quota that refills slowly, so a
player who claims lag on every move gains almost nothing. A timer should use `flagged_at`, which waits out the refund
a move still in flight could be owed. The numbers are in `LagPolicy::default()` and are starting values to be checked
against the product requirements (PRD section 9, F-17). `Game::flag_result` applies the FIDE rule that running out of
time is a draw, not a loss, when the opponent could never checkmate.
