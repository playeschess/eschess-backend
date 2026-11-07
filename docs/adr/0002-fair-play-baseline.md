# ADR 0002 — Fair play baseline

**Status:** Proposed · **Date:** 2026-11-07 · **Architecture refs:** eschess-docs §2 (fair-play truth), §3 (why
`eschess-fairplay` is separate), §15 (phase 2: "detection scoring real games"), §17 (OD-10)

## Context

The build order is *fair play before money*: before anyone stakes real money on a game, the platform must be able to
notice games that look like they were not played by the person playing them, and act on it fairly. Deposits and
withdrawals now work (ADR 0001); staked games are next on the money side, so this comes first.

Three rules from the architecture document shape everything here:

1. **The heuristics are secret.** Detection thresholds, weights and models live in `eschess-fairplay`, a repository that
   is private permanently. Published thresholds are an evasion guide.
2. **A score is not a verdict.** Fair-play truth is the Fair Play service *with a human decision for anything high
   impact*. No automated score alone bans a player, voids a result or takes money.
3. **The backend talks to fair play through a narrow interface**: `score(game_events) -> RiskScore`.

**`eschess-backend` is a public repository** (so are the other `playeschess` repositories, although the architecture
document lists them as private). That makes rule 1 the deciding constraint: nothing in this repository may reveal what
is measured with which weight, or where a line is drawn. This ADR is written to be safe to publish. It names the
plumbing, not the detection.

## Decision

### 1. What lives where

| Public `eschess-backend` | Private `eschess-fairplay` |
|---|---|
| The interface: `GameRecord` in, `RiskScore` out, and a `Scorer` trait | Every heuristic, threshold, weight and model |
| Capturing the per-move data a scorer needs | The scorer implementation |
| Storing scores and the review queue | Its own worker binary that runs the scorer |
| The review API for people with the `fairplay_reviewer` role | Its tests, which include known-cheating fixtures |
| What happens after a human decides | |
| A `NoopScorer` that scores everything 0 | |

**Dependency direction.** The architecture document has `eschess-fairplay` consumed as a private crate *by* the backend.
That does not work with a public backend. Cargo resolves even optional git dependencies when it builds the lock file, so
every contributor and every public CI run would need access to the private repository, and the dependency itself would
be visible in `Cargo.toml`. So the direction is reversed: **`eschess-fairplay` depends on the public backend crates**
(pinned to a git revision) and builds its own worker, which reads finished games and writes scores through
`eschess-storage`. The public backend never depends on private code and builds and runs with fair play absent; its own
worker can run the `NoopScorer`. This is a deviation from the architecture document, recorded here as §17 requires.

### 2. The interface (public, in a new `crates/fairplay`)

```rust
pub struct GameRecord {                 // everything the server knows about one finished game
    pub game_id, white_id, black_id, time control, rated, result, end_reason,
    pub moves: Vec<MoveRecord>,         // in order
}
pub struct MoveRecord {
    pub ply, uci, fen_before, fen_after,
    pub mover: Color,
    pub think_ms,                       // time charged for the move
    pub lag_credited_ms, lag_measured_ms (Option), remaining_ms_after,
}
pub struct RiskScore {
    pub player_id,
    pub score: u8,                      // 0..=100: how much this game deserves a person's look
    pub scorer: String,                 // name and version, so scores from different versions are never mixed
    pub summary: String,                // for the reviewer: why, in words
    pub evidence: serde_json::Value,    // for the reviewer: opaque to the backend
}
pub trait Scorer: Send + Sync {
    fn name(&self) -> &str;             // e.g. "baseline-3"
    fn score(&self, game: &GameRecord) -> Vec<RiskScore>;   // one per player who should be looked at
}
```

`score` is synchronous and pure. A scorer that needs an engine runs it inside its own worker before or around this call;
the backend never runs an engine.

The backend does not trust what a scorer returns, since it cannot see the code. Before anything is stored, the public
`check` refuses the whole batch if any score is for someone who did not play, is a second score for one player, is above
100, names a different scorer, or carries an oversized summary or evidence.

