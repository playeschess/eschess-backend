# ADR 0006 — A wallet for every user

**Status:** Accepted by the owner on 2026-11-09 (decisions below) · **Date:** 2026-11-09 · **Follows:** ADR 0001 (money in and out), ADR 0004 (staked games), the escrow
contract in `eschess-contracts` (see `plan.md`, steps 3 and 4)

## Why

Staking will move onto the escrow contract. The contract identifies each player by their own Stellar address and needs
that address to authorize its own deposit. Today all players share one platform wallet, told apart by a number in the
address (a "muxed" address), and the platform's database is the only record of who owns what. That cannot sign a
deposit as one particular player.

The owner's decision (`plan.md`): every user gets their own address at signup, **the platform holds the key** (no wallet
to download or connect), and players just press Stake.

This ADR is about what that does to how money is recorded, so the team agrees before code is written. It is safe to
publish; it contains no keys or key locations.

## What stays true, and what changes

**The rule we keep: the ledger is the books.** Every balance a player sees, and every rule that stops spending money you
do not have, still comes from the double-entry ledger in the database. The chain is custody; the ledger is the record.
The two are compared continuously and any difference stops payouts (the same stop-the-line rule as the escrow check in
ADR 0004). Nothing in the game, fair-play or settlement code needs to change its idea of a balance.

**What changes: where the money physically is.** Instead of one platform wallet holding everyone's USDC, each user's USDC
sits in their own Stellar account, and during a game their stake sits in the escrow contract.

| | Today | With user wallets |
|---|---|---|
| Where a deposit lands | the shared platform wallet, told apart by a muxed id | the user's own address |
| Where the money sits between games | the shared platform wallet | the user's own account |
| Where a stake sits during a game | a ledger entry (`stake_escrow`) | the escrow contract |
| Who can move it | the platform (one key) | the platform (one key per user, held by us), or the contract |
| What the ledger records | the same | the same, mirroring the chain |

## Decision

### 1. Creating the wallet

At signup we generate a Stellar keypair for the user. The **address is known at once** (shown on the deposit screen) and
stored with the user. The key is encrypted before it is stored (see §2). The account does not exist on the network until
someone pays it, so a background job **provisions** it shortly after: it creates the account, gives it a small XLM
balance for the account reserve and for transaction fees, and adds the USDC trustline. Until provisioned, a deposit
address is shown but the user is told it is "being set up" (a few seconds). A user who deposits earlier than that is
not a problem: a payment of USDC to an account without a trustline fails on the network and the sender keeps their
money, so provisioning is what makes the address usable.

Cost: the reserve (about 1.5 XLM for the account and its trustline) plus a few XLM for fees per user, paid from a
platform **funding account**. This is real money on the public network, so it is a number to set (§ Decisions needed).

### 2. Keeping the keys

The platform holds every user's key, so this is the most security-sensitive code in the system.

- A user's secret key is encrypted with an authenticated cipher (AES-256-GCM) using a **master key**, with the user's id
  bound in as associated data, so a ciphertext cannot be swapped between users. Only the ciphertext and a key version
  are stored.
- The master key is behind a `KeyVault` interface, the same seam `Signer` is for the platform key. For the testnet it is
  a key file outside the repository. **For real money it must be a hardware-backed key service**, with the master key
  never on the application server. This is a hard requirement before mainnet, not a later improvement.
- Keys are decrypted only inside the signing call, held in memory zeroed afterwards (the existing signer already does
  this), and never logged, returned by the API, or put in an error message.
- Signing is **only** done by the platform's own code paths, for exactly these actions: the user's stake deposits into
  the contract, a withdrawal the user asked for, and housekeeping (trustline). There is no "sign anything" endpoint.
- A backup and recovery process for the master key is required: if it is lost, every user's funds are unreachable.
  Its design is open (§ Decisions needed).

### 3. How a deposit becomes a balance

