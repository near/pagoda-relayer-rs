mod error;
mod rpc_conf;
mod shared_storage;
mod signing;
mod rpc;

use axum::{
    extract::Json,
    http::StatusCode,
    response::IntoResponse,
    Router,
    routing::{get, post}
};
use config::{Config, File};
#[cfg(test)]
use near_crypto::{KeyType, PublicKey, Signature};
#[cfg(test)]
use near_primitives::borsh::BorshSerialize;
use near_primitives::borsh::BorshDeserialize;
#[cfg(test)]
use near_primitives::delegate_action::{DelegateAction, NonDelegateAction};
use near_primitives::delegate_action::SignedDelegateAction;
#[cfg(test)]
use near_primitives::transaction::TransferAction;
use near_primitives::transaction::Action;
#[cfg(test)]
use near_primitives::types::{BlockHeight, Nonce};
use near_primitives::types::AccountId;
use near_primitives::views::ExecutionOutcomeWithIdView;
use once_cell::sync::Lazy;
use r2d2::{Pool, PooledConnection};
use r2d2_redis::{redis, RedisConnectionManager};
use r2d2_redis::redis::{Commands, RedisError};
use serde::Deserialize;
use serde_json::json;
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::net::SocketAddr;
use std::str::FromStr;
use std::string::ToString;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use axum::body::{BoxBody, HttpBody};
#[cfg(test)]
use axum::response::Response;
#[cfg(test)]
use bytes::BytesMut;
use tower_http::trace::TraceLayer;
use tracing::{debug, info};
use tracing::log::error;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::error::RelayError;
use crate::rpc_conf::{NetworkConfig, RPCConfig};
use crate::shared_storage::SharedStoragePoolManager;


// transaction cost in Gas (10^21yN or 10Tgas or 0.001N)
const TXN_GAS_ALLOWANCE: u64 = 10_000_000_000_000;
const YN_TO_GAS: u128 = 1_000_000_000;

