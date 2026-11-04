#!/usr/bin/env bash
# Tries every endpoint end to end against a real server and a real database, and says what passed.
#
#   ./scripts/try-api.sh        (or: make try)
#
# It starts Postgres (Docker), applies the migrations, starts the API on its own port with the emailed tokens
# printed in its log (so no mail server is needed), runs the checks below, then stops the server again.
# Every run uses fresh emails and usernames, so you can run it as often as you like.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-18080}"
BASE="http://127.0.0.1:$PORT"
export DATABASE_URL="${DATABASE_URL:-postgres://eschess:eschess@127.0.0.1:5440/eschess}"
LOG="$(mktemp)"
TMP="$(mktemp)"
PASS=0
FAIL=0
SERVER_PID=""

green() { printf '\033[32m%s\033[0m' "$1"; }
red() { printf '\033[31m%s\033[0m' "$1"; }
bold() { printf '\033[1m%s\033[0m\n' "$1"; }

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  rm -f "$TMP"
}
trap cleanup EXIT

# call METHOD PATH [TOKEN] [JSON_BODY]  ->  sets STATUS and BODY
call() {
  local method=$1 path=$2 token=${3:-} body=${4:-}
  local args=(-s -o "$TMP" -w '%{http_code}' -X "$method" "$BASE$path")
  [ -n "$token" ] && args+=(-H "authorization: Bearer $token")
  [ -n "$body" ] && args+=(-H 'content-type: application/json' -d "$body")
  STATUS=$(curl "${args[@]}")
  BODY=$(cat "$TMP")
}

# field PATH  ->  prints that field of the last response, e.g. `field user.email_verified`
field() {
  python3 - "$BODY" "$1" <<'PY'
import sys, json
try:
    d = json.loads(sys.argv[1])
    for k in sys.argv[2].split('.'):
        d = d[int(k)] if k.isdigit() else d[k]
    print(str(d).lower() if isinstance(d, bool) else d)
except Exception:
    print("")
PY
}

# check "what we expect" EXPECTED ACTUAL
check() {
  if [ "$2" = "$3" ]; then
    PASS=$((PASS + 1))
    printf '  %s %s\n' "$(green '✓')" "$1"
  else
    FAIL=$((FAIL + 1))
    printf '  %s %s\n      expected: %s\n      got:      %s\n' "$(red '✗')" "$1" "$2" "$3"
    [ -n "${BODY:-}" ] && printf '      last response: %s\n' "$BODY"
  fi
}

# The newest token the server "emailed" to $1 for message kind $2 (verify-email or password-reset).
emailed_token() {
  local tok="" n
  for n in $(seq 1 50); do
    tok=$(grep -F "$2" "$LOG" | grep -F "$1" | grep -o 'token [0-9a-f]\{64\}' | tail -1 | cut -d' ' -f2)
    [ -n "$tok" ] && break
    sleep 0.1
  done
  echo "$tok"
}

# ---------------------------------------------------------------- start everything

bold "Starting Postgres, the migrations and the API"
docker compose up -d --wait >/dev/null 2>&1 || { echo "Could not start Postgres. Is Docker running?"; exit 2; }
cargo run -q -p eschess-migrate >/dev/null || { echo "Migrations failed."; exit 2; }
cargo build -q -p eschess-api || { echo "Build failed."; exit 2; }
BIND="127.0.0.1:$PORT" DEV_PRINT_EMAIL_TOKENS=true target/debug/eschess-api >"$LOG" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 60); do curl -s "$BASE/health" >/dev/null 2>&1 && break; sleep 0.5; done
curl -s "$BASE/health" >/dev/null 2>&1 || { echo "The API did not start. Its log:"; cat "$LOG"; exit 2; }

RUN="$(date +%s)"
EMAIL="try.$RUN@example.com"
NAME="try_$RUN"
PASSWORD="correct horse battery"
NEW_PASSWORD="a brand new passphrase"

# ---------------------------------------------------------------- the checks

bold "Health"
call GET /health
check "GET /health answers 200" 200 "$STATUS"
check "and says it is the eschess-api" eschess-api "$(field service)"

