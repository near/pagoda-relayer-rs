# Example Configs 
This directory contains example configs corresponding to different use cases. 

Please note that these are for reference only and you should be updating the values in the `config.toml` file found in the `pagoda-relayer-rs` directory.

## Configs and Usecases
- `no_filters.toml`
  - This is a config for a relayer that covers gas for all user transactions to all contracts with no filters. To prevent abuse, this should only be used if there's only a secure backend calling the relayer
- `basic_whitelist.toml`
  - This is a config for a basic relayer that covers gas for user transactions to interact with a whitelisted set of contracts
- `redis.toml`
  - This is a config for a relayer that covers gas for user transactions up to a allowance specified in Redis to interact with a whitelisted set of contracts. 
  - Allowances are on a per-account id basis and on signup (account creation in redis and on-chain) an oauth token is required to help with sybil resistance
- `fastauth.toml`
  - This is a config for use if you intend to integrate with [fastuath sdk](https://docs.near.org/tools/fastauth-sdk)
  - It covers gas for user transactions up to a allowance specified in Redis to interact with a whitelisted set of contracts. 
  - Allowances are on a per-account id basis and on signup (account creation in redis and on-chain) an oauth token is required to help with sybil resistance 
  - This also makes use of a shared storage functionality on the near social db contract 
  - and a whitelisted sender (whitelisted_delegate_action_receiver_ids)
- `pay_with_ft.toml` 
  - This is a config for a relayer that ensures there's FTs sent to a burn address used to cover the equivalent amount of gas for user transactions to interact with a whitelisted set of contracts 
- `whitelist_senders.toml` (whitelisted_delegate_action_receiver_ids)
  - This is a config for a relayer that covers gas for a whitelisted set of users' transactions to interact with a whitelisted set of contracts
- `shared_storage.toml`
  - This is a config for a relayer that covers BOTH gas AND storage fees for user transactions to interact with a whitelisted set of contracts
  - be sure to include shared storage logic based on https://github.com/NearSocial/social-db/blob/master/contract/src/shared_storage.rs in your contract that is being whitelisted
- `exchange_withdraw.toml` 
  - This is a config for a relayer where an exchange running the relayer covers user withdraw fees when they are withdrawing stablecoins on NEAR (ie USDT or USDC)
