#!/usr/bin/env python3
"""A staked game, end to end, on the Stellar testnet (see the README, "Trying it on the testnet").

Run it with the testnet venv (it needs stellar-sdk) after starting, against a fresh database:
  the API (STAKES_USDC set), the public worker (SETTLE_SCORER=baseline-1) and the private fair-play worker.
It signs up two players, sends each real testnet USDC from the `external` wallet, plays two staked games (one made by
create and join, one by the matchmaking queue), and checks that each pot was paid once, to the winner, after the fair-play
worker had looked at the game. Usage: try-staked.py API_URL API_LOG DATABASE_CONTAINER
"""
import json, re, sys, time, urllib.request, urllib.error, subprocess, os, uuid

BASE, LOG, PG = sys.argv[1], sys.argv[2], sys.argv[3]
HORIZON = "https://horizon-testnet.stellar.org"
ISSUER = os.environ["STELLAR_USDC_ISSUER"]
RUN = str(int(time.time()))[-6:]
UNIT = 10_000_000  # USDC has 7 decimals


def call(method, path, token=None, body=None):
    req = urllib.request.Request(BASE + path, method=method, data=json.dumps(body).encode() if body is not None else None)
    req.add_header("content-type", "application/json")
    if token:
        req.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req) as r:
            text = r.read().decode()
            return r.status, json.loads(text) if text else {}
    except urllib.error.HTTPError as e:
        text = e.read().decode()
        return e.code, json.loads(text) if text else {}


def sql(query):
    out = subprocess.run(["docker", "exec", PG, "psql", "-U", "eschess", "-d", "staked_try", "-At", "-c", query],
                         capture_output=True, text=True, check=True)
    return out.stdout.strip()


def wait(what, fn, secs=90):
    end = time.time() + secs
    while time.time() < end:
        v = fn()
        if v:
            return v
        time.sleep(1)
    raise SystemExit(f"timed out waiting for {what}")


def signup(name):
    email = f"{name}.{RUN}@example.com"
    s, b = call("POST", "/v1/auth/signup", body={"email": email, "username": f"{name}{RUN}", "password": "a-long-test-password-1"})
    assert s == 201, (s, b)
    token = wait("the verification token", lambda: (re.findall(r"token ([0-9a-f]{64})", "\n".join(l for l in open(LOG) if email in l and "verify-email" in l)) or [None])[-1], 15)
    s, b = call("POST", "/v1/auth/email/verify", body={"token": token})
    assert s == 200, (s, b)
    return b if "access_token" in b else None, b, email


def login(email):
    s, b = call("POST", "/v1/auth/login", body={"email": email, "password": "a-long-test-password-1"})
    assert s == 200, (s, b)
    return b["access_token"]


def usdc(token):
    s, b = call("GET", "/v1/me", token)
    for w in b["wallet"]:
        if w["asset_id"].startswith("USDC"):
            return int(w["amount_minor"])
    return 0


def send_usdc(to, units):
    from stellar_sdk import Keypair, Server, TransactionBuilder, Asset, Network
    kp = Keypair.from_secret(open(os.path.expanduser("~/.eschess-testnet/external.secret")).read().strip())
    server = Server(HORIZON)
    tx = (TransactionBuilder(server.load_account(kp.public_key), Network.TESTNET_NETWORK_PASSPHRASE, base_fee=1000)
          .append_payment_op(to, Asset("USDC", ISSUER), str(units)).set_timeout(60).build())
    tx.sign(kp)
    return server.submit_transaction(tx)["hash"]


def fools_mate(white, black, game):
    for who, uci in [(white, "f2f3"), (black, "e7e5"), (white, "g2g4"), (black, "d8h4")]:
        s, b = call("POST", f"/v1/games/{game}/moves", who, {"uci": uci, "client_move_id": str(uuid.uuid4())})
        assert s == 200, (uci, s, b)


def tokens_by_name(game, tokens):
    s, g = call("GET", f"/v1/games/{game}", tokens["ada"])
    return tokens[re.sub(r"\d+$", "", g["white"]["username"])], tokens[re.sub(r"\d+$", "", g["black"]["username"])], g


def check(label, ok, extra=""):
    print(("  ok   " if ok else "  FAIL ") + label + (f"  [{extra}]" if extra else ""))
    if not ok:
        sys.exit(1)


SETTLED_BEFORE = int(sql("SELECT count(*) FROM ledger_transactions WHERE kind='stake_settle'"))
print("signing up and funding two players with real testnet USDC")
people = {}
for name in ("ada", "grace"):
    _, _, email = signup(name)
    people[name] = {"email": email, "token": login(email)}
