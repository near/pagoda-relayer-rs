# Pagoda Relayer

## What is a Relayer?
At a high level, the Relayer is a http server that relays transactions to the NEAR network via RPC on behalf of new users who haven't yet acquired NEAR as part of the onboarding process. The entity running the relayer covers the gas costs for the end users who are signing the transactions.

## How does a Relayer work?
This functionality depends on [NEP-366: Meta Transactions](https://github.com/near/NEPs/pull/366).

Technically, the end user (client) creates a `SignedDelegateAction` that contains the data necessary to construct a `Transaction`, signs the `SignedDelegateAction` using their key, which is then serialized and sent  to the relayer (server) as payload of a POST request. 
When the request is received, the relayer uses its own key to sign a `Transaction` using the fields in the `SignedDelegateAction` as input to create a `SignedTransaction`. 
The `SignedTransaction` is then sent to the network via RPC call and the result is then sent back to the client.

## Why use a Relayer?
1. Your users are new to NEAR and don't have any gas to cover transactions 
2. Your users have an account on NEAR, but only have a Fungible Token Balance. They can now use the FT to pay for gas
3. As an enterprise or a large startup you want to seamlessly onboard your existing users onto NEAR without needing them to worry about gas costs and seed phrases
4. As an enterprise or large startup you have a userbase that can generate large spikes of user activity that would congest the network. In this case, the relayer acts as a queue for low urgency transactions  
5. In exchange for covering the gas fee costs, relayer operators can limit where users spend their assets while allowing users to have custody and ownership of their assets 

## Features 
NOTE: These features can be mixed and matched "à la carte". Use of one feature does not preclude the use of any other feature unless specified.
1. Cover the gas costs of end users while allowing them to maintain custody of their funds and approve transactions (`/relay`, `/send_meta_tx`)
2. Only pay for users interacting with certain contracts by whitelisting contracts addresses (`whitelisted_contracts` in `config.toml`) 
3. Specify gas cost allowances for all accounts (`/update_all_allowances`) or on a per-user account basis (`/create_account_atomic`, `/register_account`, `/update_allowance`) and keep track of allowances (`/get_allowance`)
4. Specify the accounts for which the relayer will cover gas fees (`whitelisted_delegate_action_receiver_ids` in `config.toml`)
5. Only allow users to register if they have a unique Oauth Token (`/create_account_atomic`, `/register_account`)
6. Relayer Key Rotation: `keys_filenames` in `config.toml`

### Features - COMING SOON
1. Allow users to pay for gas fees using Fungible Tokens they hold. This can be implemented by either:
   1. Swapping the FT for NEAR using a DEX like [Ref finance](https://app.ref.finance/) OR
   2. Sending the FT to a burn address that is verified by the relayer and the relayer covers the equivalent amount of gas in NEAR
2. Cover storage deposit costs by deploying a storage contract
3. Configure via to config use relayer without storage contract
4. automated relayer funds "top up" service
5. Put transactions in a queue to minimize network congestion - expected early-mid 2024
6. Multichain relayers - expected early-mid 2024

## API Spec <a id="api_spc"></a>
TODO - openapi and swagger docs on github pages 
- POST `/relay`
- POST `/send_meta_tx`
- GET `/get_allowance`
- POST `/update_allowance`
- POST `/update_all_allowances`
- POST `/create_account_atomic`
- POST `/register_account`

## Basic Setup - Local Dev
1. [Install Rust for NEAR Development](https://docs.near.org/sdk/rust/get-started)
2. If you don't have a NEAR account, [create one](https://docs.near.org/concepts/basics/accounts/creating-accounts)
3. With the account from step 2, create a json file in this directory in the format `{"account_id":"example.testnet","public_key":"ed25519:98GtfFzez3opomVpwa7i4m3nptHtc7Ha514XHMWszLtQ","private_key":"ed25519:YWuyKVQHE3rJQYRC3pRGV56o1qEtA1PnMYPDEtroc5kX4A4mWrJwF7XkzGe7JWNMABbtY4XFDBJEzgLyfPkwpzC"}` using a [Full Access Key](https://docs.near.org/concepts/basics/accounts/access-keys#key-types) from an account that has enough NEAR to cover the gas costs of transactions your server will be relaying. Usually, this will be a copy of the json file found in the `.near-credentials` directory. 
4. Update values in `config.toml`
5. Open up the `port` from `config.toml` in your machine's network settings
6. Run the server using `cargo run`. To run with logs (tracing) enabled run `RUST_LOG=tower_http=debug cargo run`

## Redis Setup - OPTIONAL 
NOTE: this is only needed if you intend to use whitelisting, allowances, and oauth functionality

1. [Install redis](https://redis.io/docs/getting-started/installation/). Steps 2 & 3 assume redis installed on machine instead of docker setup. If you're connecting to a redis instance running in gcp, follow the above steps to connect to a vm that will forward requests from your local relayer server to redis running in gcp: https://cloud.google.com/memorystore/docs/redis/connect-redis-instance#connecting_from_a_local_machine_with_port_forwarding
2. Run `redis-server --bind 127.0.0.1 --port 6379` - make sure the port matches the `redis_url` in the `config.toml`.
3. Run `redis-cli -h 127.0.0.1 -p 6379`

## Multiple Key Generation - OPTIONAL
1. [Install NEAR CLI](https://docs.near.org/tools/near-cli#installation)
2. Make sure you're using the appropriate network: `echo $NEAR_ENV`. To change it, `export NEAR_ENV=testnet` 
3. Make sure your keys you want to use for the relayer have a `'FullAccess'` access_key by running `near keys your_relayer_account.testnet`. This is required to create more keys. You need to have access to at least 1 `'FullAccess'` access_key 
4. Generate a new implicit key: `near generate-key`
   1. This will output something like `Key pair with ed25519:GdsF992LXiwNiAGUtxL7VbcPBAckbYBZubF6fTYrVY5Q public key for an account "e05185d0de0d6e4897555a386fdd3f48508ad1cdeaebcbd1cac81c72116cc5ab"`
5. Add the newly generated key to the relayer account: `near add-key your_relayer_account.testnet ed25519:GdsF992LXiwNiAGUtxL7VbcPBAckbYBZubF6fTYrVY5Q`
   1. This will output something like: `Adding full access key = ed25519:GdsF992LXiwNiAGUtxL7VbcPBAckbYBZubF6fTYrVY5Q to your_relayer_account.testnet. Transaction Id Bur9nJxos4f5cbibYXugZQQmZ4Uo2jsHYiVUwPT7AZMG To see the transaction in the transaction explorer, please open this url in your browser https://explorer.testnet.near.org/transactions/Bur9nJxos4f5cbibYXugZQQmZ4Uo2jsHYiVUwPT7AZMG`
6. Repeat steps 4 & 5 until you have the desired number of keys. Anywhere between 5-20 full access keys added to the relayer account works for most cases. 
7. To double check your keys were successfully added to the account run `near keys your_relayer_account.testnet` again and you should see the newly added full access keys
8. Copy all the newly generated keyfiles (usually `.json` files located in` ~/.near-credentials` directory) into the `pagoda-relayer-rs` directory
9. Change all the `"account_id"`s of the keyfiles in the relayer directory from the implicit account_id (i.e. `e05185d0de0d6e4897555a386fdd3f48508ad1cdeaebcbd1cac81c72116cc5ab`) to the relayer account_id `your_relayer_account.testnet`

## Testing
1. Run unit tests with `cargo test`

## Pre-Deployment
1. Check to make sure the values in `entrypoint.sh` match what's in `config.toml`
2. Build and run your Docker image based on the `Dockerfile` ensuring your desired ports are exposed:
   1. `docker build  -t pagoda-relayer-rs:<tag> ./`
   2. `docker run -e RELAYER_ACCOUNT_ID="<account-id>" -e PUBLIC_KEY="<account-public-key>" -e PRIVATE_KEY="<account-private-key>" -e '[{"key_file_name":"<file_name1>","account_id":"<acc1>","public_key":"<pubkey1>","private_key":"<pvtkey1>"}, {"key_file_name":"<file_name2>","account_id":"<acc2>","public_key":"<pubkey2>","private_key":"<pvtkey2>"}]',-p 3030:3030 pagoda-relayer-rs-fastauth:<tag>`
3. Test the endpoints. See [API Spec](#api_spec)

## Deployment
The Relayer is best deployed in a serverless environment, such as AWS Lambda or GCP Cloud Run, to optimally manage automatic scalability and optimize cost. 

Alternatively, the relayer can be deployed on a VM instance if the expected traffic to the relayer (and thus the CPU, RAM requirements) is well known ahead of time.

For security, it is recommended that the Full Access Key credentials are stored in a Secret Manager Service. NOTE: there should not be an large amount of funds stored on the relayer account associated with the signing key. It is better to periodically "top up" the relayer account. "Top up" Service coming soon...
