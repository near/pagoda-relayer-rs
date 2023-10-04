use tracing::log::error;
use axum::http::StatusCode;
use tracing::{debug, info};
use near_primitives::types::AccountId;
use near_primitives::views::ExecutionOutcomeWithIdView;
use r2d2::PooledConnection;
use r2d2_redis::RedisConnectionManager;
use r2d2_redis::redis::{Commands, ErrorKind::IoError, RedisError};
use crate::error::RelayError;
use crate::{NETWORK_ENV, REDIS_POOL, YN_TO_GAS};

pub async fn get_redis_cnxn() -> Result<PooledConnection<RedisConnectionManager>, RedisError> {
    let conn_result = REDIS_POOL.get();
    let conn: PooledConnection<RedisConnectionManager> = match conn_result {
        Ok(conn) => conn,
        Err(e) => {
            return Err(RedisError::from((IoError,
                "Error getting Relayer DB connection from the pool",
                e.to_string(),
            )));
        }
    };
    Ok(conn)
}

pub async fn set_account_and_allowance_in_redis(
    account_id: &str,
    allowance_in_gas: &u64,
) -> Result<(), RedisError> {
    // Get a connection from the REDIS_POOL
    let mut conn = get_redis_cnxn().await?;

    // Save the allowance information to Redis
    conn.set(account_id, allowance_in_gas.clone())?;
    Ok(())
}

pub async fn get_oauth_token_in_redis(
    oauth_token: &str,
) -> Result<bool, RedisError> {
    // Get a connection from the REDIS_POOL
    let mut conn = get_redis_cnxn().await?;
    let is_already_used_option: Option<bool> = conn.get(oauth_token.to_owned())?;

    match is_already_used_option {
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

pub async fn set_oauth_token_in_redis(
    oauth_token: String,
) -> Result<(), RedisError> {
    // Get a connection from the REDIS_POOL
    let mut conn = get_redis_cnxn().await?;

    // Save the allowance information to Relayer DB
    conn.set(&oauth_token, true)?;
    Ok(())
}

pub async fn update_all_allowances_in_redis(allowance_in_gas: u64) -> Result<String, RelayError> {
    // Get a connection to Redis from the pool
    let mut redis_conn = match get_redis_cnxn().await {
        Ok(conn) => conn,
        Err(e) => {
            let err_msg = format!("Error getting Relayer DB connection from the pool: {}", e);
            error!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: err_msg
            });
        }
    };

    // Fetch all keys that match the network env (.near for mainnet, .testnet for testnet, etc)
    let network: String = if NETWORK_ENV.clone() != "mainnet" {
        NETWORK_ENV.clone()
    } else {
        "near".to_string()
    };
    let pattern = format!("*.{}", network);
    let keys: Vec<String> = match redis_conn.keys(pattern) {
        Ok(keys) => keys,
        Err(e) => {
            let err_msg = format!("Error fetching keys from Relayer DB: {}", e);
            error!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: err_msg
            });
        }
    };

    // Iterate through the keys and update their values to the provided allowance in gas
    for key in &keys {
        match redis_conn.set::<_, _, ()>(key, allowance_in_gas.to_string()) {
            Ok(_) => debug!("Updated allowance for key {}", key),
            Err(e) => {
                let err_msg = format!("Error updating allowance for key {}: {}", key, e);
                error!("{err_msg}");
                return Err(RelayError {
                    status_code: StatusCode::INTERNAL_SERVER_ERROR,
                    message: err_msg
                });
            }
        }
    }

    // Return a success response
    let num_keys = keys.len();
    let success_msg: String = format!("Updated {num_keys:?} keys in Relayer DB");
    info!("{success_msg}");
    Ok(success_msg)
}

pub async fn get_remaining_allowance(
    account_id: &AccountId,
) -> Result<u64, RedisError> {
    // Destructure the Extension and get a connection from the connection manager
    let mut conn = get_redis_cnxn().await?;
    let allowance: Option<u64> = conn.get(account_id.as_str())?;
    let Some(remaining_allowance) = allowance else {
        return Ok(0);
    };
    info!("get remaining allowance for account: {account_id}, {remaining_allowance}");
    Ok(remaining_allowance)
}

// fn to update allowance in redis when getting the receipts back and deduct the gas used
pub async fn update_remaining_allowance(
    account_id: &AccountId,
    gas_used_in_yn: u128,
    allowance: u64,
) -> Result<u64, RedisError> {
    let mut conn = get_redis_cnxn().await?;
    let key = account_id.clone().to_string();
    let gas_used: u64 = (gas_used_in_yn / YN_TO_GAS) as u64;
    let remaining_allowance = allowance - gas_used;
    conn.set(key, remaining_allowance)?;
    Ok(remaining_allowance.clone())
}

pub fn calculate_total_gas_burned(
    transaction_outcome: ExecutionOutcomeWithIdView,
    execution_outcome: Vec<ExecutionOutcomeWithIdView>
) -> u128 {
    let mut total_tokens_burnt_in_yn: u128 = 0;
    total_tokens_burnt_in_yn += transaction_outcome.outcome.tokens_burnt;

    let exec_outcome_sum: u128 = execution_outcome
        .iter()
        .map(|ro| {
            &ro.outcome.tokens_burnt
        })
        .sum();
    total_tokens_burnt_in_yn += exec_outcome_sum;

    total_tokens_burnt_in_yn
}
