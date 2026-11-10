# eSchess plan

Last updated: 2026-11-09. Plain words, step by step. Update the "Status" lines as work moves.

## The goal

Players stake money on chess games and the winner is paid automatically and fairly.

What the player sees:
- Sign up. A wallet address is created for them. They never download or connect a wallet.
- Add money in **USDC** (send it to their address) or, later, from their own currency.
- Enter an amount and press **Stake**. Nothing technical.
- Play. The winner is paid. Cheaters lose their stake to the honest player.
- Cash out in **USDC** (to any Stellar address they choose) or, later, to their own currency.

## Decisions made

| Decision | Choice |
|---|---|
| Where staked money is held | The on-chain escrow contract (`eschess-contracts`), not our database |
| Wallets | We generate a Stellar address for each user and hold the key (custodial). Accepted on purpose, to save users the hassle |
| Who creates games | Players, through our app. The contract's "admin" is our server's key, not a person |
| Platform fee | Postponed. The contract is not being changed for now, so it pays the winner the whole pot |
| Adding and withdrawing money | Two ways each, the same both directions. **USDC:** send it to the user's address to add, or withdraw to any Stellar address (works from day one). **Their own currency:** later, through a regulated ramp provider that converts to and from USDC. Provider not chosen |
| What happens to a cheater's stake | It goes to the honest player, automatically, once a reviewer confirms the cheating |
| Withdrawal delay | None. Approved withdrawals go out at once (a player with an open fair-play case still waits) |
| Unjoined staked games | Cancelled after 10 minutes, stake returned |

## What already exists

Done, merged and tested:
- Accounts, login, chess rules, clocks, games, live play over WebSocket, ratings, matchmaking.
- Stellar USDC deposits and withdrawals (run end to end on the testnet).
- Fair play: scoring, reviewer queue, sanctions, closing accounts with a second reviewer, appeals. A private baseline scorer in its own private repo.
- Staked games with a **database ledger** as the escrow: stakes held, settled once after fair play looked at the game, tested on the testnet with real USDC.
- The escrow contract exists in `eschess-contracts` (tested, **never run on a live network**, **not audited**).

Not built yet:
- The player-facing app (board, lobby, wallet screen).
- Anything that talks to the contract.
- Generated addresses per user.
- Fiat in and out.

## The plan

### Step 1. Prove the contract on the testnet

Status: **done on 2026-11-09** (details and results in `docs/escrow-testnet.md`)

What happened: the contract was built (our build and the CI build are byte-identical), deployed to the testnet, and run through a win, a draw, a cancel, the error cases and an admin handover. Contract address and details are in `docs/escrow-testnet.md`. The refund after expiry also worked. Not yet tried: a player removing their trustline, and a multisig admin.

1. Get the Stellar command-line tool and the contract's built file (the contract repo's CI builds it).
2. Deploy the contract to the Stellar testnet with a new admin key for our server.
3. Run a full match by hand: create, both players deposit, settle. Also try a refund after the deadline and a cancelled match.
4. Record the contract address and the file's fingerprint (the contract's runbook asks for this).

Done when: a real testnet match has been created, funded by two wallets and paid out by the contract, and the notes are written down.

Note: our account does not have write access to the `eschess-contracts` repo, so the log lives here for now.

### Step 2. Teach our backend to talk to the contract

Status: **mostly done on 2026-11-09** (the client is built and proven on the testnet; keeping the admin key safe for real money is still open, see step 7)

