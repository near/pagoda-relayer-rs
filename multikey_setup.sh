#!/bin/bash

# Prompt user for configuration values
read -p "Enter your NEAR account ID (e.g., your_account.testnet): " ACCOUNT_ID
read -p "Enter NEAR environment (testnet/mainnet): " NEAR_ENV
read -p "Enter the number of keys to generate: " KEYS_COUNT

# Set default values if input is empty
NEAR_ENV=${NEAR_ENV:-testnet}
KEYS_COUNT=${KEYS_COUNT:-5}

# Directory and file configuration
KEYS_DIR="account_keys"
KEY_FILE="${KEYS_DIR}/${ACCOUNT_ID}.json"
CONFIG_FILE="config.toml"

# Ensure NEAR environment is correctly set
export NEAR_ENV

# Check for FullAccess key
# shellcheck disable=SC2126
FULL_ACCESS_KEYS=$(near account list-keys "$ACCOUNT_ID" network-config "$NEAR_ENV" now | grep "full access" | wc -l)
if [ "$FULL_ACCESS_KEYS" -eq "0" ]; then
  echo "No FullAccess keys found for account $ACCOUNT_ID. Please add a FullAccess key before continuing."
  exit 1
fi

# Create keys directory if it doesn't exist
mkdir -p $KEYS_DIR

# Empty or create the key file
echo "[]" > "$KEY_FILE"

# Generate and add keys
# grab the relevant public, secret key info from the following lines of output:
#--------------------  Access key info ------------------
#
#Master Seed Phrase: word0 word1 word2 ...
#Seed Phrase HD Path: m/44'/397'/0'
#Implicit Account ID: ce6905ac581868701bf273986b7d29ecfa10deb7fa6718ac3a8214b7b7af93f6
#Public Key: ed25519:EtjsLWw4vVUB5Z55auabWcE5RcAj1c41j1Fgk1wYonUd
#SECRET KEYPAIR: ed25519:hidden01234
#
#--------------------------------------------------------
for ((i = 1; i <= KEYS_COUNT; i++)); do
  # Redirect stderr to stdout to capture all output
  OUTPUT=$(near account add-key "$ACCOUNT_ID" grant-full-access autogenerate-new-keypair print-to-terminal network-config "$NEAR_ENV" sign-with-keychain send 2>&1)

  # Now OUTPUT should contain all the command output, including what was sent to stderr
  echo "OUTPUT: $OUTPUT" # Debugging line to verify output

  # TODO PUBLIC_KEY and SECRET_KEY are empty - FIX
  # Extract Public Key using grep and sed for more robust parsing
  PUBLIC_KEY=$(echo "$OUTPUT" | grep -o 'Public Key: \K.*')
  echo PUBLIC_KEY $PUBLIC_KEY

  # Extract SECRET KEYPAIR using grep and sed, ensuring we get everything after the colon
  SECRET_KEY=$(echo "$OUTPUT" | grep -o 'SECRET KEYPAIR: \K.*')
  echo SECRET_KEY $SECRET_KEY

  JSON_ENTRY="{\"account_id\":\"$ACCOUNT_ID\", \"public_key\":\"$PUBLIC_KEY\", \"secret_key\":\"$SECRET_KEY\"}"
  jq ". += [$JSON_ENTRY]" "$KEY_FILE" > tmp.$$.json && mv tmp.$$.json "$KEY_FILE"
done




# Update config.toml
if grep -q "keys_filename" $CONFIG_FILE; then
  sed -i "/keys_filename/c\keys_filename = \"$KEY_FILE\"" $CONFIG_FILE
else
  echo "keys_filename = \"$KEY_FILE\"" >> $CONFIG_FILE
fi

echo "All keys have been successfully generated and saved to $KEY_FILE."