bold "Signing up"
call POST /v1/auth/signup "" "{\"email\":\"$EMAIL\",\"username\":\"$NAME\",\"password\":\"$PASSWORD\"}"
check "signup answers 201" 201 "$STATUS"
ACCESS=$(field access_token)
REFRESH=$(field refresh_token)
check "it returns an access token and a refresh token" yes "$([ -n "$ACCESS" ] && [ -n "$REFRESH" ] && echo yes || echo no)"
check "the email starts out unconfirmed" false "$(field user.email_verified)"
check "the password is not in the response" no "$(echo "$BODY" | grep -q "$PASSWORD" && echo yes || echo no)"

call POST /v1/auth/signup "" "{\"email\":\"$(echo "$EMAIL" | tr a-z A-Z)\",\"username\":\"se_$RUN\",\"password\":\"$PASSWORD\"}"
check "the same email (any capitals) is refused with 409" 409 "$STATUS"
check "and the error says email_taken" email_taken "$(field error.code)"
call POST /v1/auth/signup "" "{\"email\":\"other.$RUN@example.com\",\"username\":\"$(echo "$NAME" | tr a-z A-Z)\",\"password\":\"$PASSWORD\"}"
check "the same username (any capitals) is refused with 409" 409 "$STATUS"
check "and the error says username_taken" username_taken "$(field error.code)"
call POST /v1/auth/signup "" "{\"email\":\"not-an-email\",\"username\":\"abc_$RUN\",\"password\":\"$PASSWORD\"}"
check "a badly formed email is refused with 400" 400 "$STATUS"
call POST /v1/auth/signup "" "{\"email\":\"weak.$RUN@example.com\",\"username\":\"weak_$RUN\",\"password\":\"short\"}"
check "a too-short password is refused with 400" 400 "$STATUS"

bold "Your profile and wallet"
call GET /v1/me
check "GET /v1/me without a token is refused with 401" 401 "$STATUS"
call GET /v1/me "$ACCESS"
check "GET /v1/me with the token answers 200" 200 "$STATUS"
check "it shows your username" "$NAME" "$(field username)"
check "it shows a wallet in XLM" XLM "$(field wallet.0.asset_id)"
check "with a balance of 0 (as a string)" 0 "$(field wallet.0.amount_minor)"

bold "Confirming the email"
call POST /v1/auth/email/resend "$ACCESS"
check "asking for another email straight after signup is told to wait (429)" 429 "$STATUS"
TOKEN="$(emailed_token "$EMAIL" verify-email)"
check "a confirmation email was sent (its token is in the server log)" 64 "${#TOKEN}"
call POST /v1/auth/email/verify "" '{"token":"0000000000000000000000000000000000000000000000000000000000000000"}'
check "a made-up token is refused with 400" 400 "$STATUS"
call POST /v1/auth/email/verify "" "{\"token\":\"$TOKEN\"}"
check "the real token confirms the email (200)" 200 "$STATUS"
call GET /v1/me "$ACCESS"
check "/v1/me now says the email is confirmed" true "$(field email_verified)"
call POST /v1/auth/email/verify "" "{\"token\":\"$TOKEN\"}"
check "the same token cannot be used twice (400)" 400 "$STATUS"
call POST /v1/auth/email/resend "$ACCESS"
check "an already confirmed player is told so (409)" 409 "$STATUS"

bold "Devices and sessions"
call POST /v1/auth/login "" "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\",\"device_label\":\"second device\"}"
check "logging in again answers 200" 200 "$STATUS"
SECOND_ACCESS=$(field access_token)
call GET /v1/auth/sessions "$ACCESS"
check "the session list shows both devices" 2 "$(python3 -c "import sys,json; print(len(json.loads(sys.argv[1])['sessions']))" "$BODY")"
SECOND_ID=$(python3 -c "import sys,json; print([s['id'] for s in json.loads(sys.argv[1])['sessions'] if s['device_label']=='second device'][0])" "$BODY")
call DELETE "/v1/auth/sessions/$SECOND_ID" "$ACCESS"
check "signing out the second device answers 204" 204 "$STATUS"
call GET /v1/me "$SECOND_ACCESS"
check "the second device's token stops working (401)" 401 "$STATUS"
call GET /v1/me "$ACCESS"
check "this device still works (200)" 200 "$STATUS"
call DELETE "/v1/auth/sessions/00000000-0000-7000-8000-000000000000" "$ACCESS"
check "ending a session that is not yours (or does not exist) is 404" 404 "$STATUS"

