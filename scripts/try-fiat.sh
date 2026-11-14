#!/usr/bin/env bash
# Adding money from, and cashing out to, "your own currency" through a Stellar anchor, end to end on the TESTNET, using
# Stellar's own test anchor (testanchor.stellar.org). Real API and worker, real testnet wallets and payments. Not automatic:
# the anchor's screens are a web form you fill in. See try-fiat.py.
#
#   ./scripts/try-fiat.sh
#
# Needs (under ~/.eschess-testnet, outside the repo): the venv with stellar-sdk, funder.secret (testnet XLM), master.key; the
# programs built (cargo build -p eschess-api -p eschess-worker -p eschess-migrate). Uses a database of its own and drops it first.
set -uo pipefail
cd "$(dirname "$0")/.."
T="$HOME/.eschess-testnet"
PY="$T/venv/bin/python"
PORT="${PORT:-18084}"
PG=eschess-backend-postgres-1
DB=fiat_trial
OUT="$(mktemp -d)"
PIDS=()

cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  sleep 1
  docker exec "$PG" psql -U eschess -d postgres -c "DROP DATABASE IF EXISTS $DB" >/dev/null 2>&1
}
trap cleanup EXIT

set -a; . ./.env; set +a
FUNDER_PUBLIC="$("$PY" -c "from stellar_sdk import Keypair; print(Keypair.from_secret(open('$T/funder.secret').read().strip()).public_key)")"
export STELLAR_HOT_ACCOUNT="$FUNDER_PUBLIC" STELLAR_HOT_SECRET_FILE="$T/funder.secret"
export WALLET_MASTER_KEY_FILE="$T/master.key" WALLET_FUNDER_SECRET_FILE="$T/funder.secret"
export FIAT_ANCHOR_DOMAIN="testanchor.stellar.org"
# The test anchor's USDC is the same testnet USDC the platform uses.
export STELLAR_USDC_ISSUER="GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5"
export WITHDRAW_MIN_USDC=1

docker exec "$PG" psql -U eschess -d postgres -c "DROP DATABASE IF EXISTS $DB" -c "CREATE DATABASE $DB" >/dev/null || exit 1
export DATABASE_URL="postgres://eschess:eschess@127.0.0.1:5440/$DB"
target/debug/eschess-migrate >/dev/null || exit 1

BIND="127.0.0.1:$PORT" DEV_PRINT_EMAIL_TOKENS=true target/debug/eschess-api >"$OUT/api.log" 2>&1 & PIDS+=($!)
target/debug/eschess-worker >"$OUT/worker.log" 2>&1 & PIDS+=($!)
sleep 4

"$PY" scripts/try-fiat.py "http://127.0.0.1:$PORT" "$OUT/api.log" "$PG" "$DB" ; RESULT=$?
echo; echo "Logs: $OUT"
if [ $RESULT -ne 0 ]; then
  echo "--- worker"; tail -n 20 "$OUT/worker.log" | sed 's/\x1b\[[0-9;]*m//g'
  echo "--- api"; tail -n 10 "$OUT/api.log" | sed 's/\x1b\[[0-9;]*m//g'
fi
exit $RESULT
