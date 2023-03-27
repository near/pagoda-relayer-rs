#!/bin/bash
cat <<EOF > ./config.toml
network = "${NETWORK:-testnet}"
ip_address = [127,0,0,1]
port = "${SERVER_PORT:-3030}"
relayer_account_id = "${RELAYER_ACCOUNT_ID}"
keys_filename = "./account_key.json"
EOF

cat <<EOF > ./relayer/account_key.json
{"account_id":"${RELAYER_ACCOUNT_ID}","public_key":"${PUBLIC_KEY}","private_key":"${PRIVATE_KEY}"}
EOF

exec /relayer --config /relayer/config.toml