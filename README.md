# Pagoda Relayer
At a high level, the Relayer is a http server that relays transactions to the network via RPC on behalf of new users who haven't yet acquired NEAR as part of the onboarding process.

This functionality depends on [NEP-366: Meta Transactions](https://github.com/near/NEPs/pull/366).

More technically, the end user (client) creates a `SignedDelegateAction` that contains the data necessary to construct a `Transaction`, signs the `SignedDelegateAction` using their key, which is then serialized using borsh and then json and sends that to the relayer (server) as payload of a POST request. 
When the request is received, the relayer uses its own key to sign a `Transaction` using the fields in the `SignedDelegateAction` as input to create a `SignedTransaction`. 
The `SignedTransaction` is then sent to the network via RPC call and the result is then sent back to the client.

## Setup
1. create a json file in this directory in the format `{"account_id":"example.testnet","public_key":"ed25519:98GtfFzez3opomVpwa7i4m3nptHtc7Ha514XHMWszLtQ","private_key":"ed25519:YWuyKVQHE3rJQYRC3pRGV56o1qEtA1PnMYPDEtroc5kX4A4mWrJwF7XkzGe7JWNMABbtY4XFDBJEzgLyfPkwpzC"}` using an account that has enough NEAR to cover the gas costs of transactions your server will be relaying. Usually, this will be a copy of the json file found in the `.near-credentials` directory. 
2. update values in `config.toml`
3. make sure to open up the `port` from `config.toml` in your machine's network settings
4. run the server using `cargo run`. To run with logs (tracing) enabled run `RUST_LOG=tower_http=debug cargo run`
