#!/usr/bin/env bash
# Staked games through the escrow contract, end to end, on the Stellar TESTNET: real API, worker and fair-play worker, real
# testnet USDC and the real contract. See try-contract-staking.py for what it plays and checks.
#
#   ./scripts/try-contract-staking.sh
#
# Needs (all under ~/.eschess-testnet, outside the repo): the testnet venv with stellar-sdk and chess (pip install chess), hot.secret
# (an account with at least 4 testnet USDC), funder.secret (testnet XLM), master.key, escrow-admin.secret (the contract's admin); ESCROW_CONTRACT
# in the environment; the programs built (cargo build -p eschess-api -p eschess-worker -p eschess-migrate, and the private
# repository's fairplay-worker, whose path is FAIRPLAY_WORKER). It uses a database of its own and drops it first.
set -uo pipefail
cd "$(dirname "$0")/.."
T="$HOME/.eschess-testnet"
PY="$T/venv/bin/python"
PORT="${PORT:-18082}"
PG=eschess-backend-postgres-1
DB=contract_e2e
FAIRPLAY_WORKER="${FAIRPLAY_WORKER:-../eschess-fairplay/target/debug/fairplay-worker}"
: "${ESCROW_CONTRACT:?set ESCROW_CONTRACT to the deployed contract}"
OUT="$(mktemp -d)"
PIDS=()

cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  sleep 1
  docker exec "$PG" psql -U eschess -d postgres -c "DROP DATABASE IF EXISTS $DB" >/dev/null 2>&1
}
trap cleanup EXIT

set -a; . ./.env; set +a
# The funding account (testnet XLM only) stands in for the platform wallet, so the real hot wallet's old test USDC is not
# counted against the ledger by the books check.
FUNDER_PUBLIC="$("$PY" -c "from stellar_sdk import Keypair; print(Keypair.from_secret(open('$T/funder.secret').read().strip()).public_key)")"
export STELLAR_HOT_ACCOUNT="$FUNDER_PUBLIC" STELLAR_HOT_SECRET_FILE="$T/funder.secret"
export WALLET_MASTER_KEY_FILE="$T/master.key" WALLET_FUNDER_SECRET_FILE="$T/funder.secret"
export ESCROW_ADMIN_SECRET_FILE="$T/escrow-admin.secret"

docker exec "$PG" psql -U eschess -d postgres -c "DROP DATABASE IF EXISTS $DB" -c "CREATE DATABASE $DB" >/dev/null || exit 1
export DATABASE_URL="postgres://eschess:eschess@127.0.0.1:5440/$DB"
target/debug/eschess-migrate >/dev/null || exit 1
docker exec "$PG" psql -U eschess -d "$DB" -c "UPDATE platform_settings SET value = 'contract' WHERE name = 'stake_path'" >/dev/null

BIND="127.0.0.1:$PORT" DEV_PRINT_EMAIL_TOKENS=true STAKES_USDC=0.5 target/debug/eschess-api >"$OUT/api.log" 2>&1 & PIDS+=($!)
SETTLE_SCORER=baseline-1 SETTLE_POLL_MS=1000 RECONCILE_INTERVAL_SECS=30 target/debug/eschess-worker >"$OUT/worker.log" 2>&1 & PIDS+=($!)
FAIRPLAY_POLL_MS=1000 "$FAIRPLAY_WORKER" >"$OUT/private-worker.log" 2>&1 & PIDS+=($!)
sleep 4

"$PY" scripts/try-contract-staking.py "http://127.0.0.1:$PORT" "$OUT/api.log" "$PG" "$DB" ; RESULT=$?
if [ $RESULT -ne 0 ]; then
  echo; echo "FAILED. Logs are in $OUT"; for f in worker private-worker; do echo "--- $f"; tail -n 12 "$OUT/$f.log" | sed 's/\x1b\[[0-9;]*m//g'; done
fi
exit $RESULT
