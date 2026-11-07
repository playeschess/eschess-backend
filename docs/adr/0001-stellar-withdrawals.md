# ADR 0001 — Stellar withdrawals

**Status:** Proposed · **Date:** 2026-11-06 · **Architecture refs:** eschess-docs §5.4 (money path), §8.3, §9 (keys), §14 (failure modes)

## Context

Players can deposit USDC on the Stellar testnet (PR #9). They cannot get it out. Withdrawals are the first code path
in which **eSchess sends money**, so they are the first place a bug can lose real funds: pay twice, pay the wrong
person, or pay out money the player does not have.

The architecture document makes the rules for this path non-negotiable:

1. Balance is a database invariant. No balance is held anywhere but the ledger.
2. **One intent, many attempts.** A payout is one row; each try at sending it is a separate attempt row.
3. **Unknown is not failed.** A submission that times out is `UNKNOWN` and is resolved by looking up its
   transaction hash. Blindly building and sending a new transaction is the double-payout bug and must not exist as a
   code path.
4. Timebounds on every transaction, so that "it never landed" can be *proved* rather than assumed.
5. Prepare, approve and sign are three separate principals. Above a threshold, approval is a human.
6. Signing keys never live in environment variables or on a developer machine (production).

What exists today that we build on:

- `ledger_accounts` / `ledger_entries` with balanced postings, idempotency keys and a no-overdraft check in the database.
- `custody_stellar` (debit-normal, may not go negative): the ledger's mirror of what the hot wallet holds. Deposits
  debit it and credit the player's `user_wallet`.
- `eschess-stellar`: reads Horizon behind the `Chain` trait. It cannot build or sign transactions yet.
- No admin roles, no worker binary: the deposit watcher runs inside `api`.

## Decision

### 1. Money moves through the ledger in three postings

| When | Posting | Effect |
|---|---|---|
| Player requests a withdrawal | debit `user_wallet`, credit `platform_withdrawals_pending` | Funds are **held**: they leave the player's spendable balance at once, so they cannot be staked or withdrawn twice. The database refuses the posting if the balance is too small, so two racing requests cannot both succeed. |
| The payment is confirmed on chain | debit `platform_withdrawals_pending`, credit `custody_stellar` | The wallet's mirror goes down by what actually left. `custody_stellar` may not go negative, so the ledger can never pay out more than it recorded coming in. |
| The withdrawal definitively fails, or is cancelled / rejected before sending | debit `platform_withdrawals_pending`, credit `user_wallet` | The hold is returned to the player. |

Each posting uses an idempotency key derived from the withdrawal id and the step (`wd:<id>:hold`, `wd:<id>:settle`,
`wd:<id>:refund`), so a step repeated after a crash posts once. `settle` and `refund` are mutually exclusive: the
withdrawal's states only move forward (a trigger), and a unique index on the ledger allows one hold and one outcome
(settle or refund) per withdrawal, whatever idempotency key a posting uses.

The network fee is paid in XLM by the hot wallet, not charged to the player, for now. The fee each attempt was charged
is recorded on the attempt (`fee_charged_stroops`) but **not yet posted to the ledger**: the hot wallet's XLM was funded
outside the ledger, so the XLM `custody_stellar` account does not hold it and a fee posting would overdraw it. Fees
move into the ledger (debit `platform_network_fees`, credit `custody_stellar`) once funding the hot wallet is itself a
recorded posting. Who pays fees in production is an open product decision (PRD OD-16).

### 2. One intent, many attempts

```
withdrawals            -- the intent: who, how much, to where. One row per request, never re-created.
withdrawal_attempts    -- each signed transaction we built for it: hash, source sequence number, max_time, envelope XDR.
```

`withdrawals.status`:

```
awaiting_approval ──► approved | rejected* | cancelled*
approved          ──► submitting | cancelled*
submitting        ──► confirmed | failed*
                                             * refunded in the same transaction
confirmed, failed, rejected, cancelled are final.
```

- A request starts as `approved` when the amount is at or below the auto-approve threshold, otherwise as
  `awaiting_approval`. The hold is posted in the same transaction as the insert.
- An approver may reject only while `awaiting_approval`.
- The player may cancel while `awaiting_approval` or `approved` (nothing has been signed yet).
- `submitting` means *at least one attempt exists*. From here the player can no longer cancel, because a signed
  transaction may be in flight.
- `failed` is reached only when **every** attempt is proved not to have landed (below) and we decide to stop. The refund
  posting happens in the same database transaction as the status change.

Uniqueness: `withdrawals` has a unique `(user_id, client_request_id)` so a retried HTTP request does not create a second
intent. `withdrawal_attempts` has a unique `tx_hash` and a partial unique index allowing **at most one attempt per
withdrawal that is not yet resolved**. That index is what makes "send another transaction while the last one might still
land" impossible at the database level, not only in code.

### 3. Sending, and resolving `UNKNOWN`

Each attempt is built, signed, and **written to `withdrawal_attempts` with its hash and envelope before it is
submitted**. Then it is submitted. The attempt's states are `pending → (success | failed | expired)`.

| Horizon says | Meaning | Action |
|---|---|---|
| Accepted, result success | It landed | Attempt `success`, withdrawal `confirmed`, post `settle` + fee. |
| Accepted, result failed (e.g. no trustline) | It landed and did nothing (fee still charged) | Attempt `failed` with the result code. Retry only for codes that a retry can fix; otherwise withdrawal `failed` + refund. |
| Timeout, 5xx, connection dropped | **Unknown** | Do nothing new. Look the hash up later. |
| Hash lookup: found | Same as "accepted" rows above | |
| Hash lookup: not found, *and* the latest closed ledger's close time is past the attempt's `max_time` | Proved it can never land (timebounds expired) | Attempt `expired`. Only now may a new attempt be built. |
| Hash lookup: not found, `max_time` not yet passed | Still could land | Wait. Re-sending the **same stored envelope** is allowed (same hash, same sequence number: the network applies it at most once). Building a new one is not. |

Timebounds: `max_time = now + 2 minutes`. Short, so an unknown resolves quickly; long enough for normal ledger close
times (~5 s). The value is config. "Past `max_time`" means past it by a margin as well
(`WITHDRAW_EXPIRY_MARGIN_SECS`, 60 s): the ledger time and the hash lookup may be answered by different Horizon servers
behind a load balancer, and a server that is behind could miss a transaction that did land. The ledger time is read
before the hashes are looked up.

### 4. Approval and the three principals

- **Prepare** — the API, acting for the player. It validates and records the intent and the hold. It never signs.
- **Approve** — automatic at or below `WITHDRAW_AUTO_APPROVE_MINOR` (per asset); above it, a person with the new
  `withdrawal_approver` role, who **may not be the requesting player**. The approval records who and when. An approver
  can also reject (refund).
- **Sign** — a `Signer` trait in `eschess-stellar`. The submitter asks it to sign a transaction hash; it holds the key.

There is no admin role today. This ADR adds the smallest version: a `user_roles (user_id, role)` table, granted by SQL
for now, and `GET /v1/admin/withdrawals?status=awaiting_approval`, `POST /v1/admin/withdrawals/{id}/approve|reject`. The
same role mechanism will later cover the deposit review queue.

### 5. Keys

- `Signer` implementations: `LocalSigner` (an ed25519 secret read from a file path, **refused at startup unless
  `STELLAR_NETWORK=testnet`**) and, later, a KMS/HSM signer. The code that builds and submits transactions only ever
  sees the trait.
- The secret is read from a file (`STELLAR_HOT_SECRET_FILE`), not an environment variable, so it does not appear in
  `ps`, crash dumps or process listings, and the habit carries to production secrets mounting.
- Deposits keep working with no secret at all: the watcher still needs only the public address.

### 6. Building transactions

Add the official `stellar-xdr` crate (XDR types and encoding) and `ed25519-dalek` (signing) to `eschess-stellar`. We
build one `Payment` operation per transaction, with timebounds, the hot wallet's next sequence number, and the
destination's memo if given. The transaction hash is computed locally (SHA-256 of the network id and the transaction),
so it is known **before** submission.

Sequence numbers: one source account means one sequence number, so for now **one withdrawal is submitted at a time**
(a Postgres advisory lock, like the deposit watcher's). This is slow but correct. Channel accounts (architecture §9) come
when throughput needs them; fee-bump transactions come with them.

### 7. Validation at request time

Refused before any hold is placed:

- Email not confirmed (`AuthUser::require_verified`), or the asset is not one we accept.
- Amount below `WITHDRAW_MIN_MINOR`, or above the player's balance (the database refuses this anyway).
- Destination is not a valid G or M address, or is the hot wallet itself (or an `M...` built on it — that would be a
  deposit to ourselves).
- Destination account does not exist on chain, or has no trustline for the asset (checked on Horizon; a payment would
  fail and still cost a fee). This check can race (the trustline could be removed after), which is fine: the on-chain
  failure path refunds.
- More than `WITHDRAW_DAILY_LIMIT_MINOR` withdrawn in the last 24 hours (velocity limit; the exact figure is a product
  decision).

### 8. Where it runs

The submitter and resolver run as a background loop. Per architecture §5.3, money jobs belong in a separate `worker`
binary so a retry storm cannot slow the move loop. This work **creates `bins/worker`** and moves the deposit watcher into
it alongside the withdrawal submitter. The API keeps no background money tasks. Every job stays safe to run on several
workers at once (advisory locks + idempotency keys).

## Consequences

- A withdrawal request with too little balance, a duplicate HTTP retry, two racing requests, or a crash at any step can
  never pay twice or pay without a hold — each is enforced by a database constraint, not just code.
- Withdrawals are serialised on one source account. Fine for testnet and a pilot; channel accounts are a known next step.
- Two new dependencies on the money path (`stellar-xdr`, `ed25519-dalek`). Both are widely used; versions pinned in
  `Cargo.lock`.
- A new binary to run locally (`make worker`) and deploy.
- First admin surface and role table. Small on purpose; it will need audit logging (architecture §13, hash-chained audit
  log) before production.

## Not decided here (and not blocking testnet)

- Production signing: KMS/HSM choice, m-of-n multisig on the treasury, hot/cold wallet split and sweep policy.
- Who pays network fees, and whether players pay a withdrawal fee (OD-16).
- Payout hold window before sending (OD-13). The status machine has room for it: a delay between `approved` and
  `submitting`.
- Final threshold and limit numbers.

## Build order

Each is one PR, tests first:

1. **Transaction building and signing** in `eschess-stellar` (pure: build, hash, sign, encode), checked against known
   testnet transactions. `Signer` trait + `LocalSigner`.
2. **Migration + storage**: `withdrawals`, `withdrawal_attempts`, `user_roles`; hold / settle / refund postings;
   property test that no sequence of steps produces a payout and a refund for the same withdrawal.
3. **API**: request, list, cancel; admin approve / reject.
4. **`bins/worker`**: move the deposit watcher; add the submitter and the unknown resolver, with a fake `Chain` that can
   time out, drop, and land late.
5. **Live testnet run**: a real withdrawal end to end, plus a real muxed deposit (which also closes the open question from
   PR #9).

All five steps are done (PRs #11/#12, #13, #14, #15, and the testnet run in the README, 2026-11-06).
