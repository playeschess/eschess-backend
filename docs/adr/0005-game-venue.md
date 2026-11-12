# ADR 0005 — Where the games are played: our server or Lichess

**Status:** Direction chosen by the owner on 2026-11-12 (Option B, sending players to Lichess), **conditional on Lichess's terms** · **Date:** 2026-11-09 (updated 2026-11-12) · **Follows:** ADR 0004

## Why this is open

The product's difference is the money: stakes held in escrow, settled after a fair-play check, with deposits and
withdrawals in USDC. The chess game itself is not the difference. So it is fair to ask whether we should run the games
at all, or let an existing platform (Lichess is the obvious candidate) run them while we only hold the money and pay out
on the result.

This record sets the two options side by side, says what each one needs, and lists what is **not known yet**, so that the
people looking into Lichess know what to find out. It decides nothing.

## What exists today (our server)

Built, tested, and run once end to end on the Stellar testnet: the chess rules, clocks with lag compensation, the game
and WebSocket server, ratings, matchmaking, the fair-play pipeline and reviewer tools, sanctions and appeals, and staked
games with escrow, settlement and a fair-play gate. **Not built: the player-facing app** (a board, a lobby, a wallet
screen). Today it is an API.

## Option A — keep our server

We own the rules, the clocks, the result and the fair-play data. A pot is paid on a result the platform itself produced and
watched move by move. Anything we want to change (time controls, abort rules, what a fair-play hold does) is ours to
change. Costs: we keep building and running it, we need our own anti-cheat (the baseline is deliberately simple), and we
need an app. Open-source board components exist, and their licences need checking before use.

## Option B — Lichess runs the games, we escrow

A player links their Lichess account (OAuth). Two players who staked agree a game; it is played on Lichess; we read the
result from Lichess's API and settle.

What it would save: the game server, clocks, rules, ratings and most of the app's play screen, and a mature anti-cheat.

What it would need or cost, from what is known and what is only suspected:

| Question | Known / unknown |
|---|---|
| Do Lichess's terms allow money to depend on games played there? | **Unknown.** A first look found nothing either way. Must be read in full and, if unclear, asked. This alone can end Option B. |
| Time controls | Their Board API is limited to rapid, classical and correspondence (blitz only for direct challenges). So no bullet, and little blitz. |
| Who decides a result we cannot see: disconnects, aborts, a disputed game | Theirs, not ours. We would pay on whatever their API says. |
| A player marked for cheating after we have paid | Possible. A payout cannot be taken back from the winner, so the platform would carry that loss, or settlement would have to wait. |
| Identity | One person per Lichess account is not enforced by us. Account sharing and selling would need handling. |
| Collusion (losing on purpose to a friend) | Exists in both options; with Lichess we would see less of the game to detect it. |
| Dependence | Their outage, rate limits, API change, or a decision to ban our integration stops us with money already held in escrow. |
| Self-hosting their server instead | Their server software is open source, but its licence and what it would oblige us to publish are **unchecked**, and it is a large system to run. Not assessed. |

## What changes in our code if B were chosen

Escrow and settlement stay. The game server, clock, move and matchmaking code would be bypassed for staked games; a game
would be identified by a Lichess game id; settlement would read its result from their API instead of our own. The
fair-play gate would wait on a decision we do not control. The ADR 0004 safety properties (held once, released once,
escrow equals unsettled stakes) would still hold, but "the result is true" would become a trust in a third party.

## What to find out before deciding

1. The full terms of service, API terms and fair-play page: anything about money, wagering, commercial use, or banning
   integrations.
2. Whether Lichess will say, in writing, that an escrow service built on their API for money games is acceptable.
3. Whether their Board API's time-control limits fit the product.
4. How a finished game's result, abort and disconnect states look in their API, and how soon a cheating mark appears.

## Direction chosen (owner, 2026-11-12)

The owner chose Option B in its simplest form: the platform handles the money, and the game is played on lichess.org.

1. The player signs up on our site and links their Lichess account (Lichess OAuth proves they own it).
2. The player enters an amount and presses Stake. The stake is locked in the escrow contract, as today.
3. When two players are matched, we create the game on Lichess for those two accounts (an open challenge restricted
   to the two usernames, with a fixed time control and rated or not, so players cannot change the terms) and send both
   players to it. Whether Lichess allows exactly this call is to be confirmed.
4. They play on Lichess. Nothing is embedded for playing; an embed can show a finished game for replay or spectators.
5. We read the finished game from Lichess's API and settle through the contract with the usual rules.

Rules to design (not decided here):
- **No-shows and aborts:** no game started within a few minutes cancels the match and returns both stakes.
- **Only the right game counts:** a result is accepted only for the game we created, between the two linked accounts.
- **Time controls:** limited to what Lichess allows for challenges; rapid and classical are safe, bullet is not offered.
- **Fair play:** we cannot score moves live. After the game we can fetch it and run our timing scorer on it, and check
  whether Lichess has flagged either account. What their data really contains must be checked before relying on it.
  A mark appearing after payout cannot be undone, so settlement keeps waiting for the fair-play check as today.
- **Collusion** (losing on purpose) exists in both options and needs its own rules.

Built so far (see `plan.md`): the venue and result plumbing, account linking, creating and watching the game, and importing the finished game's moves and clocks for fair play. The
setting `game_venue` (`own` or `lichess`, in `platform_settings`) says where new staked games are played; it stays `own`
until the real-Lichess trial and the scorer check listed in `plan.md` are done. Decisions taken while building: games are **casual** on Lichess (a rated game would move the
players' Lichess ratings for our stakes); only restricted open challenges are used (`users=White,Black`, no token of ours);
the challenge expires by itself after 10 minutes and a game not started by then is a draw that returns the stakes; a game
Lichess ended for cheating is held for a person, not paid.

Build order: (1) put the game result behind a swappable source, so the money side works with our server or with Lichess
and nothing is lost if Lichess says no; (2) linking a Lichess account; (3) creating the game and watching its result;
(4) the fair-play data from Lichess.

**Condition:** findings 1 and 2 above (Lichess's terms, and a written yes) still decide whether this can launch. Until
they come back, nothing here goes beyond the testnet, and Option A's code stays. The legal review of staking money is
unchanged: moving the games elsewhere does not move that responsibility.