// load config from toml and setup json rpc client
static LOCAL_CONF: Lazy<Config> = Lazy::new(|| {
    Config::builder()
        .add_source(File::with_name("config.toml"))
        .build()
        .unwrap()
});
static NETWORK_ENV: Lazy<String> = Lazy::new(|| { LOCAL_CONF.get("network").unwrap() });
static JSON_RPC_CLIENT: Lazy<near_jsonrpc_client::JsonRpcClient> = Lazy::new(|| {
    let network_name: String = LOCAL_CONF.get("network").unwrap();
    let rpc_config = RPCConfig::default();

    // optional overrides
    if LOCAL_CONF.get::<bool>("override_rpc_conf").unwrap() {
        let network_config = NetworkConfig {
            network_name,
            rpc_url: LOCAL_CONF.get("rpc_url").unwrap(),
            rpc_api_key: LOCAL_CONF.get("rpc_api_key").unwrap(),
            wallet_url: LOCAL_CONF.get("wallet_url").unwrap(),
            explorer_transaction_url: LOCAL_CONF.get("explorer_transaction_url").unwrap(),
        };
        network_config.json_rpc_client()
    } else {
        let network_config = rpc_config.networks.get(&network_name).unwrap();
        network_config.json_rpc_client()
    }

});
static IP_ADDRESS: Lazy<[u8; 4]> = Lazy::new(|| { LOCAL_CONF.get("ip_address").unwrap() });
static PORT: Lazy<u16> = Lazy::new(|| { LOCAL_CONF.get("port").unwrap() });
static RELAYER_ACCOUNT_ID: Lazy<String> = Lazy::new(|| {
    LOCAL_CONF.get("relayer_account_id").unwrap()
});
static SHARED_STORAGE_ACCOUNT_ID: Lazy<String> = Lazy::new(|| {
    LOCAL_CONF.get("shared_storage_account_id").unwrap()
});
static KEYS_FILENAMES: Lazy<Vec<String>> = Lazy::new(|| {
    LOCAL_CONF.get::<Vec<String>>("keys_filenames")
        .expect("Failed to read 'keys_filenames' from config")
});
static KEYS_FILENAMES_LEN: Lazy<usize> = Lazy::new(||{ LOCAL_CONF.get("num_keys").unwrap() });
static SHARED_STORAGE_KEYS_FILENAME: Lazy<String> = Lazy::new(|| {
    LOCAL_CONF.get("shared_storage_keys_filename").unwrap()
});
static WHITELISTED_CONTRACTS: Lazy<Vec<String>> = Lazy::new(|| {
    LOCAL_CONF.get("whitelisted_contracts").unwrap()
});
static WHITELISTED_DELEGATE_ACTION_RECEIVER_IDS: Lazy<Vec<String>> = Lazy::new(|| {
    LOCAL_CONF.get("whitelisted_delegate_action_receiver_ids").unwrap()
});
static REDIS_POOL: Lazy<Pool<RedisConnectionManager>> = Lazy::new(|| {
    let redis_cnxn_url: String = LOCAL_CONF.get("redis_url").unwrap();
    let manager = RedisConnectionManager::new(redis_cnxn_url).unwrap();
    Pool::builder().build(manager).unwrap()
});
static SHARED_STORAGE_POOL: Lazy<SharedStoragePoolManager> = Lazy::new(|| {
    let network_name: String = LOCAL_CONF.get("network").unwrap();
    let mut social: serde_json::Map<String, serde_json::Value> = LOCAL_CONF.get("social_db").unwrap();
    let social_db_id: String = serde_json::from_value(social.remove(&network_name).unwrap()).unwrap();

    SharedStoragePoolManager::new(
        &SHARED_STORAGE_KEYS_FILENAME,
        &JSON_RPC_CLIENT,
        social_db_id.parse().unwrap(),
        SHARED_STORAGE_ACCOUNT_ID.parse().unwrap(),
    )
});
struct IndexCounter {
    idx: usize,
    max_idx: usize,
}
// thread safe mutable index counter for round robin key usage
impl IndexCounter {
    fn new(max_idx: usize) -> IndexCounter {
        IndexCounter { idx: 0, max_idx }
    }
    fn get_and_increment(&mut self) -> usize {
        let cur = self.idx;
        self.idx += 1;
        if self.idx >= self.max_idx {
            self.idx = 0;
        }
        cur
    }
}
static IDX_COUNTER: Lazy<Arc<Mutex<IndexCounter>>> = Lazy::new(|| {
    let max_idx: usize = *KEYS_FILENAMES_LEN;
    let idx_counter: IndexCounter = IndexCounter::new(max_idx);
    Arc::new(Mutex::new(idx_counter))
});

#[derive(Clone, Debug, Deserialize)]
struct AccountIdAllowanceOauthSDAJson {
    account_id: String,
    allowance: u64,
    oauth_token: String,
    signed_delegate_action: SignedDelegateAction,
}
impl Display for AccountIdAllowanceOauthSDAJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "account_id: {}, allowance in TGas: {}, oauth_token: {}, signed_delegate_action signature: {}",
            self.account_id, self.allowance, self.oauth_token, self.signed_delegate_action.signature
        )  // SignedDelegateAction doesn't implement display, so just displaying signature
    }
}

#[derive(Clone, Debug, Deserialize)]
struct AccountIdAllowanceOauthJson {
    account_id: String,
    allowance: u64,
    oauth_token: String,
}
impl Display for AccountIdAllowanceOauthJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "account_id: {}, allowance in TGas: {}, oauth_token: {}",
            self.account_id, self.allowance, self.oauth_token
        )
    }
}

#[derive(Clone, Debug, Deserialize)]
struct AccountIdAllowanceJson {
    account_id: String,
    allowance: u64,
}
impl Display for AccountIdAllowanceJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "account_id: {}, allowance in TGas: {}",
            self.account_id, self.allowance
        )
    }
}

