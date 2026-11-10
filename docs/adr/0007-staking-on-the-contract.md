# ADR 0007 — Staked games on the escrow contract

**Status:** Proposed · **Date:** 2026-11-10 · **Follows:** ADR 0004 (staked games), ADR 0006 (user wallets), the escrow
contract in `eschess-contracts` (`docs/escrow-testnet.md`)

## Why

ADR 0004 holds stakes as entries in our own database. That works, but the money is only as safe as our database and our
word. The escrow contract holds the stakes on the network itself: the platform's key can only report who won, can only
pay the two players of the match, and anyone can return the stakes if the platform disappears. Every player now has an
account of their own that can sign a deposit (ADR 0006), and the backend can call the contract (proven on the testnet).
This ADR joins them.

This record is safe to publish: it describes money handling, not detection.

## What stays, what changes

**Stays:** every rule about *who gets the pot* (ADR 0004 §5): wait for fair play, a confirmed cheat pays the opponent,
draws and very short games return the stakes, a case in review holds the pot. That decision code is reused unchanged; it
produces an outcome, and only what happens next differs.

**Stays:** the ledger is the books (ADR 0006). A stake is still a ledger move from the player's wallet to `stake_escrow`
when the game is made, and the pot is still a ledger move from `stake_escrow` to the winner when it is paid. The ledger
and the check against the chain keep working.

**Changes:** the money is no longer only in the ledger. When two players are matched, their stakes are physically moved
from their own accounts into the contract, and when the game is settled the contract pays the winner. The books-against-
chain check adds the contract's USDC balance to what the network holds.

## Decision

### 1. A staked game now starts in a new state, `funding`

When two players are matched or one joins a staked game, the game is made as today, the ledger holds both stakes as today,
and the colours are chosen as today, but the game is **not yet active and no clock runs**. A worker job locks the stakes
on the network and only then makes the game `active` and starts White's clock. Players see "locking your stakes" for
roughly 10 to 30 seconds. This is a cost in feel; the alternative (start playing and fund in the background) lets a game
be played with a pot that was never secured, so one player could lose a game for nothing. A game that cannot be funded
within two minutes is cancelled and both ledger holds are released.

### 2. Funding, as steps the job can repeat safely

For each game the job reads the contract and does the next missing thing, so it can be stopped and restarted at any
point and never repeats a step the network already shows done:

1. the match does not exist: the platform's admin key creates it (players `A` = White, `B` = Black, the token, the stake, an
   expiry);
2. a player's account holds less than the stake: top it up from the platform wallet (see §4);
3. a player has not deposited: their key signs the deposit (the contract requires the player's own authorization);
4. both have deposited: the game becomes `active`.

The match id is the game's id, so the same game can never be created twice. Each transaction is recorded before it is
sent and waited for until it can no longer land, never rebuilt while it could (ADR 0001's rule).

### 3. Settling, as steps the job can repeat safely

When the existing settle logic (ADR 0004 §5) has decided the outcome and no fair-play case blocks it, the job:

1. asks the contract to settle with that outcome (`WinA`, `WinB` or `Draw`; a confiscation from a confirmed cheat is simply
   a win for the honest player). If the match is already settled on the network it moves on;
2. **only after the network shows the match settled**, posts the ledger move (escrow to winner, or each stake back) and marks
   the game settled.

So the ledger never says "paid" before the network has paid, and a payout that cannot be delivered (a player removed their
trustline) stays in the contract as a claimable balance rather than blocking the other player. The ledger move is posted
for the player either way, since the money is theirs; a claimable balance is collected with the player's own `claim`.

A game cancelled while funding, or a match the contract refunds because it expired, is returned to the players on the
network and in the ledger the same way.

### 4. Money that is still in the platform wallet

Players who deposited before they had an account of their own have money in the platform wallet, not in their own account.
Before a deposit into the contract, if their account holds less than the stake (and the XLM to pay the fee), the platform
pays the shortfall from its wallet to their account. This does not change the ledger (it is an internal move of the same
custody) and happens automatically; the books-against-chain check sees the total either way.

### 5. Limits that come from the contract

- A match can last at most 30 days. If a fair-play case keeps a pot waiting longer, the contract lets **anyone** return the
  stakes to both players. We create every match with the longest allowed expiry and treat a case still open near that date
  as an incident for a person. This is the price of "if the platform vanishes, players get their money back".
- The contract pays the winner the whole pot; **there is no platform fee** until the contract is changed (the owner's
  decision, and a change means another look from the auditors). The settle job refuses to run with a fee configured.
- The contract has not been audited. Staking on the contract stays on the testnet until it has been.

### 6. A switch, for the transition

A setting chooses where stakes live, `ledger` (today's behaviour, unchanged) or `contract` (this ADR). It lets the work be
built and tried on the testnet while the proven ledger path keeps working, and is removed once the contract path is the
only one. A game remembers which path it was made on, so changing the setting never strands a game halfway.

### 7. What the safety check adds

The books-against-chain check includes the contract's USDC balance in what the network holds, and treats a game that is
funding or settling as money in motion. A persistent difference halts new stakes and settlements as before.

## Risks, plainly

- **Players wait to start.** Locking stakes takes seconds; a slow or busy network makes it longer, and it can fail.
- **More ways to fail,** all of them recoverable by repeating a step, and none of them able to pay the wrong person: the
  contract only pays the two players recorded when the match was created.
- **Our key is the contract's admin.** It can report a wrong winner (not pay a stranger). Protecting it, and the players'
  keys, is the key-service requirement already in ADR 0006, and a multisig admin is a recommended step before real money.
- **A removed trustline** makes a payout claimable instead of paid; the player has to claim it.

## Decisions needed from the owner

1. **Wait for the stakes to be locked before the game starts** (recommended), or start at once and fund in the background?
2. **Top up players' accounts automatically from the platform wallet** when their money is still there (recommended), or ask
   those players to move it first?
3. **Accept the 30-day limit** on a pot waiting for a fair-play decision, as described in §5?
4. **Keep the ledger path as a switch** during the transition (recommended)?

## Build order (after acceptance)

1. The funding state and the job that locks the stakes (create, top up, deposit, activate), with cancel on failure.
2. Settling through the contract, then the ledger move once the network confirms.
3. The contract's balance in the books-against-chain check, and the switch between the two paths.
4. The end-to-end run on the testnet: two players stake, play, and are paid, with a confirmed cheat and a draw as well.