Today our Stellar code can only send plain payments. It needs to:
1. Call the contract (simulate first, then sign and submit, as the contract's runbook requires).
2. Read the contract's state and events.
3. Keep the admin key safe (testnet: a key file; real money: a proper key service).

Done when: a test in our repo creates and settles a match on the testnet through our code, not by hand.

Result: `crates/stellar/src/soroban.rs` (simulate, sign, send, wait; never signs a call the network says would fail) and `escrow.rs` (the contract's calls and reading a match back). The live test `crates/stellar/tests/escrow_testnet.rs` created, funded and settled a match through them. What is left of step 2 is not code: a proper key service for the admin key before real money.

### Step 3. A wallet for every user

Status: **in progress** (design accepted in `docs/adr/0006-user-wallets.md`).
- Done: the key vault and encrypted key store; every new user gets their own address at signup (testnet master key in a file).
- Next: set the account up on the network at the first deposit intent, then move deposit watching and withdrawals to the users' own addresses, then the books-against-chain check.

1. When someone signs up, create a Stellar account for them (we pay the small starting reserve and add the USDC trustline).
2. Store their key encrypted, readable only by the server.
3. Show them their deposit address and balance. Move the existing deposit/withdraw code over to these addresses.

This is the **USDC route** in both directions. Adding: the user (or anyone) sends USDC to their address and it shows up as their balance. Withdrawing: the user asks to send USDC to any Stellar address and we send it, with the same safeguards as today (daily limit, a person approves large amounts, a player with an open fair-play case waits). Both already work today with the shared platform address; this step moves them to each user's own address.

Done when: a new user can receive testnet USDC on their own address and see the balance, with no wallet app.

### Step 4. One-click staking on the contract

Status: not started

1. Player picks an amount and presses Stake, or joins the queue with a stake.
2. When two players are matched, our server creates the match on the contract and signs each player's deposit for them (we hold their keys).
3. Keep the fair-play rules we already built: wait for the fair-play check, a confirmed cheat pays the opponent, draws and very short games refund, a stuck game is cancelled.
4. When the game is settled, our server reports the result to the contract and the contract pays out.
5. Keep a check that the contract's balance matches what we expect, and stop paying if it ever does not.

Done when: the existing end-to-end test script plays staked games through the contract on the testnet and every pot is paid once, to the right person.

### Step 5. The app players use

Status: not started, needs decisions (web or mobile, and who builds it)

A board, a lobby, a Stake button, a wallet screen, notices ("your stake was held", "you won"). We can reuse open-source board pieces, but their licences must be checked first.

### Step 6. Fiat in and out

Status: not started, needs a provider. This is the second way to add and withdraw money; the USDC route (step 3) does not wait for it.

1. Pick a regulated ramp provider (compare fees, countries, ID checks).
2. Let users pay in their currency and receive USDC on their generated address.
3. Let users cash out the same way, back to their own currency.

### Step 7. Before real money (cannot be skipped)

Status: not started

- **Legal review:** whether this counts as gambling where players live, allowed countries, age checks, terms, holding users' keys and funds.
- **Contract audit:** an outside security review of the escrow contract. The contract's own rules say no mainnet before this.
- **Key safety:** the admin key and the users' keys held in a proper key service, not on a server disk.
- **Monitoring:** alerts for the contract's events and for the balance check.
- **Run the workers somewhere real:** the API, the worker and the private fair-play worker.

## Parallel track: Lichess

A teammate is reading Lichess's terms and may contact them. Nothing here waits on it. The write-up is `docs/adr/0005-game-venue.md`. If Lichess agrees in writing, the game server could be replaced later, but staking and escrow stay ours either way.

## Later (not now)

- Platform fee: needs a contract change, which means another look from the auditors. Easiest to do before the audit.
- Notifications in the app (planned, paused).
- Case notices for players, other currencies, mobile wallet options.

## Decisions still needed from you

1. App: web or mobile first, and who builds it?
2. Fee: how much, and when (needs the contract change).
3. Fiat provider and which countries.
4. Hold limit while a case is open (placeholder: 7 days) and who gets the escalation.
5. Appeal deadline (none today).
6. Where the servers will run.
7. Legal counsel: who is handling it?

## Rules we follow

- Testnet money only until the legal review and the audit are done.
- Every change goes through a pull request with tests; the fair-play and money code is checked twice.
- We do not change the contract without a written reason, because it is what the audit covers.
