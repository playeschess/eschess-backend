# Escrow contract: testnet deployment log

First run of the escrow on a live network, following the runbook in the `eschess-contracts` repo (`docs/RUNBOOK.md`). Testnet only.

## Deployment

| | |
|---|---|
| Date | 2026-11-09 |
| Commit | `76e3a52` (main) |
| WASM SHA-256 | `34a2602a9d4897b2e8a354396ac78981e2157380ac181cfd7c59b5f3f8feb00c` |
| WASM check | CI artifact `escrow-wasm` (run 41825990462) and a local `stellar contract build` gave the same hash |
| stellar-cli | 28.0.0, the version CI pins (archive hash checked against CI's `STELLAR_CLI_SHA256`) |
| Network | Stellar testnet |
| Contract | `CAJLKHWOKJPUBLRG6GNZZQXYWSCFZE3VLMAVUOEDI3MC7GV4GVVUN6BT` |
| Token | testnet USDC (`USDC:GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5`), asset contract `CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA` |
| Admin at deploy | `GCQGQ4NLCT57AOOT5MAJPWABI6N6IX6YTI6LYOAKO5NCLYQXB5DKS3JH` (confirmed with `admin` straight after) |
| Admin now | `GA3324MHWAKHUX3MRATCUWQP2AUU2AW3EPHS3CYPS2ZWBINENZ7EIZHZ` (after the handover below) |

Test players: `GCAXUIIKXD6ZFBCTLBPA32LISCFNI3SPTPMNPIDSJ5ASGOZ4VOWTGCBC` (Ada) and
`GAG2WRYCJFRJX3GNJLUDMF4YDXO7UNQW4CJ3MQGS4V6QEI6IXIKCPFAU` (Grace). Stake 1 USDC each.

## What was run

| Check | Result |
|---|---|
| `create` simulated first (`--send=no`), then sent | Worked; the `Created` event came back |
| Win for A: both deposit, admin `settle WinA` | Ada 2 to 4 USDC, Grace stayed at 2, escrow back to 0 |
| Draw: both deposit, `settle Draw` | Each got their stake back |
| Cancel after one deposit | The depositor was refunded; the match closed |
| `settle` before both deposited | Refused, error 13 `NotFunded` |
| Depositing twice | Refused, error 12 `AlreadyDeposited` |
| Someone who is not a player depositing | Refused, error 11 `NotAPlayer` |
| `settle` on a cancelled match | Refused, error 8 `NotOpen` |
| A player calling `create` | Not possible: the call needs the admin's authorization |
| `refund_expired` before expiry plus grace | Refused, error 7 here only because the match had never been created (an earlier attempt used an expiry that had already passed); the real early-refund check is covered by the unit tests |
| Admin handover | `propose_admin`, then `pending_admin` showed the right key; the new key could do nothing before `accept_admin`; after it the old key could not act |
| `refund_expired` by an unrelated key after expiry plus grace | Worked: both stakes returned, escrow 0; a second call refused |

## Expiry refund

Match `52de3146f114bd00dcee4ee18dd21359`: both players deposited and it was left to expire (expiry 14:13:42 UTC, so
refundable from 15:13:42 UTC). Tested at about 23:15 UTC by a key that is not a player and is no longer the admin
(`eschess-admin`). `refund_expired` succeeded and returned both stakes: Ada 3 to 4 USDC, Grace 1 to 2, escrow 0. Calling it
again was refused with error 8 (`NotOpen`). So the "admin disappears" guarantee works on a live network.

## Things the run showed

- The CLI refuses to sign for a key that is not the caller's, so a non-admin calling `create` stops at signing; the on-chain
  rejection (`require_auth`) is covered by the unit tests, not shown here.
- `expires_at` must still be in the future when the transaction lands. A value computed a few minutes earlier in a script
  failed for that reason; the backend should compute it at submit time.
- Reading a value with `invoke` prints "Simulation identified as read-only" and the answer; add `--send=no` for reads.
- The runbook commands worked as written.

## Not yet tested

- Trustline removal by a player (a payout that cannot be delivered becomes claimable, then `claim`).
- A multisig admin.
- Anything on mainnet. This contract has not been audited.
