#!/usr/bin/env python3
"""Fiat in and out through Stellar's test anchor, on the testnet (see try-fiat.sh). Run it in a terminal you can type in.

  1. makes a player here, with a wallet of their own on the testnet;
  2. ADD MONEY: opens the anchor's deposit screens; you fill the demo form; the anchor sends test USDC to the player's own
     address and the deposit watcher credits it;
  3. CASH OUT: opens the anchor's withdrawal screens; you fill the demo form; our worker sends the USDC to the anchor's own
     account with its memo, as an ordinary withdrawal, and the anchor completes it;
and checks the sessions, the ledger and the network at each step.
Usage: try-fiat.py API_URL API_LOG DATABASE_CONTAINER DATABASE_NAME
"""
import json, os, re, subprocess, sys, time, urllib.error, urllib.request

BASE, LOG, PG, DB = sys.argv[1:5]
HORIZON = "https://horizon-testnet.stellar.org"
UNIT = 10_000_000
RUN = str(int(time.time()))[-6:]
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


def wait(what, fn, secs=120, every=2.0):
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
        r"token ([0-9a-f]{64})", "\n".join(l for l in open(LOG) if email in l and "verify-email" in l)) or [None])[-1], 20, 1)
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


def on_chain(address):
    try:
        with urllib.request.urlopen(f"{HORIZON}/accounts/{address}") as r:
            for b in json.load(r)["balances"]:
                if b.get("asset_code") == "USDC":
                    return round(float(b["balance"]) * UNIT)
    except urllib.error.HTTPError:
        pass
    return 0


def session(p, sid):
    return call("GET", f"/v1/wallet/fiat/{sid}", p["token"])[1]


def follow(p, sid, until, secs, label):
    last = None
    def done():
        nonlocal last
        s = session(p, sid)
        view = (s["status"], s.get("anchor_status"), s.get("message"))
        if view != last:
            print(f"    {label}: {s['status']}  (anchor says {s.get('anchor_status')}){'  - ' + s['message'] if s.get('message') else ''}")
            last = view
        return s if s["status"] in until else None
    return wait(f"{label} to reach {until}", done, secs, 3)


print("== a player here, with a wallet of their own on the testnet")
ada = signup("ada")
def ready():
    s, info = call("GET", "/v1/wallet/deposit-info", ada["token"])
    w = info.get("own_wallet") or {}
    return w.get("address") if w.get("status") == "ready" else None
ada["address"] = wait("the account to be set up", ready, 120)
print(f"  address {ada['address'][:12]}...")
s, b = call("GET", "/v1/wallet/fiat", ada["token"])
ok("fiat is available and names the anchor", s == 200 and b["available"] and b["partner"] == "testanchor.stellar.org", json.dumps(b)[:120])

print("\n== ADD MONEY: the anchor's deposit screens")
s, dep = call("POST", "/v1/wallet/fiat/deposit", ada["token"], {})
ok("the API opened the anchor's deposit screens", s == 200 and dep["url"].startswith("https://"), f"{s} {str(dep)[:150]}")
print("\n  >>> Open this address in a browser, fill in the demo form (any details; amount between 1 and 10 USDC; the anchor")
print("      may ask for an email and a made-up identity), and finish. Then come back here.\n")
print(dep["url"], "\n")
input("  Enter when you have finished the form> ")
sess = follow(ada, dep["id"], ("completed", "failed", "expired"), 600, "the deposit")
ok("the anchor completed the deposit", sess["status"] == "completed", sess.get("message") or "")
got = wait("the deposit to be credited", lambda: (call("GET", "/v1/wallet/deposit-info", ada["token"]), ledger(ada))[1] or None, 180)
ok("the USDC was credited to the player's balance by the deposit watcher", got > 0, f"{units(got)} USDC")
ok("and it is on the network at the player's own address", on_chain(ada["address"]) == got, f"{units(on_chain(ada['address']))}")
before = got

print("\n== CASH OUT: the anchor's withdrawal screens")
s, wd = call("POST", "/v1/wallet/fiat/withdraw", ada["token"], {})
ok("the API opened the anchor's withdrawal screens", s == 200 and wd["url"].startswith("https://"), f"{s} {str(wd)[:150]}")
print("\n  >>> Open this address, enter an amount of at most what you added (1 USDC is fine), fill in the demo form")
print("      (any bank details) and finish. Then come back here. We will then send the USDC to the anchor ourselves.\n")
print(wd["url"], "\n")
input("  Enter when you have finished the form> ")
sess = follow(ada, wd["id"], ("sending", "completed", "failed", "expired"), 600, "the cash-out")
ok("our worker made the payment to the anchor (or it completed)", sess["status"] in ("sending", "completed"), sess.get("message") or "")
w = call("GET", f"/v1/wallet/withdrawals/{sess['withdrawal_id']}", ada["token"])[1]
note("the withdrawal", f"{w['amount']} USDC to {w['destination'][:10]}... memo {w['memo']}")
ok("it goes to an account the anchor lists, with the memo it asked for", w["memo"] is not None and w["destination"] == sql(f"SELECT destination FROM withdrawals WHERE id = '{sess['withdrawal_id']}'"))
sent = wait("the payment to be confirmed on the network", lambda: (call("GET", f"/v1/wallet/withdrawals/{sess['withdrawal_id']}", ada["token"])[1]["status"] in ("confirmed", "failed")) and call("GET", f"/v1/wallet/withdrawals/{sess['withdrawal_id']}", ada["token"])[1], 300)
ok("the payment landed on the network", sent["status"] == "confirmed", sent["status"])
note("transaction", (sent.get("attempts") or [{}])[-1].get("tx_hash", ""))
sess = follow(ada, wd["id"], ("completed", "failed", "expired"), 600, "the cash-out")
ok("the anchor completed the cash-out", sess["status"] == "completed", sess.get("message") or "")
after = ledger(ada)
paid = int(sent["amount_minor"])
ok("the player's balance went down by exactly what was sent", after == before - paid, f"{units(before)} -> {units(after)} (sent {units(paid)})")
ok("exactly one withdrawal was made for the session", sql(f"SELECT count(*) FROM withdrawals WHERE id = '{sess['withdrawal_id']}'") == "1"
   and sql("SELECT count(*) FROM withdrawals") == "1")
print("\nall checks passed")
