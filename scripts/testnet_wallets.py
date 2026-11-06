"""Creates (or checks) the Stellar TESTNET wallets for trying deposits and withdrawals end to end.

    python3 -m venv ~/.eschess-testnet/venv && ~/.eschess-testnet/venv/bin/pip install 'stellar-sdk==12.2.0'
    ~/.eschess-testnet/venv/bin/python scripts/testnet_wallets.py

Two wallets, each funded with test XLM by friendbot and given a trustline to Circle's testnet USDC:

  hot       the platform wallet: STELLAR_HOT_ACCOUNT, and its key file is STELLAR_HOT_SECRET_FILE
  external  stands in for a player's own wallet: it deposits into eSchess and receives withdrawals

The secret keys are written to ~/.eschess-testnet/<name>.secret, readable only by you, and never printed. Running it
again reuses the keys and only does what is missing. TESTNET ONLY: these keys are disposable and must never hold anything
real. Nothing here touches the public network.
"""

import os
import sys
import time

import requests
from stellar_sdk import Asset, Keypair, Network, Server, TransactionBuilder
from stellar_sdk.exceptions import NotFoundError

HORIZON = "https://horizon-testnet.stellar.org"
USDC = Asset("USDC", "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5")
DIR = os.path.expanduser("~/.eschess-testnet")
server = Server(HORIZON)


def keypair(name):
    path = os.path.join(DIR, f"{name}.secret")
    if os.path.exists(path):
        with open(path) as f:
            return Keypair.from_secret(f.read().strip()), path
    kp = Keypair.random()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(kp.secret + "\n")
    return kp, path


def account(public):
    try:
        return server.accounts().account_id(public).call()
    except NotFoundError:
        return None


def ensure(name):
    kp, path = keypair(name)
    if account(kp.public_key) is None:
        r = requests.get("https://friendbot.stellar.org", params={"addr": kp.public_key}, timeout=60)
        if r.status_code != 200:
            sys.exit(f"friendbot could not fund {name}: {r.status_code} {r.text[:200]}")
        time.sleep(2)
    acc = account(kp.public_key)
    trusted = any(
        b.get("asset_code") == USDC.code and b.get("asset_issuer") == USDC.issuer for b in acc["balances"]
    )
    if not trusted:
        tx = (
            TransactionBuilder(server.load_account(kp.public_key), Network.TESTNET_NETWORK_PASSPHRASE, base_fee=1000)
            .append_change_trust_op(USDC)
            .set_timeout(120)
            .build()
        )
        tx.sign(kp)
        server.submit_transaction(tx)
        acc = account(kp.public_key)
    balances = {
        ("XLM" if b["asset_type"] == "native" else b["asset_code"]): b["balance"] for b in acc["balances"]
    }
    print(f"{name:9} {kp.public_key}  XLM {balances.get('XLM')}  USDC {balances.get('USDC')}  key: {path}")
    return kp.public_key


if __name__ == "__main__":
    os.makedirs(DIR, mode=0o700, exist_ok=True)
    hot = ensure("hot")
    external = ensure("external")
    print()
    print("In .env:")
    print(f"  STELLAR_HOT_ACCOUNT={hot}")
    print(f"  STELLAR_HOT_SECRET_FILE={os.path.join(DIR, 'hot.secret')}")
    print()
    print(f"Test USDC: at https://faucet.circle.com choose Stellar testnet and send to the external wallet:\n  {external}")
