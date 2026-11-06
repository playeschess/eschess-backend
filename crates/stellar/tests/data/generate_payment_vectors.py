"""Writes payment_vectors.json: signed Stellar payment transactions built by the official Python SDK, so the Rust
builder in `eschess_stellar::tx` is checked against an independent implementation, byte for byte.

ed25519 signatures are deterministic, so the same inputs always give the same envelope and the file only changes when
this script does. The keys are derived from fixed text: they are throwaway test keys and hold nothing on any network.

    python3 -m venv venv && venv/bin/pip install 'stellar-sdk==12.2.0'
    venv/bin/python crates/stellar/tests/data/generate_payment_vectors.py
"""

import hashlib
import json
import os

from stellar_sdk import Account, Asset, Keypair, MuxedAccount, Network, TransactionBuilder
from stellar_sdk.memo import HashMemo, IdMemo, NoneMemo, ReturnHashMemo, TextMemo


def key(label):
    return Keypair.from_raw_ed25519_seed(hashlib.sha256(label.encode()).digest())


SOURCE = key("eschess test hot wallet")
DEST = key("eschess test player wallet")
ISSUER = key("eschess test issuer")
USDC_TESTNET_ISSUER = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5"

NETWORKS = {"testnet": Network.TESTNET_NETWORK_PASSPHRASE, "public": Network.PUBLIC_NETWORK_PASSPHRASE}


def minor_to_str(minor):
    return f"{minor // 10_000_000}.{minor % 10_000_000:07d}"


def build(case):
    asset = Asset.native() if case["asset"] == "XLM" else Asset(*case["asset"].split(":"))
    memo = {
        "none": lambda v: NoneMemo(),
        "id": lambda v: IdMemo(int(v)),
        "text": lambda v: TextMemo(v),
        "hash": lambda v: HashMemo(v),
        "return": lambda v: ReturnHashMemo(v),
    }[case["memo_type"]](case.get("memo"))
    source = Account(case["source"], case["sequence"] - 1)  # the builder uses the account's next sequence number
    tx = (
        TransactionBuilder(source, NETWORKS[case["network"]], base_fee=case["fee"])
        .add_time_bounds(case["min_time"], case["max_time"])
        .add_memo(memo)
        .append_payment_op(case["destination"], asset, minor_to_str(case["amount_minor"]))
        .build()
    )
    tx.sign(SOURCE)
    return {**case, "hash": tx.hash_hex(), "envelope": tx.to_xdr()}


base = {
    "network": "testnet",
    "source": SOURCE.public_key,
    "secret": SOURCE.secret,
    "sequence": 4_294_967_297,
    "fee": 100,
    "min_time": 0,
    "max_time": 1_700_000_120,
    "destination": DEST.public_key,
    "asset": f"USDC:{USDC_TESTNET_ISSUER}",
    "amount_minor": 25_000_000,
    "memo_type": "none",
}

cases = [
    {**base, "name": "usdc to a plain account, no memo"},
    {**base, "name": "usdc with an id memo (an exchange deposit)", "memo_type": "id", "memo": "18446744073709551615"},
    {**base, "name": "usdc with a text memo", "memo_type": "text", "memo": "withdrawal 0190f3a2"},
    {**base, "name": "usdc with a hash memo", "memo_type": "hash", "memo": "ab" * 32},
    {**base, "name": "usdc with a return memo", "memo_type": "return", "memo": "cd" * 32},
    {**base, "name": "xlm, the native asset", "asset": "XLM", "amount_minor": 1},
    {
        **base,
        "name": "to a muxed address",
        "destination": MuxedAccount(DEST.public_key, 9_223_372_036_854_775_808).account_muxed,
    },
    {**base, "name": "a 12-character asset code", "asset": f"LONGASSETCOD:{ISSUER.public_key}"},
    {**base, "name": "a 5-character asset code", "asset": f"EURCX:{ISSUER.public_key}"},
    {**base, "name": "the largest amount", "amount_minor": 2**63 - 1},
    {**base, "name": "higher fee, a min_time and a large sequence", "fee": 12_345, "min_time": 1_699_999_000,
     "sequence": 2**62 + 7},
    {**base, "name": "the public network gives a different hash", "network": "public"},
]

out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "payment_vectors.json")
with open(out, "w") as f:
    json.dump({"generated_by": "stellar-sdk 12.2.0 (Python)", "cases": [build(c) for c in cases]}, f, indent=1)
    f.write("\n")
print(f"wrote {len(cases)} cases to {out}")