tokens = {n: p["token"] for n, p in people.items()}
for name, p in people.items():
    s, info = call("GET", "/v1/wallet/deposit-info", p["token"])
    assert s == 200, (s, info)
    h = send_usdc(info["muxed_address"], "3")
    print(f"  sent 3 USDC to {name} ({h[:12]}…)")
for name, p in people.items():
    wait(f"{name}'s deposit", lambda: usdc(p["token"]) == 3 * UNIT)
check("both deposits were credited (3 USDC each)", True)

print("game 1: created, joined, played")
s, g = call("POST", "/v1/games", tokens["ada"], {"time_control": {"base_seconds": 300, "increment_seconds": 0}, "color": "white", "rated": True, "stake": "1"})
assert s == 201, (s, g)
g1 = g["id"]
check("the creator's stake left the wallet", usdc(tokens["ada"]) == 2 * UNIT)
s, b = call("POST", f"/v1/games/{g1}/join", tokens["grace"], {})
assert s == 200, (s, b)
check("the joiner's stake left the wallet", usdc(tokens["grace"]) == 2 * UNIT)
fools_mate(tokens["ada"], tokens["grace"], g1)
s, g = call("GET", f"/v1/games/{g1}", tokens["ada"])
check("grace won by checkmate", g["result"] == "black", g["result"])

wait("settlement", lambda: call("GET", f"/v1/games/{g1}", tokens["ada"])[1]["stake"]["settlement"] == "settled")
runs = sql(f"SELECT string_agg(scorer || ':' || outcome, ',' ORDER BY scorer) FROM fairplay_runs WHERE game_id = '{g1}'")
check("the private scorer looked at the game before the pot moved", "baseline-1:scored" in runs, runs)
check("the winner got the pot, the loser lost the stake", (usdc(tokens["grace"]), usdc(tokens["ada"])) == (4 * UNIT, 2 * UNIT),
      f"grace {usdc(tokens['grace'])/UNIT}, ada {usdc(tokens['ada'])/UNIT}")

print("game 2: found through the matchmaking queue")
s, b = call("POST", "/v1/matchmaking", tokens["ada"], {"time_control": {"base_seconds": 300, "increment_seconds": 0}, "rated": True, "stake": "1"})
assert s == 200 and b["status"] == "queued", (s, b)
check("nothing is held while queued", usdc(tokens["ada"]) == 2 * UNIT)
s, b = call("POST", "/v1/matchmaking", tokens["grace"], {"time_control": {"base_seconds": 300, "increment_seconds": 0}, "rated": True, "stake": "1"})
assert s == 200, (s, b)
# Their ratings moved apart in game 1, so the pairing window may need a few seconds of waiting to include both.
wait("the pairing", lambda: call("GET", "/v1/matchmaking", tokens["grace"])[1].get("status") == "matched", 60)
g2 = call("GET", "/v1/matchmaking", tokens["grace"])[1]["game"]
g2 = g2["id"] if isinstance(g2, dict) else g2
check("both stakes were held when they were paired", (usdc(tokens["ada"]), usdc(tokens["grace"])) == (1 * UNIT, 3 * UNIT))
white, black, g = tokens_by_name(g2, tokens)
fools_mate(white, black, g2)
wait("settlement of game 2", lambda: call("GET", f"/v1/games/{g2}", tokens["ada"])[1]["stake"]["settlement"] == "settled")
winner = "grace" if g["black"]["username"].startswith("grace") else "ada"
a, gr = usdc(tokens["ada"]), usdc(tokens["grace"])
check("the pot went to the player who mated", (a + gr) == 4 * UNIT and (gr if winner == "grace" else a) >= 3 * UNIT,
      f"ada {a/UNIT}, grace {gr/UNIT}")

print("the books")
check("escrow is empty", sql("SELECT COALESCE(sum(b.balance_minor),0) FROM ledger_accounts a JOIN ledger_balances b ON b.account_id=a.id WHERE a.kind='stake_escrow'") == "0")
check("each of the two games settled exactly once", int(sql("SELECT count(*) FROM ledger_transactions WHERE kind='stake_settle'")) - SETTLED_BEFORE == 2)
check("no game was settled twice", sql("SELECT count(*) FROM (SELECT reference_id FROM ledger_transactions WHERE kind='stake_settle' GROUP BY reference_id HAVING count(*) > 1) d") == "0")
check("no game is left unsettled", sql("SELECT count(*) FROM games WHERE stake_minor IS NOT NULL AND settled_at IS NULL") == "0")
print("all checks passed")
