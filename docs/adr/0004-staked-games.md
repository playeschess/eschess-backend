# ADR 0004 — Staked games

**Status:** Proposed · **Date:** 2026-11-07 · **Follows:** ADR 0001 (money in and out), ADR 0002 and 0003 (fair play
and what its decisions do) · **Architecture refs:** §5.4 (money path), §14 (failure modes), OD-13

## Context

Money goes in (deposits) and out (withdrawals), games are played, and fair play can notice, hold, sanction and
reverse. Staked games join them: two players each put money on a game and the winner takes the pot. This is the first
place the platform moves player money *because of* a game result, so the rules are: money is held before play and
settled once, from the ledger, never from a number a client sends; and **a pot is not paid while fair play might still
change who deserves it.**

This ADR is written to be safe to publish. It describes money handling, not detection.

## Decision

### 1. What a staked game is

A staked game is a **rated** game with a stake: one amount in one asset (USDC), the same for both players, chosen from
a fixed list rather than typed freely. It is created, joined or matched exactly like a rated game with one more field.
Unrated games cannot be staked.

*Values below are proposals for the owner to set (§9):* stakes of 1, 5, 10 and 25 USDC; USDC only.

### 2. Who may stake

A player may create, join or queue for a staked game only if all of these hold, checked inside the same transaction
that takes the stake:

- their email is confirmed (as for any money path);
- they have no open fair-play case, no suspension, and no closure (`fairplay_restrictions`, ADR 0003);
- their spendable balance covers the stake. The database already refuses an overdraft.

A player with an open case cannot start a new staked game, but a game already started is not cancelled; it settles by
the rule in §5.

### 3. Holding the stake

Taking a stake is one ledger posting: debit the player's `user_wallet`, credit a `stake_escrow` account, with the game
as the reference and an idempotency key made from the game and the player. A creator's stake is held when the game is
created and returned if it is cancelled or expires unjoined; a joiner's is held in the same transaction that starts the
game, so a game never starts with only one stake in. If the second stake cannot be held (no funds, restricted since),
the join fails and nothing moves.

Escrow holds money, never a player's spendable balance, so a staked player cannot withdraw it mid-game.

### 4. Results that do not pay a winner

- **Draw:** each player's stake is returned.
- **Aborted** (a game that never reached its second move for each side, or one that the server ended without a result):
  stakes returned.
- **No fee** is taken on either.

### 5. Settlement waits for fair play

A finished staked game is **unsettled** until both of these hold:

1. the fair-play worker has finished with the game (scored it, or set it aside as unscorable), and
2. neither player has an open fair-play case.

Then it settles by result. This is the hold OD-13 asks for, made precise: the pot does not move while a person might
still be asked to look. A game whose scoring is delayed stays unsettled; that is a correct outcome, not a failure
(F-45). The worker settles in small batches and tells the players the state (`settling`) so they know why.

Settlement is one posting per game, keyed by the game, so a retry or two workers cannot pay twice:

- **Decisive result:** the winner receives the pot minus the fee (§6).
- **Draw or abort:** each stake returns.
- **Fair-play decision** (ADR 0003 §4, OD-14): if a reviewer **confirmed** a case about one player that includes this
  game, the pot goes to the opponent, whatever the result, as the owner decided. If both players were confirmed for
  the same game, both stakes return and nobody wins. A case that was **cleared** changes nothing.

A confirmation that arrives after a game has already settled (a case opened by a *later* game) does not reach back:
it covers the games in its case, and a game in a case is not settled while the case is open. Settled money is final,
the same property withdrawals have.

### 6. The fee

The platform takes a fee from the winner's pot on a decisive result only, as a ledger credit to a platform revenue
account in the same posting. *Proposed: 0 at launch; the mechanism is built so the value is configuration.*

### 7. Safety properties

- Every movement is a balanced ledger posting; there is no other way money moves.
- A game's stakes are held exactly once and released exactly once. The database refuses a second settlement of a game.
- A stake is never taken from, or paid to, an account with an open case, a suspension, or a closure, except the
  settlement of a game already started.
- Escrow for a game always equals its stakes: a check that sums escrow per game and compares to open games runs in the
  worker's loop and raises an alarm on any difference.
- Closed accounts keep their wallet (ADR 0003): a closed player's escrowed stake in an unsettled game follows §5.

### 8. What it needs

- Migration: `games.stake_minor` (nullable, `NULL` = unstaked) and `games.settled_at`; a `stake_escrow` ledger account
  kind; a unique settlement record per game.
- `eschess-storage`: staked create/join/queue with the eligibility checks and holds; cancel and expiry releases.
- Worker job `settle`: finds finished staked games that are unsettled, applies §5, posts once.
- API and WebSocket: a `stake` field on create and matchmaking, `settling`/`settled` on the game.
- Tests that kill the worker between steps and show nothing is lost or paid twice.

## Consequences

- A cheater who wins cannot withdraw a pot while their game is under review; the opponent is paid when the case is
  confirmed. Ordinary games settle within a worker round or two of finishing.
- Settlement depends on the fair-play worker running. If it is down, staked games stay unsettled rather than pay
  unreviewed. This is deliberate and means the worker is now a production dependency of staked play.
- Rated and unrated play are unchanged.

## Needs a decision outside this ADR

- **Stake sizes and assets** (§1) and **the fee** (§6).
- **Draw and abort rules** (§4), as proposed above.
- **Legal and licensing review.** Money staked on the outcome of a game can be gambling or a game of skill with
  contest rules depending on where players live. This must be settled, with allowed regions, age checks and terms,
  **before any real-money stakes are enabled**. Everything here can be built and tested on testnet in the meantime.

## Build order

1. Schema, escrow account, and staked create/join/queue with holds and releases (testnet only).
2. The `settle` job and the unsettled state, with fair-play gating.
3. Fair-play settlement (confirmed case pays the opponent).
4. API/WebSocket fields, the reconciliation check, and the failure-injection tests.
