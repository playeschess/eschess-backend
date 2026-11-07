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

**Decided by the owner (OD-14, 2026-11-07): a confirmed cheater's stake goes to the fair player, automatically.**

When a reviewer confirms a case, every staked game in it is settled by the ledger in the same step, with no second
approval. A score alone never moves money; only a person's confirmation does.

- If the cheater won, the opponent receives both stakes: their own back and the cheater's.
- If the cheater lost or drew, the opponent already holds what the result gave them and nothing further moves.
- A case covering several staked games settles each one separately, and a game is never settled twice.

All of this is ordinary double-entry ledger postings, with the game and the case recorded as the reason; there is no new
way to move money. The platform keeps no part of a confiscated stake.

*Known cost:* if an appeal later overturns a confirmation, money the opponent has already withdrawn cannot be taken
back from them. The platform bears that reversal. This is why confirmation of a staked game needs a reviewer's full
attention, and why §5 gives every sanction an appeal.

### 5. Appeals

Every sanction can be appealed once, by the player, with a written statement. An appeal is decided by a reviewer who did
not decide the case, using the same rules as ADR 0002 (not involved in any of the games). Overturning a sanction lifts
restrictions and returns held money; ratings are restored.

### 6. Audit

Decisions, sanctions, appeals and every money movement they cause are append-only and record who, when and why. Nothing
is deleted.

## Needs a decision from the owner

- Hold maximum age and who the escalation goes to (§2).
- Whether sanction 3 (closure) may be applied by one reviewer, or needs a second (proposed: a second).
- Legal review of terms of service wording, the appeal process, and holding a user's funds.

## Build order (after acceptance)

1. Case state to the payout path: "open case" blocks sending withdrawals and creating staked games.
2. Sanctions and their effects on account status; settling staked games on confirmation.
3. Appeals.
4. Notifications to the player (hold, sanction, outcome).
