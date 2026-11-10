#!/usr/bin/env python3
"""Staked games through the escrow contract, end to end, on the Stellar testnet (see try-contract-staking.sh).

Real processes (API, worker, the private fair-play worker), real testnet USDC, the real contract. Three games between two
players, each staking 0.5 USDC:
  1. a win by checkmate           -> the contract pays the winner the whole pot
  2. a draw by agreement          -> each player gets their own stake back
  3. a long game at a perfectly even pace that White wins, which the fair-play scorer flags for both players; a reviewer
     confirms White's case and clears Black's -> the pot goes to Black although White won the game (the owner's rule)
and then checks the balances on the network and in the ledger, the books against the chain, and that every game settled once.

Usage: try-contract-staking.py API_URL API_LOG DATABASE_CONTAINER DATABASE_NAME  (needs the `chess` package to make the long game)
"""
import json, os, re, subprocess, sys, time, urllib.error, urllib.request, uuid

BASE, LOG, PG, DB = sys.argv[1:5]
HORIZON = "https://horizon-testnet.stellar.org"
ISSUER = os.environ["STELLAR_USDC_ISSUER"]
RUN = str(int(time.time()))[-6:]
UNIT = 10_000_000  # USDC has seven decimals
STAKE = UNIT // 2  # 0.5 USDC
PASSWORD = "a-long-test-password-1"


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


def long_game(plies=55, seed=7):
    """A legal game of `plies` half-moves that does not end or repeat early (made with the `chess` package)."""
    import random
    import chess
    rng = random.Random(seed)
    board, moves = chess.Board(), []
    while len(moves) < plies:
        legal = list(board.legal_moves)
        rng.shuffle(legal)
        for m in legal:
            board.push(m)
            if board.is_game_over() or board.can_claim_threefold_repetition() or board.is_repetition(2):
                board.pop()
                continue
            moves.append(m.uci())
            break
        else:
            raise SystemExit("could not make a long game")
    return moves


def ledger(p):
    s, b = call("GET", "/v1/me", p["token"])
    for w in b["wallet"]:
        if w["asset_id"].startswith("USDC"):
            return int(w["amount_minor"])
    return 0


def on_chain(address):
    with urllib.request.urlopen(f"{HORIZON}/accounts/{address}") as r:
        for b in json.load(r)["balances"]:
            if b.get("asset_code") == "USDC":
                return round(float(b["balance"]) * UNIT)
    return 0


def send_usdc(to, amount):
    from stellar_sdk import Keypair, Server, TransactionBuilder, Asset, Network
    # The testnet wallet that holds the test USDC (it stands in for a player paying from outside).
    kp = Keypair.from_secret(open(os.path.expanduser("~/.eschess-testnet/hot.secret")).read().strip())
    server = Server(HORIZON)
    tx = (TransactionBuilder(server.load_account(kp.public_key), Network.TESTNET_NETWORK_PASSPHRASE, base_fee=1000)
          .append_payment_op(to, Asset("USDC", ISSUER), amount).set_timeout(60).build())
    tx.sign(kp)
    return server.submit_transaction(tx)["hash"]


def game(token, gid):
    return call("GET", f"/v1/games/{gid}", token)[1]


def settlement(p, gid):
    return (game(p["token"], gid).get("stake") or {}).get("settlement")


def play(gid, white, black, moves, pause=0.0):
    for i, uci in enumerate(moves):
        who = white if i % 2 == 0 else black
        s, b = call("POST", f"/v1/games/{gid}/moves", who["token"], {"uci": uci, "client_move_id": str(uuid.uuid4())})
        assert s == 200, (i + 1, uci, s, b)
        if pause:
            time.sleep(pause)


def new_game(white, black):
    """White creates a staked game, Black joins; it waits in `funding` until the stakes are locked in the contract."""
    s, g = call("POST", "/v1/games", white["token"],
                {"time_control": {"base_seconds": 300, "increment_seconds": 0}, "color": "white", "rated": True, "stake": "0.5"})
    assert s == 201, (s, g)
    gid = g["id"]
    s, b = call("POST", f"/v1/games/{gid}/join", black["token"], {})
    assert s == 200, (s, b)
    ok("the game waits while the stakes are locked in the contract", b["status"] == "funding", b["status"])
    wait("the game to start", lambda: game(white["token"], gid)["status"] == "active", 120)
    ok("the stakes were locked and the game started", True)
    return gid


def settled(p, gid):
    wait("settlement", lambda: settlement(p, gid) == "settled", 240)


print("== two players and a reviewer")
ada, grace, rev = signup("ada"), signup("grace"), signup("rev")
sql(f"INSERT INTO user_roles (user_id, role) VALUES ('{rev['id']}', 'fairplay_reviewer')")
for p in (ada, grace):
    def ready(p=p):
        s, info = call("GET", "/v1/wallet/deposit-info", p["token"])
        w = info.get("own_wallet") or {}
        return w.get("address") if w.get("status") == "ready" else None
    p["address"] = wait(f"{p['name']}'s account to be set up", ready, 120, 2)
    print(f"  {p['name']}'s own address: {p['address'][:12]}…")
    send_usdc(p["address"], "2")
for p in (ada, grace):
    def credited(p=p):
        call("GET", "/v1/wallet/deposit-info", p["token"])  # keeps the address being watched often
        return ledger(p) == 2 * UNIT
    wait(f"{p['name']}'s deposit to be credited", credited, 120, 2)
ok("both players deposited 2 USDC to their own addresses and it was credited", True)