#[derive(Clone, Debug, Deserialize)]
struct AccountIdJson {
    account_id: String,
}
impl Display for AccountIdJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "account_id: {}", self.account_id)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct AllowanceJson {  // TODO: LP use for return type of GET get_allowance
    allowance_in_gas: u64,
}
impl Display for AllowanceJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "allowance in Gas: {}", self.allowance_in_gas)
    }
}


#[tokio::main]
async fn main() {
    // initialize tracing (aka logging)
    tracing_subscriber::registry().with(tracing_subscriber::fmt::layer()).init();

    // initialize our shared storage pool manager
    if let Err(err) = SHARED_STORAGE_POOL.check_and_spawn_pool().await {
        tracing::error!("Error initializing shared storage pool: {err}");
        return;
    }

    //TODO: not secure, allow only for testnet, whitelist endpoint etc. for mainnet
    let cors_layer = tower_http::cors::CorsLayer::permissive();

    // build our application with a route
    let app = Router::new()
        // `POST /relay` goes to `relay` handler function
        .route("/relay", post(relay))
        .route("/send_meta_tx", post(send_meta_tx))
        .route("/create_account_atomic", post(create_account_atomic))
        .route("/get_allowance", get(get_allowance))
        .route("/update_allowance", post(update_allowance))
        .route("/update_all_allowances", post(update_all_allowances))
        .route("/register_account", post(register_account_and_allowance))
        // See https://docs.rs/tower-http/0.1.1/tower_http/trace/index.html for more details.
        .layer(TraceLayer::new_for_http())
        .layer(cors_layer);

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from((*IP_ADDRESS, *PORT));
    info!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn get_allowance(account_id_json: Json<AccountIdJson>) -> impl IntoResponse {
    // convert str account_id val from json to AccountId so I can reuse get_remaining_allowance fn
    let Ok(account_id_val) = AccountId::from_str(&account_id_json.account_id) else {
        return (StatusCode::FORBIDDEN, format!("Invalid account_id: {}", account_id_json.account_id)).into_response();
    };
    match get_remaining_allowance(&account_id_val).await {
        Ok(allowance) => (
            StatusCode::OK,
            allowance.to_string()  // TODO: LP return in json format
            // AllowanceJson {
            //     allowance_in_gas: allowance
            // }
        ).into_response(),
        Err(err) => {
            let err_msg = format!(
                "Error getting allowance for account_id {} in Redis: {:?}",
                account_id_val.clone().as_str(), err
            );
            info!("{err_msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response()
        }
    }
}

async fn get_redis_cnxn() -> Result<PooledConnection<RedisConnectionManager>, RedisError> {
    let conn_result = REDIS_POOL.get();
    let conn: PooledConnection<RedisConnectionManager> = match conn_result {
        Ok(conn) => conn,
        Err(e) => {
            return Err(redis::RedisError::from((
                redis::ErrorKind::IoError,
                "Error getting Relayer DB connection from the pool",
                e.to_string(),
            )));
        }
    };
    Ok(conn)
}

async fn set_account_and_allowance_in_redis(
    account_id: &str,
    allowance_in_gas: &u64,
) -> Result<(), RedisError> {
    // Get a connection from the REDIS_POOL
    let mut conn = get_redis_cnxn().await?;

    // Save the allowance information to Redis
    conn.set(account_id, *allowance_in_gas)?;
    Ok(())
}


async fn create_account_atomic(
    account_id_allowance_oauth_sda: Json<AccountIdAllowanceOauthSDAJson>
) -> impl IntoResponse {
    /*
    This function atomically creates an account, both in our systems (redis)
    and on chain created both an on chain account and adding that account to the storage pool

    Motivation for doing this is when calling /register_account and then /send_meta_tx and
    /register_account succeeds, but /send_meta_tx fails, then the account is now
    unable to use the relayer without manual intervention deleting the record from redis
     */

    // get individual vars from json object
    let account_id: &String = &account_id_allowance_oauth_sda.account_id;
    let allowance_in_gas: u64 = account_id_allowance_oauth_sda.allowance;
    let oauth_token: &String = &account_id_allowance_oauth_sda.oauth_token;
    let sda: SignedDelegateAction = account_id_allowance_oauth_sda.signed_delegate_action.clone();

    /*
        do logic similar to register_account_and_allowance fn
        without updating redis or allocating shared storage
        if that fails, return error
        if it succeeds, then continue
     */

    // check if the oauth_token has already been used and is a key in Redis
    match get_oauth_token_in_redis(&oauth_token).await {
        Ok(is_oauth_token_in_redis) => {
            if is_oauth_token_in_redis {
                let err_msg = format!(
                    "Error: oauth_token {oauth_token} has already been used to register an account. \
                    You can only register 1 account per oauth_token",
                );
                info!("{}", err_msg);
                return (StatusCode::BAD_REQUEST, err_msg).into_response();
            }
        }
        Err(err) => {
            let err_msg = format!(
                "Error getting oauth_token for account_id {account_id}, oauth_token {oauth_token} in Relayer DB: {err:?}",
            );
            info!("{}", err_msg);
            return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
        }
    }
    let redis_result = set_account_and_allowance_in_redis(
        account_id,
        &allowance_in_gas,
    ).await;

    let Ok(_) = redis_result else {
        let err_msg = format!(
            "Error creating account_id {account_id} with allowance {allowance_in_gas} in Relayer DB:\n{redis_result:?}");
        info!("{}", err_msg);
        return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
    };

    /*
        call process_signed_delegate_action fn
        if there's an error, then return error
        if it succeeds, then add oauth token to redis and allocate shared storage
        after updated redis and adding shared storage, finally return success msg
     */
    let create_account_sda_result = process_signed_delegate_action(sda).await;
    if create_account_sda_result.is_err() {
        let err: RelayError = create_account_sda_result.err().unwrap();
        return (err.status_code, err.message).into_response();
    }

    // allocate shared storage for account_id
    let Ok(account_id) = account_id.parse::<AccountId>() else {
        return (StatusCode::FORBIDDEN, format!("Invalid account_id: {}", account_id)).into_response();
    };
    if let Err(err) = SHARED_STORAGE_POOL.allocate_default(account_id.clone()).await {
        let msg = format!("Error allocating storage for account {account_id}: {err:?}");
        info!("{}", msg);
        return (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response();
    }

    // add oauth token to redis (key: oauth_token, val: true)
    match set_oauth_token_in_redis(oauth_token.clone()).await {
        Ok(_) => {
            (
                StatusCode::CREATED,
                format!(
                    "Added Oauth token {oauth_token:?} for account_id {account_id:?} \
                    with allowance (in Gas) {allowance_in_gas:?} to Relayer DB. \
                    Near onchain account creation response: {create_account_sda_result:?}")
            ).into_response()
        }
        Err(err) => {
            let err_msg = format!(
                "Error creating oauth token {oauth_token:?} in Relayer DB:\n{err:?}",
            );
            info!("{}", err_msg);
            (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response()
        }
    }
}

async fn get_oauth_token_in_redis(
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

async fn set_oauth_token_in_redis(
    oauth_token: String,
) -> Result<(), RedisError> {
    // Get a connection from the REDIS_POOL
    let mut conn = get_redis_cnxn().await?;

    // Save the allowance information to Relayer DB
    conn.set(&oauth_token, true)?;
    Ok(())
}

async fn update_allowance(
    account_id_allowance: Json<AccountIdAllowanceJson>
) -> impl IntoResponse {
    let account_id = &account_id_allowance.account_id;
    let allowance_in_gas = account_id_allowance.allowance;

    let redis_result = set_account_and_allowance_in_redis(
        account_id,
        &allowance_in_gas,
    ).await;

    let Ok(_) = redis_result else {
        let err_msg = format!(
            "Error updating account_id {account_id} with allowance {allowance_in_gas} in Relayer DB:\
            \n{redis_result:?}"
        );
        info!("{err_msg}");
        return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
    };
    let success_msg: String = format!("Relayer DB updated for {account_id_allowance:?}");
    info!("{}", success_msg);
    (
        StatusCode::CREATED,
        success_msg
    ).into_response()
}

async fn update_all_allowances_in_redis(allowance_in_gas: u64) -> Result<String, RelayError> {
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

async fn update_all_allowances(
    Json(allowance_json): Json<AllowanceJson>,
) -> impl IntoResponse {
    let allowance_in_gas = allowance_json.allowance_in_gas;
    let redis_response = update_all_allowances_in_redis(allowance_in_gas).await;
    match redis_response {
        Ok(response) => response.into_response(),
        Err(err) => (err.status_code, err.message).into_response(),
    }
}


async fn register_account_and_allowance(
    account_id_allowance_oauth: Json<AccountIdAllowanceOauthJson>
) -> impl IntoResponse {
    let account_id: &String = &account_id_allowance_oauth.account_id;
    let allowance_in_gas: u64 = account_id_allowance_oauth.allowance;
    let oauth_token: &String = &account_id_allowance_oauth.oauth_token;
    // check if the oauth_token has already been used and is a key in Relayer DB
    match get_oauth_token_in_redis(oauth_token).await {
        Ok(is_oauth_token_in_redis) => {
            if is_oauth_token_in_redis {
                let err_msg = format!(
                    "Error: oauth_token {oauth_token} has already been used to register an account. \
                    You can only register 1 account per oauth_token",
                );
                info!("{err_msg}");
                return (StatusCode::BAD_REQUEST, err_msg).into_response();
            }
        }
        Err(err) => {
            let err_msg = format!(
                "Error getting oauth_token for account_id {account_id}, \
                oauth_token {oauth_token} in Relayer DB: {err:?}",
            );
            error!("{err_msg}");
            return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
        }
    }
    let redis_result = set_account_and_allowance_in_redis(
        account_id,
        &allowance_in_gas,
    ).await;

    let Ok(_) = redis_result else {
        let err_msg = format!(
            "Error creating account_id {account_id} with allowance {allowance_in_gas} in Relayer DB:\
            \n{redis_result:?}");
        error!("{err_msg}");
        return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
    };

    let Ok(account_id) = account_id.parse::<AccountId>() else {
        return (StatusCode::FORBIDDEN, format!("Invalid account_id: {}", account_id)).into_response();
    };
    if let Err(err) = SHARED_STORAGE_POOL.allocate_default(account_id.clone()).await {
        let msg = format!("Error allocating storage for account {account_id}: {err:?}");
        error!("{msg}");
        return (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response();
    }

    // add oauth token to redis (key: oauth_token, val: true)
    match set_oauth_token_in_redis(oauth_token.clone()).await {
        Ok(_) => {
            (
                StatusCode::CREATED,
                format!("Added Oauth token {account_id_allowance_oauth:?} to Relayer DB")
            ).into_response()
        }
        Err(err) => {
            let err_msg = format!(
                "Error creating oauth token {oauth_token:?} in Relayer DB:\n{err:?}",
            );
            error!("{err_msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response()
        }
    }
}

async fn get_remaining_allowance(
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
async fn update_remaining_allowance(
    account_id: &AccountId,
    gas_used_in_yn: u128,
    allowance: u64,
) -> Result<u64, RedisError> {
    let mut conn = get_redis_cnxn().await?;
    let key = account_id.clone().to_string();
    let gas_used: u64 = (gas_used_in_yn / YN_TO_GAS) as u64;
    let remaining_allowance = allowance - gas_used;
    conn.set(key, remaining_allowance)?;
    Ok(remaining_allowance)
}

async fn relay(
    data: Json<Vec<u8>>,
) -> impl IntoResponse {
    // deserialize SignedDelegateAction using borsh
    match SignedDelegateAction::try_from_slice(&data.0) {
        Ok(signed_delegate_action) => match process_signed_delegate_action(
            signed_delegate_action,
        ).await {
            Ok(response) => response.into_response(),
            Err(err) => (err.status_code, err.message).into_response(),
        },
        Err(e) => {
            let err_msg = format!(
                "{}: {:?}", "Error deserializing payload data object", e.to_string(),
            );
            error!("{err_msg}");
            (StatusCode::BAD_REQUEST, err_msg).into_response()
        },
    }
}

async fn send_meta_tx(
    data: Json<SignedDelegateAction>,
) -> impl IntoResponse {
    let relayer_response = process_signed_delegate_action(
        // deserialize SignedDelegateAction using serde json
        data.0,
    ).await;
    match relayer_response {
        Ok(response) => response.into_response(),
        Err(err) => (err.status_code, err.message).into_response(),
    }
}

fn calculate_total_gas_burned(
    transaction_outcome: ExecutionOutcomeWithIdView,
    execution_outcome: Vec<ExecutionOutcomeWithIdView>
) -> u128 {
    let mut total_tokens_burnt_in_yn: u128 = 0;
    total_tokens_burnt_in_yn += transaction_outcome.outcome.tokens_burnt;

    let exec_outcome_sum: u128 = execution_outcome
        .iter()
        .map(|ro| {
            ro.outcome.tokens_burnt
        })
        .sum();
    total_tokens_burnt_in_yn += exec_outcome_sum;

    total_tokens_burnt_in_yn
}

async fn process_signed_delegate_action(
    signed_delegate_action: SignedDelegateAction,
) -> Result<String, RelayError> {
    debug!("Deserialized SignedDelegateAction object: {:#?}", signed_delegate_action);

    // create Transaction from SignedDelegateAction
    let signer_account_id: AccountId = RELAYER_ACCOUNT_ID.as_str().parse().unwrap();
    // the receiver of the txn is the sender of the signed delegate action
    let receiver_id = signed_delegate_action.delegate_action.sender_id.clone();
    let da_receiver_id = signed_delegate_action.delegate_action.receiver_id.clone();

    // check that the delegate action receiver_id is in the whitelisted_contracts
    let is_whitelisted_da_receiver = WHITELISTED_CONTRACTS.iter().any(
        |s| s == da_receiver_id.as_str()
    );
    if !is_whitelisted_da_receiver {
        // check if sender id and receiver id are the same AND (AddKey or DeleteKey action)
        let non_delegate_action = signed_delegate_action.delegate_action.actions.get(0).ok_or_else(|| {
            RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: "DelegateAction must have at least one NonDelegateAction".to_string(),
            }
        })?;
        let contains_key_action = match (*non_delegate_action).clone().into() {
            Action::AddKey(_) => true,
            Action::DeleteKey(_) => true,
            _ => false,
        };
        // check if the receiver_id (delegate action sender_id) if a whitelisted delegate action receiver
        let is_whitelisted_sender = WHITELISTED_DELEGATE_ACTION_RECEIVER_IDS.iter().any(
            |s| s == receiver_id.as_str()
        );
        if (receiver_id != da_receiver_id || !contains_key_action) && !is_whitelisted_sender {
            let err_msg = format!(
                "Delegate Action receiver_id {} or sender_id {} is not whitelisted OR \
                (they do not match AND the NonDelegateAction is not AddKey or DeleteKey)",
                da_receiver_id.as_str(),
                receiver_id.as_str(),
            );
            info!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::BAD_REQUEST,
                message: err_msg
            });
        }
    }

    // Check the sender's remaining gas allowance in Redis
    let end_user_account = &signed_delegate_action.delegate_action.sender_id;
    let remaining_allowance = get_remaining_allowance(end_user_account).await.unwrap_or(0);
    if remaining_allowance < TXN_GAS_ALLOWANCE {
        let err_msg = format!(
            "AccountId {} does not have enough remaining gas allowance.",
            end_user_account.as_str()
        );
        info!("{err_msg}");
        return Err(RelayError {
            status_code: StatusCode::BAD_REQUEST,
            message: err_msg,
        });
    }

    let actions = vec![Action::Delegate(signed_delegate_action)];
    // round robin usage of keys to prevent nonce race condition
    let tmp_str = "".to_string();
    let mut keys_filename: &String = &tmp_str;
    {
        let idx: usize = IDX_COUNTER.lock().unwrap().get_and_increment();
        keys_filename = &KEYS_FILENAMES[idx];
    }  // lock is released when it goes out of scope here
    let execution = rpc::send_tx(
        &JSON_RPC_CLIENT,
        keys_filename,
        &signer_account_id,
        &receiver_id,
        actions,
        "delegate_action",
    )
    .await
    .map_err(|_err| {
        let err_msg: String = format!("Error signing transaction: {:?}", _err.to_string());
        error!("{err_msg}");
        RelayError {
            status_code: StatusCode::INTERNAL_SERVER_ERROR,
            message: err_msg.into(),
        }
    })?;

    let status = &execution.status;
    let mut response_msg: String = "".to_string();
    match status {
        near_primitives::views::FinalExecutionStatus::Failure(_) => {
            response_msg = "Error sending transaction".to_string();
        }
        _ => {
            response_msg = "Relayed and sent transaction".to_string();
        }
    }
    let status_msg = json!({
        "message": response_msg,
        "status": &execution.status,
        "Transaction Outcome": &execution.transaction_outcome,
        "Receipts Outcome": &execution.receipts_outcome,
    });

    let gas_used_in_yn = calculate_total_gas_burned(
        execution.transaction_outcome,
        execution.receipts_outcome,
    );
    debug!("total gas burnt in yN: {}", gas_used_in_yn);
    let new_allowance = update_remaining_allowance(
        &signer_account_id,
        gas_used_in_yn,
        remaining_allowance
    )
    .await
    .map_err(|err| {
        let err_msg = format!(
            "Updating redis remaining allowance errored out: {err:?}"
        );
        error!("{err_msg}");
        RelayError {
            status_code: StatusCode::INTERNAL_SERVER_ERROR,
            message: err_msg,
        }
    })?;

    info!("Updated remaining allowance for account {signer_account_id}: {new_allowance}",);
    return match status {
        near_primitives::views::FinalExecutionStatus::Failure(_) => {
            error!("Error message: \n{status_msg:?}");
            Err(RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: status_msg.to_string(),
            })
        }
        _ => {
            info!("Success message: \n{status_msg:?}");
            Ok(status_msg.to_string())
        }
    }

}

/**
--------------------------- Testing below here ---------------------------
*/
#[cfg(test)]
fn create_signed_delegate_action(
    sender_id: String,
    receiver_id: String,
    actions: Vec<Action>,
    nonce: i32,
    max_block_height: i32,
    ) -> SignedDelegateAction {
    let max_block_height: i32 = max_block_height;
    let public_key: PublicKey = PublicKey::empty(KeyType::ED25519);
    let signature: Signature = Signature::empty(KeyType::ED25519);
    SignedDelegateAction {
        delegate_action: DelegateAction {
            sender_id: sender_id.parse().unwrap(),
            receiver_id: receiver_id.parse().unwrap(),
            actions: actions
                .iter()
                .map(|a| NonDelegateAction::try_from(a.clone()).unwrap())
                .collect(),
            nonce: nonce as Nonce,
            max_block_height: max_block_height as BlockHeight,
            public_key,
        },
        signature,
    }
}

#[cfg(test)]
async fn read_body_to_string(mut body: BoxBody) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // helper fn to convert the awful BoxBody dtype into a String so I can view the darn msg
    let mut bytes = BytesMut::new();
    while let Some(chunk) = body.data().await {
        bytes.extend_from_slice(&chunk?);
    }
    Ok(String::from_utf8(bytes.to_vec())?)
}

#[tokio::test]
// NOTE: uncomment ignore locally to run test bc redis doesn't work in github action build env
#[ignore]
async fn test_send_meta_tx() {   // tests assume testnet in config
    // Test Transfer Action
    let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
    let sender_id: String = String::from("relayer_test0.testnet");
    let receiver_id: String = String::from("relayer_test1.testnet");
    let nonce: i32 = 1;
    let max_block_height = 2000000000;

    // simulate calling the '/update_allowance' function with sender_id & allowance
    let allowance_in_gas: u64 = u64::MAX;
    set_account_and_allowance_in_redis(&sender_id, &allowance_in_gas).await.expect(
        "Failed to update account and allowance in redis"
    );

    // Call the `/send_meta_tx` function happy path
    let signed_delegate_action = create_signed_delegate_action(
        sender_id.clone(),
        receiver_id.clone(),
        actions.clone(),
        nonce,
        max_block_height,
    );
    let json_payload = Json(signed_delegate_action);
    println!("SignedDelegateAction Json Serialized (no borsh): {:?}", json_payload);
    let response: Response = send_meta_tx(json_payload).await.into_response();
    let response_status: StatusCode = response.status();
    let body: BoxBody = response.into_body();
    let body_str: String = read_body_to_string(body).await.unwrap();
    println!("Response body: {body_str:?}");
    assert_eq!(response_status, StatusCode::OK);
}

#[tokio::test]
async fn test_send_meta_tx_no_gas_allowance() {
    let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
    let sender_id: String = String::from("relayer_test0.testnet");
    let receiver_id: String = String::from("arrr_me_not_in_whitelist");
    let nonce: i32 = 54321;
    let max_block_height = 2000000123;

    // Call the `send_meta_tx` function with a sender that has no gas allowance
    // (and a receiver_id that isn't in whitelist)
    let sda2 = create_signed_delegate_action(
        sender_id.clone(),
        receiver_id.clone(),
        actions.clone(),
        nonce,
        max_block_height,
    );
    let non_whitelist_json_payload = Json(sda2);
    println!("SignedDelegateAction Json Serialized (no borsh) receiver_id not in whitelist: {:?}", non_whitelist_json_payload);
    let err_response = send_meta_tx(non_whitelist_json_payload).await.into_response();
    let err_response_status = err_response.status();
    let body: BoxBody = err_response.into_body();
    let body_str: String = read_body_to_string(body).await.unwrap();
    println!("Response body: {body_str:?}");
    assert_eq!(err_response_status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore]
async fn test_relay_with_load() {   // tests assume testnet in config
    // Test Transfer Action

    let actions = vec![
        Action::Transfer(TransferAction { deposit: 1 })
    ];
    let account_id0: String = "relayer_test0.testnet".to_string();
    let account_id1: String = "relayer_test1.testnet".to_string();
    let mut sender_id: String = String::new();
    let mut receiver_id: String = String::new();
    let mut nonce: i32 = 1;
    let max_block_height = 2000000000;

    let num_tests = 100;
    let mut response_statuses = vec![];

    // fire off all post requests in rapid succession and save the response status codes
    for i in 0..num_tests {
        if i % 2 == 0 {
            sender_id.push_str(&*account_id0.clone());
            receiver_id.push_str(&*account_id1.clone());
        } else {
            sender_id.push_str(&*account_id1.clone());
            receiver_id.push_str(&*account_id0.clone());
        }
        // Call the `relay` function happy path
        let signed_delegate_action = create_signed_delegate_action(
            sender_id.clone(),
            receiver_id.clone(),
            actions.clone(),
            nonce,
            max_block_height,
        );
        let json_payload = signed_delegate_action.try_to_vec().unwrap();
        let response = relay(Json(Vec::from(json_payload))).await.into_response();
        response_statuses.push(response.status());

        // increment nonce & reset sender, receiver strs
        nonce += 1;
        sender_id.clear();
        receiver_id.clear();
    }

    // all responses should be successful
    for i in 0..response_statuses.len() {
        assert_eq!(response_statuses[i], StatusCode::OK);
    }
}