use crate::{get_redis_cnxn, Allowances, YN_TO_GAS};
use near_primitives::types::AccountId;
use near_primitives::views::ExecutionOutcomeWithIdView;
use r2d2::Pool;
use r2d2_redis::{
    redis::{Commands, RedisError},
    RedisConnectionManager,
};
use tracing::debug;

pub async fn set_account_and_allowance_in_redis(
    redis_pool: &Pool<RedisConnectionManager>,
    account_id: &str,
    allowance_in_gas: &u64,
) -> Result<(), RedisError> {
    // Get a connection from the REDIS_POOL
    let mut conn = get_redis_cnxn(redis_pool).await?;

    // Save the allowance information to Redis
    conn.set(account_id, allowance_in_gas.clone())?;
    Ok(())
}

pub async fn get_oauth_token_in_redis(
    redis_pool: &Pool<RedisConnectionManager>,
    oauth_token: &str,
) -> Result<bool, RedisError> {
    // Get a connection from the REDIS_POOL
    let mut conn = get_redis_cnxn(redis_pool).await?;
    let is_already_used_option: Option<bool> = conn.get(oauth_token.to_owned())?;

    match is_already_used_option {
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

pub async fn set_oauth_token_in_redis(
    redis_pool: &Pool<RedisConnectionManager>,
    oauth_token: String,
) -> Result<bool, RedisError> {
    // Get a connection from the REDIS_POOL
    let mut conn = get_redis_cnxn(redis_pool).await?;

    // Save the allowance information to Relayer DB
    let old_value: Option<bool> = conn.getset(&oauth_token, true)?;
    Ok(old_value.unwrap_or(false))
}

pub async fn get_remaining_allowance(
    redis_pool: &Pool<RedisConnectionManager>,
    account_id: &AccountId,
) -> Result<u64, RedisError> {
    // Destructure the Extension and get a connection from the connection manager
    let mut conn = get_redis_cnxn(redis_pool).await?;
    let allowance: Option<u64> = conn.get(account_id.as_str())?;
    let Some(remaining_allowance) = allowance else {
        return Ok(0);
    };
    debug!("get remaining allowance for account: {account_id}, {remaining_allowance}");
    Ok(remaining_allowance)
}

// fn to update allowance in redis when getting the receipts back and deduct the gas used
// Migrate to Allowances and use redis's decr function
pub async fn update_remaining_allowance(
    allowances: &Allowances,
    account_id: &AccountId,
    gas_used_in_yn: u128,
    allowance: u64,
) -> Result<u64, RedisError> {
    let gas_used: u64 = (gas_used_in_yn / YN_TO_GAS) as u64;
    let remaining_allowance = allowance - gas_used;
    allowances
        .set(account_id.clone(), remaining_allowance)
        .await?;
    Ok(remaining_allowance.clone())
}

pub fn calculate_total_gas_burned(
    transaction_outcome: ExecutionOutcomeWithIdView,
    execution_outcome: Vec<ExecutionOutcomeWithIdView>,
) -> u128 {
    let mut total_tokens_burnt_in_yn: u128 = 0;
    total_tokens_burnt_in_yn += transaction_outcome.outcome.tokens_burnt;

    let exec_outcome_sum: u128 = execution_outcome
        .iter()
        .map(|ro| &ro.outcome.tokens_burnt)
        .sum();
    total_tokens_burnt_in_yn += exec_outcome_sum;

    total_tokens_burnt_in_yn
}