print("== game 1: a win by checkmate")
g1 = new_game(ada, grace)
ok("a stake is held for each player in the ledger", (ledger(ada), ledger(grace)) == (2 * UNIT - STAKE, 2 * UNIT - STAKE))
play(g1, ada, grace, ["f2f3", "e7e5", "g2g4", "d8h4"])
ok("Black won by checkmate", game(ada["token"], g1)["result"] == "black")
settled(ada, g1)
ok("the pot went to the winner in the ledger", (ledger(ada), ledger(grace)) == (UNIT + UNIT // 2, 2 * UNIT + UNIT // 2),
   f"ada {units(ledger(ada))}, grace {units(ledger(grace))}")
wait("the network to show it", lambda: on_chain(grace["address"]) == 2 * UNIT + UNIT // 2, 60, 2)
ok("and on the network the winner holds the pot", (on_chain(ada["address"]), on_chain(grace["address"])) == (UNIT + UNIT // 2, 2 * UNIT + UNIT // 2))
runs = sql(f"SELECT string_agg(scorer || ':' || outcome, ',' ORDER BY scorer) FROM fairplay_runs WHERE game_id = '{g1}'")
ok("the private scorer looked at the game before the pot moved", "baseline-1:scored" in runs, runs)

print("== game 2: a draw by agreement")
g2 = new_game(ada, grace)
play(g2, ada, grace, ["e2e4", "e7e5", "g1f3", "b8c6"])
s, b = call("POST", f"/v1/games/{g2}/draw-offer", ada["token"], {})
assert s == 200, (s, b)
s, b = call("POST", f"/v1/games/{g2}/draw-offer/accept", grace["token"], {})
assert s == 200, (s, b)
ok("the game was drawn", game(ada["token"], g2)["result"] == "draw")
settled(ada, g2)
ok("each player got their own stake back", (ledger(ada), ledger(grace)) == (UNIT + UNIT // 2, 2 * UNIT + UNIT // 2))
wait("the network to show it", lambda: (on_chain(ada["address"]), on_chain(grace["address"])) == (UNIT + UNIT // 2, 2 * UNIT + UNIT // 2), 60, 2)
ok("and on the network", True)

print("== game 3: a long game at a perfectly even pace, which White wins")
moves = long_game()
g3 = new_game(ada, grace)
t0 = time.time()
play(g3, ada, grace, moves, pause=2.0)
s, b = call("POST", f"/v1/games/{g3}/resign", grace["token"], {})
assert s == 200, (s, b)
ok("White won", game(ada["token"], g3)["result"] == "white", f"{len(moves)} plies in {time.time() - t0:.0f}s")

def open_cases():
    s, b = call("GET", "/v1/admin/fairplay/cases", rev["token"])
    return b.get("cases", []) if s == 200 else []

cases = wait("the fair-play scorer to flag the game", lambda: open_cases() or None, 120, 2)
ok("fair play opened a case, so the pot is held", True, ", ".join(c["player"]["username"] for c in cases))
time.sleep(8)  # let the settle job look at it, so we can see it does not pay
ok("the pot is still held", settlement(ada, g3) == "settling")
ok("and the ledger has not moved (each stake is still held)", (ledger(ada), ledger(grace)) == (UNIT, 2 * UNIT),
   f"ada {units(ledger(ada))}, grace {units(ledger(grace))}")

print("== the reviewer confirms White's case and clears Black's")
for c in open_cases():
    detail = call("GET", f"/v1/admin/fairplay/cases/{c['id']}", rev["token"])[1]
    ids = [g["game_id"] for g in detail["game_records"]]
    if c["player"]["username"] == ada["name"]:
        s, b = call("POST", f"/v1/admin/fairplay/cases/{c['id']}/confirm", rev["token"],
                    {"reason": "perfectly even pace across the whole game", "games": ids, "sanction": {"kind": "warning"}})
    else:
        s, b = call("POST", f"/v1/admin/fairplay/cases/{c['id']}/clear", rev["token"],
                    {"reason": "nothing found against this player", "games": ids})
    assert s == 200, (s, b)
    print(f"  {c['player']['username']}: {b['status']}")
settled(ada, g3)
ok("the pot went to the honest player although White won the game",
   (ledger(ada), ledger(grace)) == (UNIT, 3 * UNIT), f"ada {units(ledger(ada))}, grace {units(ledger(grace))}")
wait("the network to show it", lambda: (on_chain(ada["address"]), on_chain(grace["address"])) == (UNIT, 3 * UNIT), 60, 2)
ok("and on the network", True)

print("== the books")
ok("every game settled exactly once", sql("SELECT count(*) FROM ledger_transactions WHERE kind = 'stake_settle'") == "3"
   and sql("SELECT count(DISTINCT reference_id) FROM ledger_transactions WHERE kind = 'stake_settle'") == "3")
ok("no stake is left in the ledger's escrow",
   sql("SELECT COALESCE(sum(b.balance_minor), 0) FROM ledger_accounts a JOIN ledger_balances b ON b.account_id = a.id WHERE a.kind = 'stake_escrow'") == "0")
ok("the contract holds no leftover match", sql("SELECT count(*) FROM games WHERE stake_minor IS NOT NULL AND settled_at IS NULL") == "0")
latest = wait("a books-against-chain check made after the last payout",
              lambda: sql("SELECT verdict || ' ' || ledger_minor || ' ' || chain_minor || ' ' || players_minor || ' ' || contract_minor "
                          "FROM money_checks WHERE checked_at > (SELECT max(settled_at) FROM games) ORDER BY id DESC LIMIT 1") or None, 150, 5)
verdict, ledger_total, network_total, players_total, contract_total = latest.split()
ok("after the last payout the books match the network", verdict == "ok" and ledger_total == network_total == str(4 * UNIT), latest)
ok("and the contract holds nothing: all of it is back in the players' own accounts",
   (contract_total, players_total) == ("0", str(4 * UNIT)), f"players {units(int(players_total))}, contract {units(int(contract_total))}")
print("all checks passed")
