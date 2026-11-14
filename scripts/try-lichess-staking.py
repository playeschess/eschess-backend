#!/usr/bin/env python3
"""Staked games on the real lichess.org with testnet money (ADR 0005). Run it with try-lichess-staking.sh in a terminal you can type in.

Two people each play one Lichess account. What it does:
  1. makes two players here and puts 1 testnet USDC in each player's own wallet;
  2. links each player to a Lichess account by the real Lichess login (you paste the address your browser ends up on);
  3. switches staked games to be played on Lichess, then makes a staked game (0.5 USDC each, 3+2 clock);
  4. waits for the worker to make the game on Lichess, tells you where to play, and checks that Lichess's game id is the one
     we hold, that the moves and clocks were imported and that the pot went where the result says;
  5. (if the no-show window is short, or NOSHOW=1) makes a second staked game that nobody starts, and checks it ends as a
     draw with the stakes returned.
Usage: try-lichess-staking.py API_URL API_LOG DATABASE_CONTAINER DATABASE_NAME WINDOW_SECS
"""
import json, os, re, subprocess, sys, time, urllib.error, urllib.parse, urllib.request

BASE, LOG, PG, DB, WINDOW = sys.argv[1:6]
WINDOW = int(WINDOW)
HORIZON = "https://horizon-testnet.stellar.org"
LICHESS = "https://lichess.org"
ISSUER = os.environ["STELLAR_USDC_ISSUER"]
RUN = str(int(time.time()))[-6:]
UNIT = 10_000_000
STAKE = UNIT // 2
PASSWORD = "a-long-test-password-1"
UA = {"User-Agent": "eschess-integration-trial"}


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
        try:
            return e.code, json.loads(text) if text else {}
        except ValueError:
            return e.code, {"raw": text}


def sql(query):
    out = subprocess.run(["docker", "exec", PG, "psql", "-U", "eschess", "-d", DB, "-At", "-c", query],
                         capture_output=True, text=True, check=True)
    return out.stdout.strip()


def wait(what, fn, secs=120, every=1.0):
    end = time.time() + secs
    while time.time() < end:
        v = fn()
        if v:
            return v
        time.sleep(every)
    raise SystemExit(f"timed out waiting for {what}")


def ok(label, cond, extra=""):
    print(("  ok   " if cond else "  FAIL ") + label + (f"  [{extra}]" if extra else ""))
    if not cond:
        sys.exit(1)


def note(label, extra=""):
    print(f"  note {label}" + (f"  [{extra}]" if extra else ""))


def units(minor):
    return f"{minor / UNIT:g}"


def signup(name):
    email = f"{name}.{RUN}@example.com"
    s, b = call("POST", "/v1/auth/signup", body={"email": email, "username": f"{name}{RUN}", "password": PASSWORD})
    assert s == 201, (s, b)
    token = wait("the verification token", lambda: (re.findall(
        r"token ([0-9a-f]{64})", "\n".join(l for l in open(LOG) if email in l and "verify-email" in l)) or [None])[-1], 20)
    s, b = call("POST", "/v1/auth/email/verify", body={"token": token})
    assert s == 200, (s, b)
    s, b = call("POST", "/v1/auth/login", body={"email": email, "password": PASSWORD})
    assert s == 200, (s, b)
    return {"name": f"{name}{RUN}", "token": b["access_token"], "id": sql(f"SELECT id FROM users WHERE email = '{email}'")}


def ledger(p):
    s, b = call("GET", "/v1/me", p["token"])
    for w in b["wallet"]:
        if w["asset_id"].startswith("USDC"):
            return int(w["amount_minor"])
    return 0


def send_usdc(to, amount):
    from stellar_sdk import Keypair, Server, TransactionBuilder, Asset, Network
    kp = Keypair.from_secret(open(os.path.expanduser("~/.eschess-testnet/hot.secret")).read().strip())
    server = Server(HORIZON)
    tx = (TransactionBuilder(server.load_account(kp.public_key), Network.TESTNET_NETWORK_PASSPHRASE, base_fee=1000)
          .append_payment_op(to, Asset("USDC", ISSUER), amount).set_timeout(60).build())
    tx.sign(kp)
    return server.submit_transaction(tx)["hash"]


