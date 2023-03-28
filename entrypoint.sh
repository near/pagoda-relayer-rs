#!/bin/bash
cat <<EOF > ./config.toml
network = "${NETWORK:-testnet}"
ip_address = [0, 0, 0, 0]
port = ${SERVER_PORT:-3030}
relayer_account_id = "${RELAYER_ACCOUNT_ID}"
keys_filename = "./${RELAYER_ACCOUNT_ID}.json"
EOF

cat <<EOF > ./${RELAYER_ACCOUNT_ID}.json
{"account_id":"${RELAYER_ACCOUNT_ID}","public_key":"${PUBLIC_KEY}","private_key":"${PRIVATE_KEY}"}
EOF

exec /relayer-app/relayer --config config.toml 