bold "Refreshing tokens"
call POST /v1/auth/refresh "" "{\"refresh_token\":\"$REFRESH\"}"
check "refreshing answers 200" 200 "$STATUS"
NEW_ACCESS=$(field access_token)
NEW_REFRESH=$(field refresh_token)
call GET /v1/me "$ACCESS"
check "the old access token stops working (401)" 401 "$STATUS"
call GET /v1/me "$NEW_ACCESS"
check "the new access token works (200)" 200 "$STATUS"
call POST /v1/auth/refresh "" "{\"refresh_token\":\"$REFRESH\"}"
check "replaying the old refresh token is refused (401)" 401 "$STATUS"
call GET /v1/me "$NEW_ACCESS"
check "and that replay ended the whole session (401)" 401 "$STATUS"
call POST /v1/auth/refresh "" "{\"refresh_token\":\"$NEW_REFRESH\"}"
check "so even the newest refresh token is dead (401)" 401 "$STATUS"

bold "Logging out"
call POST /v1/auth/login "" "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}"
LOGIN_ACCESS=$(field access_token)
call POST /v1/auth/logout "$LOGIN_ACCESS"
check "logout answers 204" 204 "$STATUS"
call GET /v1/me "$LOGIN_ACCESS"
check "the token stops working at once (401)" 401 "$STATUS"

bold "Wrong passwords and lockout"
call POST /v1/auth/login "" "{\"email\":\"$EMAIL\",\"password\":\"wrong password here\"}"
WRONG_BODY="$BODY"
check "a wrong password is refused (401)" 401 "$STATUS"
call POST /v1/auth/login "" "{\"email\":\"nobody.$RUN@example.com\",\"password\":\"wrong password here\"}"
check "an unknown email gets exactly the same answer" "$WRONG_BODY" "$BODY"
LOCK_EMAIL="lock.$RUN@example.com"
call POST /v1/auth/signup "" "{\"email\":\"$LOCK_EMAIL\",\"username\":\"lock_$RUN\",\"password\":\"$PASSWORD\"}"
for _ in 1 2 3 4 5; do call POST /v1/auth/login "" "{\"email\":\"$LOCK_EMAIL\",\"password\":\"wrong password here\"}"; done
call POST /v1/auth/login "" "{\"email\":\"$LOCK_EMAIL\",\"password\":\"$PASSWORD\"}"
check "after 5 wrong passwords the account is locked, even for the right one (401)" 401 "$STATUS"

bold "Forgotten password"
call POST /v1/auth/password/forgot "" "{\"email\":\"$EMAIL\"}"
REAL_STATUS="$STATUS"; REAL_BODY="$BODY"
call POST /v1/auth/password/forgot "" "{\"email\":\"nobody.$RUN@example.com\"}"
check "a registered and an unregistered address get the identical answer" "$REAL_STATUS $REAL_BODY" "$STATUS $BODY"
check "that answer is 202 \"check your email\"" check_your_email "$(field status)"
RESET="$(emailed_token "$EMAIL" password-reset)"
check "a reset email was sent to the real address (its token is in the log)" 64 "${#RESET}"
call POST /v1/auth/password/reset "" "{\"token\":\"$RESET\",\"new_password\":\"short\"}"
check "a too-short new password is refused (400)" 400 "$STATUS"
call POST /v1/auth/login "" "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}"
OLD_PASSWORD_ACCESS=$(field access_token)
call POST /v1/auth/password/reset "" "{\"token\":\"$RESET\",\"new_password\":\"$NEW_PASSWORD\"}"
check "the reset link sets the new password (200)" 200 "$STATUS"
call POST /v1/auth/password/reset "" "{\"token\":\"$RESET\",\"new_password\":\"another passphrase here\"}"
check "the same link cannot be used twice (400)" 400 "$STATUS"
call GET /v1/me "$OLD_PASSWORD_ACCESS"
check "every device was signed out by the reset (401)" 401 "$STATUS"
call POST /v1/auth/login "" "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}"
check "the old password no longer works (401)" 401 "$STATUS"
call POST /v1/auth/login "" "{\"email\":\"$EMAIL\",\"password\":\"$NEW_PASSWORD\"}"
check "the new password works (200)" 200 "$STATUS"

# ---------------------------------------------------------------- result

echo
if [ "$FAIL" -eq 0 ]; then
  printf '%s  %d checks, all passed.\n' "$(green 'PASS')" "$PASS"
else
  printf '%s  %d passed, %d failed. The server log is at %s\n' "$(red 'FAIL')" "$PASS" "$FAIL" "$LOG"
  exit 1
fi
rm -f "$LOG"
