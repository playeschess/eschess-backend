# eschess-backend

Rust backend for eSchess. The architecture is in [eschess-docs](https://github.com/playeschess/eschess-docs).

```
crates/game     the move loop: what a move does to a game (pure, no database)
crates/domain   pure rules: money, chess, game clock (rating to come); no I/O
crates/notification  sending email (log, in-memory for tests, or any SMTP provider)
crates/auth     password hashing, tokens, input validation (no database)
crates/ledger   double-entry postings and idempotency keys (pure, no database)
crates/fairplay fair play's public interface: the game record a scorer reads, the score it returns (no heuristics)
crates/stellar  the Stellar network: reading deposits, building and signing payments (no database)
crates/storage  PostgreSQL access: migrations, ledger, identity
bins/api        REST API
bins/worker     money jobs: credits Stellar deposits, sends withdrawals (kept apart from the API)
bins/migrate    applies database migrations
migrations/     forward-only SQL migrations
docs/adr/       architecture decision records (ADR 0001: withdrawals)
```

More crates and binaries (ledger, storage, game, realtime, worker ...) are added step by step, following the
build order in the architecture document.

## Everyday commands

```
make dev               # start Postgres in Docker (needs Docker)
make migrate           # create/update the database tables
make run               # start the API, then: curl localhost:8080/health
make worker            # start the worker next to it (deposits and withdrawals; needs STELLAR_HOT_ACCOUNT)
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
GET    /v1/me                   your profile, wallet balances and ratings
POST   /v1/games                open a game for someone to join (time control, colour, rated or casual; rated ones may carry a
                                "stake": "5", held from your USDC wallet now and returned if you cancel; off unless STAKES_USDC is set; the pot is paid by the worker's settle job, see below)
GET    /v1/games                the lobby (?scope=open), your games (?scope=mine), your past games (?scope=finished)
GET    /v1/games/{id}           a game: position, moves, clock, and your legal moves when it is your turn
POST   /v1/games/{id}/join      join an open game, which starts it
POST   /v1/games/{id}/cancel    withdraw a game nobody has joined
POST   /v1/games/{id}/moves     play a move: {"uci":"e2e4","client_move_id":"<uuid>"}; a retry with the same id is safe
POST   /v1/games/{id}/resign    resign
POST   /v1/games/{id}/claim-draw  claim a draw by threefold repetition or the fifty-move rule
POST   /v1/games/{id}/draw-offer         offer the opponent a draw
POST   /v1/games/{id}/draw-offer/accept  accept the offer made to you: the game is drawn by agreement
POST   /v1/games/{id}/draw-offer/decline decline the offer made to you, or withdraw your own
POST   /v1/matchmaking           ask for a game: {time_control, rated?}; paired at once if someone suitable waits
GET    /v1/matchmaking           still waiting, or the game that was found
DELETE /v1/matchmaking           leave the queue (not possible once a game was found)
GET    /v1/wallet/deposit-info   how to deposit USDC: an M... address, or the wallet address plus a memo (needs a confirmed email)
GET    /v1/wallet/deposits       your deposits, newest first
POST   /v1/wallet/withdrawals    send USDC out: {client_request_id, amount, destination, memo?}; the money is held at once
GET    /v1/wallet/withdrawals    your withdrawals, newest first
GET    /v1/wallet/withdrawals/{id}         one of them, with its transaction attempts
POST   /v1/wallet/withdrawals/{id}/cancel  withdraw the request, only before anything is signed (the money comes back)
GET    /v1/admin/withdrawals     approvers only: the approval queue (?status= for other states)
POST   /v1/admin/withdrawals/{id}/approve  approvers only, never their own
POST   /v1/admin/withdrawals/{id}/reject   approvers only: {reason}, shown to the player; the money comes back
GET    /v1/admin/fairplay/cases  reviewers only: the fair-play queue, oldest first (?status=cleared|confirmed for history)
GET    /v1/admin/fairplay/cases/{id}          reviewers only: the case's games, moves, clocks, lag, scores and evidence
POST   /v1/admin/fairplay/cases/{id}/clear    reviewers only: {reason, games}, games = the ids you looked at
POST   /v1/admin/fairplay/cases/{id}/confirm  reviewers only: {reason, games, sanction}; sanction = {kind:"warning"}, {kind:"suspension", days:1-365} or {kind:"closure"} (needs a second reviewer)
GET    /v1/admin/fairplay/closures                reviewers only: closures proposed and waiting for a second reviewer
POST   /v1/admin/fairplay/cases/{id}/closure/approve  reviewers only, not the proposer: {reason}; closes the account
POST   /v1/admin/fairplay/cases/{id}/closure/decline  reviewers only, not the proposer: {reason}; nothing happens to the player
GET    /v1/me/sanctions         your own sanctions: kind, dates, whether in force, appeal state (never the case)
POST   /v1/me/sanctions/{id}/appeal  {statement}: appeal a suspension or closure that restricts you, once (works when closed)
GET    /v1/admin/fairplay/appeals                      reviewers only: appeals waiting for a decision
POST   /v1/admin/fairplay/appeals/{id}/overturn|uphold reviewers only, with no part in the sanction: {reason}
GET    /v1/leaderboard          the best players in a speed class (?category=blitz&limit=50)
GET    /v1/ws                   WebSocket for live play (see below)
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
  the game. Over plain HTTP the server cannot measure a player's lag, so lag credit is zero there; over the
  WebSocket it is measured (below).
- **Open games.** A player may have at most 3 games waiting for an opponent (`409 too_many_open_games`).
- **Visibility.** Open games are public; a game in progress or finished is visible only to its two players (anyone else
  is told it does not exist).

## Live play over a WebSocket (bins/api)

`GET /v1/ws` upgrades to a WebSocket (protocol version 1). Every message is a JSON object with a `type`:

```
client -> HELLO         {protocol_version, access_token}   must be first, within 5 s; the token is never put in the URL
server -> WELCOME       {protocol_version, user_id, server_time_ms}
client -> SUBSCRIBE     {game_id, last_event_seq?}         watch a game you may see
server -> GAME_SNAPSHOT {game_id, event_seq, unchanged, game}
server -> GAME_UPDATE   {event_seq, game}                  whenever the game changes, whoever changed it (HTTP too)
client -> MOVE_REQUEST  {game_id, uci, client_move_id}
server -> MOVE_ACCEPTED {client_move_id, game_id, ply, uci, replayed, measured_lag_ms, credited_ms, charged_ms, event_seq}
      or MOVE_REJECTED  {client_move_id, game_id, code, message}
server -> CLOCK_SYNC    {game_id, event_seq, turn, clock}  every few seconds, to correct a drifting clock
server -> PING {id, server_time_ms}    client -> PONG {id}
server -> MATCH_FOUND   {game_id}                          matchmaking paired you (on every device you are connected with)
server -> ERROR         {code, message, game_id?}
client -> UNSUBSCRIBE   {game_id}
```

- **The server stays in charge.** A move request names only a game and a move; identity, position, turn and time are
  never taken from the client. A stranger who subscribes to someone else's game gets the same `not_found` as for a game
  that does not exist.
- **Gaps.** `event_seq` goes up by one on every change to a game. A client that sees a jump re-sends SUBSCRIBE and gets
  a fresh snapshot; sending its last `event_seq` makes the reply cheap (`unchanged: true`).
- **Several servers.** A change is announced through Postgres `LISTEN/NOTIFY` when it commits, so a player connected to
  any server hears about a move made through any other.
- **Lag.** The server times each PING/PONG round trip and takes half of the median as the player's lag. A move refunds
  that lag to the mover's clock, capped per move, from a quota that refills slowly, so a bad connection is not punished
  but cannot be abused.
- **Housekeeping.** Silent connections are closed (no PONG within 30 s), tokens are re-checked every few seconds so
  signing out closes the socket, and oversized messages, floods, repeated garbage and too many subscriptions are cut
  off. The limits are the `WS_*` settings in `.env.example` (all optional, with sensible defaults). A typo, a zero, or a pong
  timeout no longer than the ping interval stops the server from starting rather than running with a broken limit.

## Ratings (crates/domain, crates/storage)

Ratings are **Glicko-2** (`eschess_domain::rating`, checked against the worked example in Glickman's paper): a rating,
a deviation (how unsure it is) and a volatility. A player has one per speed class (bullet, blitz, rapid, classical,
from the time control), starting at 1500 with deviation 350.

- Every finished game updates both players inside the same transaction that ends it, whatever the way it ended
  (checkmate, resignation, timeout, draw), and `rating_history` (append-only) records the before and after, once per
  player per game.
- A game counts only if at least 4 plies were played (`MIN_PLIES_FOR_RATING`), so abandoning a game at once cannot be
  used to trade rating. Each game is its own rating period.
- A rating is `provisional` while its deviation is above 110. The leaderboard lists only established players.
- **Rated or casual.** The creator chooses when opening a game (`"rated": false` for casual; rated is the default). The
  choice is shown on the game, lobby included, and cannot change afterwards. Casual games never touch ratings.

## Matchmaking (crates/domain, crates/storage, bins/api)

Players who just want a game join a queue (`POST /v1/matchmaking`) instead of opening a game and waiting in the lobby.

- **Who meets whom** is the pure `eschess_domain::pairing`: only players who asked for the same time control and the
  same rated/casual choice can meet, and their ratings must be close. The acceptable gap starts at 100 points and grows by
  25 per second waited, up to 800, and both players' windows must allow it (the narrower decides), so nobody is thrown
  at a much stronger opponent the moment they arrive. The longest waiter is served first and gets the closest rating.
- **Pairing happens** when a player joins, and again every second in a background task, so people are matched as their
  windows widen. One database lock serialises pairing, so two servers never pair the same player twice.
- **A found game** starts immediately; the player who waited longer is recorded as its creator and colours are random.
  Both players' queue rows keep the game id for ten minutes, so a player who was not looking can still find it with
  `GET /v1/matchmaking`. Someone waiting more than two minutes is dropped. A player has at most one place in the queue;
  asking again replaces the earlier request.
- **Push.** A connected player is sent `MATCH_FOUND {game_id}` over the WebSocket the moment they are paired, on any
  server (it travels through Postgres `LISTEN/NOTIFY` like game updates, and only once the pairing commits). The push is
  a convenience, not the source of truth: a client that was offline, or whose connection dropped, finds the game with
  `GET /v1/matchmaking`.

## Draw offers (crates/game, crates/storage)

A player may offer a draw at any time in a game that is going, whoever's turn it is; the rules are the pure
`eschess_game::DrawOffers`. The game JSON carries `draw_offer` (the colour that has an offer waiting, or null), and
every change is pushed to both players as an ordinary `GAME_UPDATE`.

- The opponent accepts (the game ends as a draw, `end_reason: "agreement"`) or declines. The offerer may decline their own
  offer to withdraw it.
- **A move by the player the offer was made to counts as declining it.** The offerer may keep playing; their offer stays.
- **No pestering:** each side may offer once per position. After an offer is turned down, a new one needs a move to have
  been played. If both sides offer at once, that is an agreement.
- An agreed draw is rated like any other result (rated games of at least 4 plies).
- Offers are made over REST only; over the WebSocket there is no draw message, but both players see the offer arrive.

## Stellar deposits (crates/stellar, crates/storage, bins/api) — testnet only

Players deposit **USDC on the Stellar testnet** (Circle's issuer, `GBBD47IF…LA5`; a same-named asset from another issuer is a
different asset and is not accepted). Deposits are off unless `STELLAR_HOT_ACCOUNT` is set (see `.env.example`); the public
network is refused on purpose until it is a deliberate step.

- **One platform wallet, a deposit id per player.** All funds sit in a single Stellar account (the "hot wallet"). Each player
  has a deposit id, and `GET /v1/wallet/deposit-info` gives two equivalent ways to pay: an **`M...` muxed address** (the wallet
  with the id built in, SEP-23; nothing to forget) or the plain wallet address with the id as a **memo**. Either way the
  payment names the player. A text memo, the sender's address and the like are never trusted.
- **Watching the wallet.** The worker (`make worker`, not the API) reads the wallet's payments from Horizon (behind the `Chain` trait, so the
  provider can change and tests use a fake), after a saved cursor, and credits players through the ledger: debit the platform's
  `custody_stellar` account (which mirrors what the wallet holds), credit the player's wallet. Only one server polls at a
  time (a database lock). The cursor moves forward only after a whole page is dealt with.
- **Exactly once, whatever crashes.** Each payment is recorded under Horizon's operation id (the primary key), posted to the
  ledger under an idempotency key derived from the same id, then marked credited. Handling the same payment again, at once or
  after a crash between any two steps, never pays twice.
- **Never a guess.** A payment with no deposit id, an unknown or conflicting one, an asset we do not accept, or a record we
  could not read is kept as `needs_review` with the reason, not credited and not dropped.
- **Both ways are checked on the live testnet.** Muxed destinations are read from Horizon's `to_muxed_id` field; a real
  payment to an `M...` address with no memo was credited to the right player by its muxed id (see "Trying it on the
  testnet" below).
- **Not built yet:** the naira on/off ramp.

## Withdrawals (crates/storage, crates/stellar, bins/api, bins/worker) — testnet only

The design is [ADR 0001](docs/adr/0001-stellar-withdrawals.md). A player asks for USDC to be sent to a Stellar address
(`POST /v1/wallet/withdrawals`); the worker sends it.

- **Held at once.** The amount leaves the player's wallet for `platform_withdrawals_pending` in the same transaction that
  records the request, so it cannot be staked or withdrawn twice. A cancel, a rejection or a failed payment brings it
  back; a payment that lands moves it to `custody_stellar`. The database allows one hold and one outcome per withdrawal.
- **Checked before anything is held:** a confirmed email; at least `WITHDRAW_MIN_USDC`; a valid `G...` or `M...` address
  that is not the platform wallet (or an `M...` over it) nor USDC's issuer; a memo that fits; at most
  `WITHDRAW_DAILY_LIMIT_USDC` in 24 hours (checked inside the transaction, so requests at once cannot pass it together);
  and, on the network, that the account exists and can receive USDC (otherwise the payment would fail and cost a fee).
- **Approval.** Up to `WITHDRAW_AUTO_APPROVE_USDC` goes ahead at once. Above it, a person with the `withdrawal_approver`
  role approves or rejects (`/v1/admin/withdrawals`); nobody decides on their own withdrawal. Roles are granted by SQL:
  `INSERT INTO user_roles (user_id, role) VALUES ('<id>', 'withdrawal_approver');`
- **Retries are safe.** The app picks a `client_request_id`; the same request again answers with the first withdrawal.
- **Sending** (`bins/worker`, `make worker`). The worker needs the hot wallet's signing key in a file
  (`STELLAR_HOT_SECRET_FILE`, readable only by its owner; testnet only, refused for the public network). Without it,
  withdrawals are accepted and wait, approved. Each round it first settles what is in flight, then sends the next one:
  - a transaction is built with an expiry (`max_time`), signed, and **recorded before it is submitted**;
  - its outcome is decided by **looking its hash up**, never by the submission's answer: paid → confirmed and settled;
    failed on chain → refunded with the reason; not found → the *same* envelope is re-sent (safe: it can apply once);
  - only when the network has closed a ledger past `max_time` (plus `WITHDRAW_EXPIRY_MARGIN_SECS`) and the hash is still
    not there is the attempt marked expired, and only then is a new transaction built. A timeout is never a failure;
  - one payment is in flight at a time (the hot wallet has one sequence number), and a database lock lets several workers
    run without sending twice.
- **Not built yet:** network fees in the ledger (recorded per attempt for now; see the ADR), channel accounts for more
  than one payment at a time, and production key custody (a KMS or HSM behind the same `Signer` trait).

## Fair play (crates/fairplay, crates/storage, bins/worker)

The design is ADR 0002. The detection rules are secret and live in the private `eschess-fairplay` repository; this
repository has the plumbing, and a scorer that gives everyone 0.

- **What is recorded.** Every move stores, besides the move and the clock, how it arrived (`ws` or `http`) and the lag the
  server measured for it (WebSocket only).
- **Scoring.** The worker gives each finished game to the scorer once per scorer version (`fairplay_runs`), and stores a
  score from 0 to 100 per player with a summary and evidence for the reviewer (`fairplay_scores`). A new scorer version
  scores everything again; old scores are kept. Games in progress are never scored.
- **Never trusted, never stuck.** What the scorer returns is checked first: a score for someone who did not play, two for
  one player, or a number above 100 refuses the whole result. A game that cannot be scored (inconsistent moves, a scorer
  that panics, scores that break the rules) is recorded as `unscorable` with the reason, and the worker moves on.
- **Cases.** A score at or above `FAIRPLAY_CASE_THRESHOLD` opens a review case for the player, or adds the game to their
  open case (one open case per player). **A score is not a verdict**: only a reviewer decides, and in this baseline a
  decision is only recorded.
- **Review.** People with the `fairplay_reviewer` role (granted by SQL:
  `INSERT INTO user_roles (user_id, role) VALUES ('<id>', 'fairplay_reviewer')`) see the queue at
  `/v1/admin/fairplay/cases`, open a case to see every game's moves, clocks, lag, scores and evidence, and clear or
  confirm it with a written reason; a confirmation names a sanction (a warning, or a suspension of 1 to 365 days),
  recorded with it. A suspended player cannot create, join or queue for rated games until it ends (403
  `rated_play_suspended`); unrated play is unaffected. A closure takes effect only when a second reviewer approves it: then the player's open
  games are cancelled, live ones resigned, and every route except `/v1/me`, `/v1/wallet/*` and `/v1/auth/*` answers 403
  `account_closed` (a new route is closed to them by default). Their withdrawals always wait for a person to approve.
  A decision is final. The player can appeal a restricting suspension or closure once; a reviewer who had no part in it
  (not the confirmer, the closure's reviewer, or anyone in the case) upholds it or overturns it, which lifts the
  restriction at once and keeps the record. Games a closure ended stay ended.
  - Everyone else gets 404, so a player cannot learn they are under review. A reviewer never sees, let alone decides, a
    case about themselves **or about a game they played in** (the opponent has a stake); the database refuses it too.
  - The decision lists the games the reviewer looked at. If the worker added a game to the case since, it is refused
    (`409 case_changed`) until they look again: nobody clears evidence they never saw.
- The database refuses scores for non-players, changed or deleted scores, a second open case, reopening a decided case,
  adding a game to it, and deciding a case about yourself.

## Staked games (crates/storage, bins/worker) — testnet only

A rated game can carry a stake from the list in `STAKES_USDC`. The creator's stake is held (wallet to a `stake_escrow`
ledger account) when the game opens and the joiner's when it starts; cancelling a waiting game returns the creator's. A
player with an open fair-play case, a suspension or a closure cannot stake.

The worker's **settle job** pays the pot, once, per finished staked game. It runs only if `SETTLE_SCORER` names the
fair-play scorer to wait for, and it waits until that scorer has looked at the game and neither player has an open
case; if the scorer is not running, games stay unsettled rather than pay unreviewed. Then:

- a case **confirmed** against one player pays the other the whole pot, whatever the result; against both, each gets
  their stake back;
- a draw, or a game that ended before each side had made two moves, returns the stakes;
- otherwise the winner gets the pot less `STAKE_FEE_BPS` (0 by default; at most 20%).

A finished staked game's JSON carries `stake.settlement`: `settling` while the pot waits for fair play, `settled` once
paid or returned. Every settle round first checks that the escrow account equals the stakes of unsettled games (one for
a waiting game, two for a started one); if not it logs an `ESCROW MISMATCH` error and **settles nothing** until a person
looks.

`POST /v1/matchmaking` also takes a `stake`; only players asking for the same stake (and time control) meet. Nothing is
held while queued: both stakes are held in the transaction that pairs them, all or nothing. A player who can no longer
stake at that moment (spent the money, a case opened) leaves the queue, the other keeps waiting, and no game is made.

## Trying it on the testnet

A real deposit and withdrawal, end to end, with disposable testnet wallets. Nothing here touches the public network.

```
python3 -m venv ~/.eschess-testnet/venv && ~/.eschess-testnet/venv/bin/pip install 'stellar-sdk==12.2.0'
~/.eschess-testnet/venv/bin/python scripts/testnet_wallets.py
```

The script creates two wallets (or checks them on later runs), funds them with test XLM and adds a USDC trustline:
`hot`, the platform wallet, and `external`, standing in for a player's own wallet. The secret keys go to
`~/.eschess-testnet/` (outside the repo, readable only by you) and are never printed. Put the two lines it prints into
`.env`, then get test USDC into the external wallet from https://faucet.circle.com (Stellar). If the faucet does not
deliver, the testnet's exchange also sells Circle's test USDC for test XLM (a path payment from XLM to USDC).

Then `make run` and `make worker`, sign up and confirm a player, send USDC from the external wallet to the player's
`muxed_address` from `GET /v1/wallet/deposit-info`, and withdraw some back with `POST /v1/wallet/withdrawals`.

First run (2026-11-06): 5 USDC sent to a player's `M...` address with no memo was credited by its muxed id about a second
after it landed (tx `d936f27e…`); a 3 USDC withdrawal with a text memo was signed, recorded, sent and confirmed about 5
seconds after it was asked for (tx `1dd957a7…`, `max_time` two minutes after it was built). Afterwards the hot wallet held
2 USDC on chain, and the ledger agreed: custody 2, the player 2, pending 0, nothing in review.

Staked play, end to end (2026-11-09): `scripts/try-staked.py` (run with the testnet venv, against a fresh database, the API with
`STAKES_USDC` set, the public worker with `SETTLE_SCORER=baseline-1` and the private `fairplay-worker`) signs up two
players, sends each 3 real testnet USDC, and plays staked games by create-and-join and through the queue. Three games ran
this way: each was scored by `baseline-1` before its pot moved, each settled exactly once (checkmate winner paid the pot,
a game resigned before any move returned both stakes), the escrow ended at 0 and the players' balances summed to the 6 USDC
deposited. The queue-paired game came from the background sweep. Run it only on the testnet.
