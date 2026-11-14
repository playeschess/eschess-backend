#!/usr/bin/env bash
# Staked games played on the REAL lichess.org, with money on the Stellar TESTNET (ADR 0005). Not automatic: two people log in
# at Lichess and play. See try-lichess-staking.py for what happens and what is checked.
#
#   LICHESS_WINDOW=60 ./scripts/try-lichess-staking.sh        (60 = no-show window in seconds, 60 to 3600; default 600)
#
# You need TWO Lichess accounts you control (new throwaway ones are fine), each logged in in a browser you can use during the run.
# Also needs (under ~/.eschess-testnet, outside the repo): the venv with stellar-sdk, hot.secret (an account with at least 2
# testnet USDC), funder.secret (testnet XLM), master.key; the programs built (cargo build -p eschess-api -p eschess-worker
# -p eschess-migrate, and the private repository's fairplay-worker, whose path is FAIRPLAY_WORKER). It uses a database of its
# own and drops it first. It plays no money on the public network.
set -uo pipefail
cd "$(dirname "$0")/.."
T="$HOME/.eschess-testnet"
PY="$T/venv/bin/python"
PORT="${PORT:-18083}"
PG=eschess-backend-postgres-1
DB=lichess_trial
FAIRPLAY_WORKER="${FAIRPLAY_WORKER:-../eschess-fairplay/target/debug/fairplay-worker}"
WINDOW="${LICHESS_WINDOW:-600}"
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
# Lichess sends the player back here after login. Nothing listens: the page will not load, and that is fine. The script asks
# you to paste the address your browser ends up on, which carries the code.
export LICHESS_REDIRECT_URI="http://localhost:3000/lichess"
export LICHESS_START_WINDOW_SECS="$WINDOW"

docker exec "$PG" psql -U eschess -d postgres -c "DROP DATABASE IF EXISTS $DB" -c "CREATE DATABASE $DB" >/dev/null || exit 1
export DATABASE_URL="postgres://eschess:eschess@127.0.0.1:5440/$DB"
target/debug/eschess-migrate >/dev/null || exit 1

BIND="127.0.0.1:$PORT" DEV_PRINT_EMAIL_TOKENS=true STAKES_USDC=0.5 target/debug/eschess-api >"$OUT/api.log" 2>&1 & PIDS+=($!)
SETTLE_SCORER=baseline-1 SETTLE_POLL_MS=1000 target/debug/eschess-worker >"$OUT/worker.log" 2>&1 & PIDS+=($!)
FAIRPLAY_POLL_MS=1000 "$FAIRPLAY_WORKER" >"$OUT/private-worker.log" 2>&1 & PIDS+=($!)
sleep 4

"$PY" scripts/try-lichess-staking.py "http://127.0.0.1:$PORT" "$OUT/api.log" "$PG" "$DB" "$WINDOW" ; RESULT=$?
echo; echo "Logs: $OUT"
if [ $RESULT -ne 0 ]; then
  for f in worker private-worker; do echo "--- $f"; tail -n 15 "$OUT/$f.log" | sed 's/\x1b\[[0-9;]*m//g'; done
fi
exit $RESULT
