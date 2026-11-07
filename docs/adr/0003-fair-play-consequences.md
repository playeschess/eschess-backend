# ADR 0003 — What a fair-play decision does

**Status:** Proposed · **Date:** 2026-11-07 · **Follows:** ADR 0002 (§6 defers this) · **Architecture refs:** OD-13
(payout hold), OD-14 (confiscation), F-45

## Context

ADR 0002 built the pipeline: scores open cases, a reviewer clears or confirms, and **confirming records the decision and
nothing else**. That is safe while no money is staked. It must change before staked games ship, because then a confirmed
case is the only thing standing between a cheater and other players' money.

This ADR is written to be safe to publish: it describes consequences, never detection.

Principles carried over: a score is never a verdict; every consequence follows a human decision; a hold is a correct
outcome, not a latency failure (F-45); and a wrong consequence must be reversible where money allows.

## Proposed decision

### 1. Two separate things: holds and sanctions

- **A hold** is temporary and automatic-adjacent: it pauses money for a player with an *open* case. It is not a finding.
- **A sanction** follows a *confirmed* case only, and always by a human decision.

Keeping them apart means opening a case never punishes anyone, and the payout path has one question to ask: "does this
player have an open case?"

### 2. Hold (while a case is open)

For a player with an open case: withdrawals stay in `approved` and are not sent (the status machine from ADR 0001 has
room for this), and new staked games are refused. Free and unrated play continue. The hold lifts when the case is
cleared. The player is told only that their account is under routine review, never why.

A hold must have a **maximum age** (proposed: 7 days). A case still open after that is escalated to the reviewers' lead,
because an indefinite silent hold is itself harm. *Value to be set by the owner (OD-13).*

### 3. Sanctions after a confirmed case

Proposed, in increasing severity, decided per case by the reviewer with a reason:

1. **Warning**: recorded, no restriction.
2. **Rated and staked play suspended** for a stated period.
3. **Account closed**; balance withdrawable after the hold window and a final check.

Ratings from confirmed games are voided for the *offender's* opponents' benefit only: opponents keep what they gained,
and the offender's rating is reset. Results are not rewritten.

### 4. Staked games and money

If a confirmed case involves staked games, the opponent's stake is returned in full and the offender's stake is **not**
paid out. Whether the offender's stake is returned, kept as platform revenue, or shared with opponents is OD-14 and is a
business and legal decision, not an engineering one. Until it is decided, the offender's stake stays held in a ledger
account of its own, and nothing is paid out.

All money movements here use the existing double-entry ledger; there is no new way to move money.

### 5. Appeals

Every sanction can be appealed once, by the player, with a written statement. An appeal is decided by a reviewer who did
not decide the case, using the same rules as ADR 0002 (not involved in any of the games). Overturning a sanction lifts
restrictions and returns held money; ratings are restored.

### 6. Audit

Decisions, sanctions, appeals and every money movement they cause are append-only and record who, when and why. Nothing
is deleted.

## Needs a decision from the owner

- **OD-14**: what happens to a confirmed cheater's stake (§4).
- Hold maximum age and who the escalation goes to (§2).
- Whether sanction 3 (closure) may be applied by one reviewer, or needs a second (proposed: a second).
- Legal review of terms of service wording, the appeal process, and holding a user's funds.

## Build order (after acceptance)

1. Case state to the payout path: "open case" blocks sending withdrawals and creating staked games.
2. Sanctions and their effects on account status; the offender-stake ledger account.
3. Appeals.
4. Notifications to the player (hold, sanction, outcome).
