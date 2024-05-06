import asyncio
import json
import os

import base58
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ed25519
from py_near import transactions
from py_near.account import Account
from py_near.models import TransactionResult

ACCOUNT_ID = "nomnomnom.testnet"
PRIVATE_KEY = "ed25519:KEY_HERE"
NETWORK = "testnet"
KEYS_TO_GENERATE = 5

rpc = "https://rpc.testnet.near.org" if NETWORK == "testnet" else "https://rpc.mainnet.near.org"


def base58_encode(data):
    """Encodes data using Base58."""
    return base58.b58encode(data).decode("utf-8")


class KeyPairEd25519:
    def __init__(self):
        self._private_key_seed = os.urandom(32)
        self._private_key = ed25519.Ed25519PrivateKey.from_private_bytes(self._private_key_seed)
        self._public_key = self._private_key.public_key()

    @property
    def private_key(self):
        """Returns the Base58 encoded extended private key (seed + public key)."""
        public_key_bytes = self._public_key.public_bytes(
            encoding=serialization.Encoding.Raw, format=serialization.PublicFormat.Raw
        )
        extended_private_key = self._private_key_seed + public_key_bytes
        return "ed25519:" + base58_encode(extended_private_key)

    @property
    def public_key(self):
        """Returns the Base58 encoded public key."""
        public_key_bytes = self._public_key.public_bytes(
            encoding=serialization.Encoding.Raw, format=serialization.PublicFormat.Raw
        )
        return "ed25519:" + base58_encode(public_key_bytes)


async def main():
    actions = []
    keys = []

    # Generate keys
    for i in range(KEYS_TO_GENERATE):
        key_pair = KeyPairEd25519()
        keys.append({"account_id": ACCOUNT_ID, "public_key": key_pair.public_key, "secret_key": key_pair.private_key})
        actions.append(transactions.create_full_access_key_action(key_pair.public_key))

    # Save keys to file
    with open(f"../account_keys/{ACCOUNT_ID}.json", "w") as f:
        f.write(json.dumps(keys, indent=4))

    # Load existing account
    acc = Account(ACCOUNT_ID, PRIVATE_KEY, rpc_addr=rpc)
    await acc.startup()

    # Broadcast batch transaction with all the keys
    result: TransactionResult = await acc.sign_and_submit_tx(receiver_id=ACCOUNT_ID, actions=actions, nowait=False)
    print(f"Transaction result: {result.status}")


asyncio.run(main())
