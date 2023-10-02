# Example Configs 
This directory contains example configs corresponding to different use cases. 

Please note that these are for reference only and you should be updating the values in the `config.toml` file found in the `pagoda-relayer-rs` directory.

## Configs and Usecases
- `basic_whitelist.toml`
  - This is a config for a basic relayer that covers gas for user transactions to interact with a whitelisted set of contracts
- `redis.toml`
  - This is a config for a relayer that covers gas for user transactions up to a allowance specified in Redis to interact with a whitelisted set of contracts. 
  - Allowances are on a per-account id basis and on signup (account creation in redis and on-chain) an oauth token is required to help with sybil resistance
- 