# ADR 0008 — Fiat in and out through a Stellar anchor

**Status:** Built and tested against a fake anchor and Stellar's test anchor; the real provider is not chosen · **Date:** 2026-11-14 ·
**Follows:** ADR 0001 (money in and out), ADR 0006 (user wallets)

## What the player sees

On the wallet screen: "Add money" and "Cash out" in their own currency. Each opens the provider's own screens (ID check, bank or
card, amount). Afterwards the money shows up in their balance, or in their bank.

## How it works

The provider is a Stellar **anchor**: a regulated company that takes fiat and pays USDC on Stellar, and the other way round. Players
and the platform never hold the fiat. The standard way to talk to one is SEP-24 (its own screens), with SEP-10 (proving you own a
Stellar account) and SEP-1 (the company's `stellar.toml`, which names its keys and accounts).

- **Add money.** We log in to the anchor *as the player* (we hold their key, ADR 0006) and open a deposit for their own address.
  The player pays in the anchor's screens; the anchor sends USDC to their address; the deposit watcher we already have credits it.
  Nothing new moves money.
- **Cash out.** Same login, a withdrawal screen. When the player has finished, the anchor tells us how much USDC to send, where, and
  with which memo. The worker sends that as an **ordinary withdrawal** from the player's own account. So daily limits, approval of
  large amounts, the hold while a fair-play case is open, and the books-against-chain check all apply with no new code. The
  withdrawal is keyed by the session, so it can never be made twice.

## What can go wrong, and what stops it

- **Signing for the anchor with the player's key.** SEP-10 asks us to sign a challenge transaction. The code signs only a challenge of
  exactly the shape the standard allows (sequence number zero, only data entries, the first naming the player's account and the
  anchor's domain, made and signed by the anchor's key, not expired). Fourteen hostile variants are tested to never reach the key.
- **An anchor telling us to pay the wrong place.** We send a cash-out only to an account the anchor's own `stellar.toml` lists, never
  our wallet or the USDC issuer, with an id or text memo and a positive USDC amount. Anything else fails the session and sends nothing.
- **The wrong asset.** The anchor's `stellar.toml` must list USDC from our issuer, on the right network.
- **The screen's address carries a login token.** It is returned once to the player and never stored or logged.
- **Not enough money yet.** The session waits and retries while the anchor waits, and tells the player why.
- **The anchor gives up.** A cash-out payment still waiting is cancelled and the money goes back.
- **Flooding.** At most three open sessions per player.

## What is not decided

- **Which provider, and which countries and currencies.** The company is a setting (`FIAT_ANCHOR_DOMAIN`); nothing in the code names one.
  The provider must support SEP-24 for USDC on Stellar. Their fees, limits, countries, currencies and ID rules decide the product;
  they are the owner's to choose, and a provider will want a contract before it opens to real customers.
- **Legal.** Whether the platform's part (starting the screens, sending a cash-out) needs more than the provider's own licences is a
  question for the legal review, like holding the keys (ADR 0006).
- **Testnet only** until then, like everything else.
