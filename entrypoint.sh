#!/bin/bash
extract_key_file_names() {
  local RELAYER_KEYS="$1"
  jq -r '[.[].key_file_name + ".json"]' <<< "$RELAYER_KEYS" | tr -d '[:space:]'
}

result=$(extract_key_file_names "$RELAYER_KEYS")


parse_and_create_json_files() {
  for item in $(echo "${RELAYER_KEYS}" | jq -c '.[]'); do
    # Extract the key_file_name and data fields
    key_file_name=$(echo "${item}" | jq -r '.key_file_name')
    data=$(echo "${item}" | jq '{account_id, public_key, private_key}')

    # Create the JSON file
    echo "${data}" > "${key_file_name}.json"
done
}

# Call the function to create JSON files
parse_and_create_json_files "$RELAYER_KEYS"

cat <<EOF > ./config.toml
network = "${NETWORK}"
ip_address = [0, 0, 0, 0]
port = ${SERVER_PORT:-3030}
relayer_account_id = "${RELAYER_ACCOUNT_ID}"
keys_filenames = $(extract_key_file_names "$RELAYER_KEYS")
shared_storage_account_id = "${STORAGE_ACCOUNT_ID:-storage-acc}"
shared_storage_keys_filename = "./${STORAGE_ACCOUNT_ID}.json"
whitelisted_contracts = [${WHITELISTED_CONTRACT}]
whitelisted_delegate_action_receiver_ids = [${WHITELISTED_RECEIVER_IDS}]
redis_url = "redis://${REDIS_HOST}:6379"
override_rpc_conf = ${OVERRIDE_RPC_CONF:-false}
rpc_url = "${RELAYER_RPC_URL}"
wallet_url = "https://wallet.testnet.near.org"
explorer_transaction_url = "https://explorer.testnet.near.org/transactions/"
rpc_api_key = ""
num_keys = ${NUM_KEYS}

[social_db]
mainnet = "social.near"
testnet = "v1.social08.testnet"
custom = "${CUSTOM_SOCIAL_DB_ID}"

EOF

cat <<EOF > ./${RELAYER_ACCOUNT_ID}.json
{"account_id":"${RELAYER_ACCOUNT_ID}","public_key":"${PUBLIC_KEY}","private_key":"${PRIVATE_KEY}"}
EOF

if [ "${NETWORK}" == "mainnet" ]; then
    cat <<EOF > ./${STORAGE_ACCOUNT_ID}.json
    {"account_id":"${STORAGE_ACCOUNT_ID}","public_key":"${STORAGE_PUBLIC_KEY}","private_key":"${STORAGE_PRIVATE_KEY}"}
EOF
fi

if [ "${NETWORK}" == "custom" ]; then
cat <<EOF >./${STORAGE_ACCOUNT_ID}.json
    {"account_id":"${STORAGE_ACCOUNT_ID}","public_key":"${STORAGE_PUBLIC_KEY}","private_key":"${STORAGE_PRIVATE_KEY}"}
EOF
fi
cat config.toml

exec /relayer-app/relayer --config config.toml
