# eschess-backend

Rust backend for eSchess. The architecture is in [eschess-docs](https://github.com/playeschess/eschess-docs).

```
crates/game     the move loop: what a move does to a game (pure, no database)
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
POST   /v1/games                open a game for someone to join (time control, colour)
GET    /v1/games                the lobby (?scope=open), your games (?scope=mine), your past games (?scope=finished)
GET    /v1/games/{id}           a game: position, moves, clock, and your legal moves when it is your turn
POST   /v1/games/{id}/join      join an open game, which starts it
POST   /v1/games/{id}/cancel    withdraw a game nobody has joined
POST   /v1/games/{id}/moves     play a move: {"uci":"e2e4","client_move_id":"<uuid>"}; a retry with the same id is safe
POST   /v1/games/{id}/resign    resign
POST   /v1/games/{id}/claim-draw  claim a draw by threefold repetition or the fifty-move rule
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

## Playing a game (crates/game, bins/api)

One player opens a game (`POST /v1/games`) and another joins it; the server flips a coin for the colours if the creator
asked for random. From then on the **server alone decides** what is legal, whose turn it is and how much time is left:
a request says only which move is wanted, and who is asking comes from the access token. All game state lives in
Postgres (`games`, and an append-only `game_moves`), so any server can carry on any game, and a player who reconnects
just asks for `GET /v1/games?scope=mine`.

- **Retries are safe.** Each move carries a `client_move_id` chosen by the app. If the same id arrives again (a dropped
  connection), the original outcome is returned and nothing is played twice, even after the opponent has replied.
- **One move at a time per game.** Every change locks the game's row first, so two simultaneous requests take turns.
- **Time.** The server's time comes from the database. A flag that has fallen is noticed by the next request that touches
  the game. Lag credit is zero for now: over plain HTTP the server cannot measure a player's lag, so that comes with
  WebSockets.
- **Visibility.** Open games are public; a game in progress or finished is visible only to its two players (anyone else
  is told it does not exist).