def lichess_game(gid):
    """What Lichess says about the game (no login needed), or None while it is only a challenge."""
    req = urllib.request.Request(f"{LICHESS}/game/export/{gid}?moves=true&tags=false&clocks=true&evals=false",
                                 headers={**UA, "Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise


def link(label, p):
    s, b = call("POST", "/v1/lichess/link/start", p["token"], {})
    assert s == 200, (s, b)
    print(f"\n  >>> {label}: open this address in a browser where you are logged in at Lichess as the account for {label}.")
    print("      Click 'Authorize'. Your browser will then try to open http://localhost:3000/lichess?code=... and show an error:")
    print("      that is expected. Copy the WHOLE address from the address bar and paste it here.\n")
    print(b["authorize_url"], "\n")
    while True:
        pasted = input("  paste the address> ").strip()
        q = urllib.parse.parse_qs(urllib.parse.urlparse(pasted).query)
        if "code" in q and "state" in q:
            break
        print("  that address has no code and state; try again (or the login was refused)")
    s, b = call("POST", "/v1/lichess/link/finish", p["token"], {"code": q["code"][0], "state": q["state"][0]})
    ok(f"{label} is linked to a Lichess account", s == 200 and b.get("linked"), json.dumps(b)[:200])
    p["lichess"] = b["linked"]["lichess_id"]
    print(f"      linked: {b['linked']['username']}")


def new_game(white, black):
    s, g = call("POST", "/v1/games", white["token"],
                {"time_control": {"base_seconds": 180, "increment_seconds": 2}, "color": "white", "rated": True, "stake": "0.5"})
    assert s == 201, (s, g)
    gid = g["id"]
    s, b = call("POST", f"/v1/games/{gid}/join", black["token"], {})
    assert s == 200, (s, b)
    ok("both stakes are held in the ledger", ledger(white) < UNIT or ledger(black) < UNIT, f"{units(ledger(white))} / {units(ledger(black))}")
    ok("the game is a Lichess game and cannot be played here",
       sql(f"SELECT venue FROM games WHERE id = '{gid}'") == "lichess")
    s, b = call("POST", f"/v1/games/{gid}/moves", white["token"], {"uci": "e2e4", "client_move_id": "00000000-0000-4000-8000-000000000001"})
    ok("our server refuses moves for it", s == 409 and (b.get("error") or {}).get("code") == "played_on_lichess", f"{s} {b}")
    lid = wait("the worker to make the game on Lichess", lambda: sql(f"SELECT lichess_game_id FROM games WHERE id = '{gid}'") or None, 90, 2)
    ok("the worker made a game on Lichess", len(lid) == 8, lid)
    return gid, lid


def status_of(gid):
    return sql(f"SELECT status || ' ' || coalesce(result, '-') || ' ' || ply || ' ' || coalesce(end_reason, '-') FROM games WHERE id = '{gid}'")


print("== two players here, and testnet money")
p1, p2 = signup("white"), signup("black")
for p in (p1, p2):
    def ready(p=p):
        s, info = call("GET", "/v1/wallet/deposit-info", p["token"])
        w = info.get("own_wallet") or {}
        return w.get("address") if w.get("status") == "ready" else None
    p["address"] = wait(f"{p['name']}'s account to be set up", ready, 120, 2)
    send_usdc(p["address"], "1")
for p in (p1, p2):
    wait(f"{p['name']}'s deposit", lambda p=p: (call("GET", "/v1/wallet/deposit-info", p["token"]), ledger(p) == UNIT)[1], 120, 2)
ok("both players hold 1 USDC", True)

print("\n== link a Lichess account to each player (the real Lichess login)")
link("player 1 (will play White)", p1)
link("player 2 (will play Black)", p2)
ok("two different Lichess accounts", p1["lichess"] != p2["lichess"], f"{p1['lichess']} / {p2['lichess']}")

sql("UPDATE platform_settings SET value = 'lichess' WHERE name = 'game_venue'")

print("\n== game A: played on Lichess")
gid, lid = new_game(p1, p2)
print(f"\n  >>> The game exists on Lichess: {LICHESS}/game/{lid}")
print(f"      player 1 ({p1['lichess']}) opens it logged in as that account; player 2 ({p2['lichess']}) opens it logged in as theirs.")
print("      Only those two can join. Play at least 10 moves each, then finish it any way you like")
print("      (resign, checkmate, run out of time). Press Enter here once BOTH have joined and the game has started.")
input("  Enter> ")
lg = lichess_game(lid)
if lg is None:
    note("Lichess has no game with this id yet", "either it has not started, or a challenge's id is NOT its game's id")
else:
    ok("Lichess's game id is the id we hold (the challenge id became the game id)", lg["id"] == lid, lg["id"])
    names = [lg["players"][c].get("user", {}).get("id") for c in ("white", "black")]
    ok("White is player 1's account and Black is player 2's", names == [p1["lichess"], p2["lichess"]], str(names))
    ok("standard, casual, the clock we asked for", (lg["variant"], lg["rated"], lg["clock"]["initial"], lg["clock"]["increment"]) == ("standard", False, 180, 2),
       str((lg["variant"], lg["rated"], lg.get("clock"))))
print("  Waiting for the game to finish (up to 40 minutes)...")
last = ""
end = time.time() + 2400
while time.time() < end:
    st = status_of(gid)
    if st != last:
        print("  our record:", st)
        last = st
    if st.startswith("finished"):
        break
    time.sleep(10)
else:
    raise SystemExit("the game did not finish in time")
status, result, ply, reason = st.split(" ")
ply = int(ply)
lg = lichess_game(lid)
ok("Lichess shows the game over", lg is not None and lg["status"] not in ("created", "started"), lg and lg["status"])
winner = lg.get("winner")
ok("our result is Lichess's result", result == (winner or "draw"), f"ours {result}, Lichess {winner or 'draw'} ({lg['status']})")
ok("the number of moves is Lichess's", ply == len(lg["moves"].split()), f"{ply}")
n = int(sql(f"SELECT count(*) FROM game_moves WHERE game_id = '{gid}'"))
ok("the moves were imported for fair play", n == ply, f"{n} of {ply}")
think = sql(f"SELECT string_agg(charged_ms::text, ' ' ORDER BY ply) FROM (SELECT * FROM game_moves WHERE game_id = '{gid}' AND ply > 2 ORDER BY ply LIMIT 8) m")
note("think times (ms) of plies 3 to 10, from Lichess's clocks: do they look like the game you played?", think)
runs = wait("the private scorer", lambda: sql(f"SELECT string_agg(scorer || ':' || outcome, ',') FROM fairplay_runs WHERE game_id = '{gid}'") or None, 60, 2)
ok("the fair-play scorer looked at the game", "baseline-1" in runs, runs)
scores = sql(f"SELECT coalesce(string_agg(score::text, ' '), 'none') FROM fairplay_scores WHERE game_id = '{gid}'")
note("fair-play scores", scores)
cases = sql("SELECT count(*) FROM fairplay_cases WHERE status = 'open'")
if cases != "0":
    note("a fair-play case is open, so the pot is held until a reviewer decides", "stopping here")
    sys.exit(0)
wait("settlement", lambda: sql(f"SELECT settled_at IS NOT NULL FROM games WHERE id = '{gid}'") == "t", 60, 2)
a, b = ledger(p1), ledger(p2)
if ply < 4 or result == "draw":
    expect = (UNIT, UNIT)
elif result == "white":
    expect = (UNIT + STAKE, UNIT - STAKE)
else:
    expect = (UNIT - STAKE, UNIT + STAKE)
ok("the pot went where the result says", (a, b) == expect, f"player 1 {units(a)}, player 2 {units(b)}; result {result}, {ply} plies")
ok("settled exactly once, and nothing left in escrow",
   sql("SELECT count(*) FROM ledger_transactions WHERE kind = 'stake_settle'") == "1"
   and sql("SELECT COALESCE(sum(b.balance_minor), 0) FROM ledger_accounts a JOIN ledger_balances b ON b.account_id = a.id WHERE a.kind = 'stake_escrow'") == "0")

if WINDOW <= 180 or os.environ.get("NOSHOW") == "1":
    print(f"\n== game B: nobody starts it (the no-show window is {WINDOW}s)")
    gid2, lid2 = new_game(p1, p2)
    print(f"  >>> Do NOT open {LICHESS}/game/{lid2}. Waiting for the challenge to expire (about {WINDOW + 90}s)...")
    wait("the no-show", lambda: status_of(gid2).startswith("finished"), WINDOW + 240, 5)
    st = status_of(gid2)
    ok("it ended as a draw with no moves, as a no-show", st == "finished draw 0 lichess:no_show", st)
    wait("settlement", lambda: sql(f"SELECT settled_at IS NOT NULL FROM games WHERE id = '{gid2}'") == "t", 60, 2)
    ok("both stakes came back", (ledger(p1), ledger(p2)) == (a, b), f"{units(ledger(p1))} / {units(ledger(p2))}")
else:
    note("skipped the no-show game (the window is long); rerun with LICHESS_WINDOW=60, or NOSHOW=1 to wait it out")
print("\nall checks passed")