### 3. Data capture

`game_moves` already holds the move, the position after it, the time charged, the lag credited, the clock left, and when
it was played. Two additions, both nullable (expand step):

- `lag_measured_ms`: the round-trip lag the server measured for the move over the WebSocket (`NULL` over plain HTTP). Today
  only the refund is stored, which hides what the player's connection was really doing.
- `transport`: `'ws'` or `'http'`.

Client-side signals (the app losing focus, a second device) are **not** collected in this baseline. They need a
protocol change, a privacy review and the mobile app. A later ADR covers them.

### 4. Storage

```
fairplay_scores   one row per (game, player, scorer). Append-only. Score, summary, evidence, when.
fairplay_cases    the review queue: one open case per player at most. A score at or above the case threshold opens a
                  case or adds the game to the player's open case. A case is open, then cleared or confirmed, with
                  who decided, when, and a written reason. Append-only history.
```

The **case threshold** (which score opens a case) is configuration of the private worker, not of the backend. The
backend only stores what it is given.

Scoring is idempotent: the unique key `(game_id, player_id, scorer)` means re-running a scorer on a game changes nothing,
and a new scorer version scores everything again without touching the old scores.

### 5. Review (public API)

A new role, `fairplay_reviewer`, granted by SQL like `withdrawal_approver`. Reviewers see open cases oldest first, with
each game's moves, clocks and the scorer's summary and evidence. They clear or confirm, with a reason. Nobody reviews a
case about themselves, or about a game they played in.

The routes answer 404 to everyone else, like the withdrawal admin routes.

A decision names the games the reviewer looked at, and is refused if the case has gained a game since (the worker can
add one at any moment). Without this, a reviewer could clear evidence they never saw. Both rules (not involved, games
seen) are checked with the case locked, and the database refuses a decision by anyone who played in one of the case's
games.

### 6. What a decision does

In this baseline, **confirming a case records the decision and nothing else.** No automated consequence exists yet. The
consequences are product and policy decisions, and they need their own ADR before staked games ship:

- account restriction (no rated, no staked play) and how it is appealed;
- what happens to ratings won in confirmed games;
- for staked games: withholding payout during review (the hold window, OD-13) and confiscation (OD-14).

The architecture document's warning applies: a hold is a correct outcome, not a latency failure (F-45).

### 7. Running it

- The public worker gets a `fairplay` job: find finished games without a score from the configured scorer, build their
  `GameRecord`, score, store. Batch, idempotent, behind a database lock like the other jobs.
- In the public repository the scorer is `NoopScorer`, so the pipeline (capture, records, storage, queue, review API) is
  built and tested in the open with fake scores.
- The private repository builds the same job with the real scorer, and that worker runs in production.

## Consequences

- Detection can change without a backend release, and none of it is ever public.
- The backend can be fully tested without the private repository, which means the private scorer is tested against the
  public interface, not inside the backend's CI. Its own CI must pin and test against the backend revision it targets.
- Two new tables, one new crate, two nullable columns, a new role and a worker job. No change to how games are played.
- Until the private scorer exists, the queue is empty. The exit gate for phase 2 ("detection scoring real games") needs
  the private repository.

## Needs a decision outside this ADR

- **Creating `playeschess/eschess-fairplay` as a private repository**, and who has access.
- **OD-10, build or buy.** A bought service would replace the private worker's scorer with an integration; the public
  side of this ADR stays the same either way.
- **Repository visibility.** The architecture document lists every repository as private; all are public today. For
  `eschess-contracts` ("private until audited") and for this one, that may not be intended.

## Build order

1. **Interface and capture**: `crates/fairplay` (types, `Scorer`, `NoopScorer`), the two `game_moves` columns, and building
   a `GameRecord` from storage.
2. **Storage and the worker job**: `fairplay_scores`, `fairplay_cases`, scoring finished games idempotently.
3. **Review API**: the reviewer role, the queue, clear and confirm.
4. **Private repository**: the baseline scorer and its worker (outside this repository).