A deposit is a USDC payment to the user's own address. The watcher that credits deposits today reads the platform
wallet's payments; it now reads the **users' addresses'** payments instead, and credits the ledger exactly once per
payment as today (the payment's operation id is the unique key). The ledger rule is unchanged: debit `custody_stellar`,
credit the user's wallet.

Watching many accounts does not scale by asking about each one every few seconds. So the watcher polls
**adaptively**: an address is checked often while it has recently had activity or the user has the deposit screen
open, and rarely otherwise, with a slow sweep over everyone as a safety net. At the start (hundreds of users) this is
cheap; if it ever is not, the answer is to run our own network access point, not to change the design. Payments to an
address are never lost by being checked late, only credited late.

### 4. Withdrawals

A withdrawal is paid **from the user's own account** to the address they chose, signed with their key by the existing
withdrawal machinery (one intent, many attempts, timebounds, prove-it-never-landed before rebuilding). The ledger flow
and the safeguards (daily limit, approval of large amounts, hold while a fair-play case is open) are unchanged; only the
source of the money changes from the shared wallet to the user's account. The user keeps enough XLM in the account for
the reserve; the platform pays the fee.

### 5. Staking

Staked games follow ADR 0004's rules (wait for fair play, confirmed cheat pays the opponent, draws and short games
refund, stop on a mismatch), with the stake held in the contract instead of a ledger account. That is the next step and
has its own record; what this ADR guarantees it is that each player has an address that can sign their deposit.

### 6. Checking the books against the chain

A job compares, per asset: the sum of every user wallet's on-chain balance, plus what the escrow contract holds, plus
the funding and platform accounts, against what the ledger says the platform owes. Any difference stops staking and
withdrawals and raises an alert; nothing is repaired automatically. This replaces "the hot wallet holds what the ledger
says it holds" and is the new central safety check.

## What this does not do

- It does not let players hold or export their own key. That would be non-custodial and is a different product
  decision; it can be offered later (export, or bring your own wallet) without changing the contract.
- It does not change who may stake, fair play, sanctions, or appeals.
- The existing shared-wallet deposit addresses keep working during the move; money sent to one is still credited. They
  are retired once no user depends on them.

## Risks, plainly

- **Custody.** If the master key or the key store is stolen, every user's funds can be stolen. This is the cost of "no
  wallet to download". Mitigations are the hardware-backed key service, limits on how much any one signing path can
  move, and the books-against-chain check; none of them makes it risk-free. It also makes the platform a custodian,
  which is a legal question for the review, not an engineering detail.
- **Reserve cost.** Each account locks a little XLM. Accounts of users who never deposit still cost it, so provisioning
  could wait until a user's first deposit intent (e.g. opening the deposit screen).
- **Lost master key** loses everyone's funds. Backup design is not optional.

## Decisions (owner, 2026-11-09)

1. **The on-chain account is set up at the user's first deposit intent**, not at signup. The address itself is generated
   at signup, because that costs nothing.
2. **XLM per account:** about 2 XLM per active user to start (the reserve plus fees), funded from a platform funding account
   that we keep stocked by buying XLM on an exchange (free from the friendbot on the testnet). Reserve values are set by
   the network and must be checked at launch. Later options: the platform pays fees by fee bump and sponsors the reserve,
   so users hold no XLM of their own. The exact amount is a configuration value.
3. **Master key:** the key service of the cloud that hosts the servers, used so the application never holds the master key.
   Which cloud is not decided (it depends on where the servers run). Until then, a key file on the testnet only.
4. **Holding user keys is acceptable to the owner.** It stays on the list for the legal review.

## Build order (after acceptance)

1. The `KeyVault` and the encrypted key store; create the address at signup (no network needed).
2. The provisioning job: create the account, fund it, add the trustline (testnet).
3. Switch deposit watching to user addresses, with adaptive polling.
4. Withdrawals from the user's account.
5. The books-against-chain check.
6. Then staking on the contract (the next ADR).
