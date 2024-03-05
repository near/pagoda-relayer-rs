mod error;
mod redis_fns;
mod rpc_conf;
mod shared_storage;

use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use config::{Config, File as ConfigFile};
use near_crypto::{InMemorySigner, PublicKey};
use near_fetch::signer::{ExposeAccountId, KeyRotatingSigner, SignerExt};
use near_jsonrpc_client::methods::broadcast_tx_async::RpcBroadcastTxAsyncRequest;
use near_jsonrpc_client::methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest;
use near_primitives::delegate_action::SignedDelegateAction;
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{Action, FunctionCallAction, Transaction};
use near_primitives::types::AccountId;
use near_primitives::views::{
    ExecutionOutcomeWithIdView, FinalExecutionOutcomeView, FinalExecutionStatus,
};
use near_primitives::{borsh::BorshDeserialize, transaction::CreateAccountAction};
use once_cell::sync::Lazy;
use r2d2::{Pool, PooledConnection};
use r2d2_redis::redis::{Commands, ErrorKind::IoError, RedisError};
use r2d2_redis::RedisConnectionManager;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::string::ToString;
use std::time::Duration;
use std::{net::SocketAddr, sync::Arc};
use tokio::time::sleep;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, instrument, warn};
use tracing_flame::FlameLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use utoipa::{OpenApi, ToSchema};
use utoipa_rapidoc::RapiDoc;
use utoipa_swagger_ui::SwaggerUi;

use crate::error::RelayError;
use crate::redis_fns::*;
use crate::rpc_conf::NetworkConfig;
use crate::shared_storage::SharedStoragePoolManager;
#[cfg(feature = "shared_storage")]
use crate::shared_storage::*;

// transaction cost in Gas (10^21yN or 10Tgas or 0.001N)
const TXN_GAS_ALLOWANCE: u64 = 10_000_000_000_000;
const YN_TO_GAS: u128 = 1_000_000_000;
const FT_TRANSFER_METHOD_NAME: &str = "ft_transfer";
const STORAGE_DEPOSIT_METHOD_NAME: &str = "storage_deposit";
const STORAGE_DEPOSIT_AMOUNT_FT: u128 = 1250000000000000000000;
const FT_TRANSFER_ATTACHMENT_DEPOSIT_AMOUNT: u128 = 1;

#[derive(Debug, Clone, serde::Deserialize)]
struct RelayerConfiguration {
    #[serde(rename = "network")]
    network_env: String,
    #[serde(flatten)]
    network_config: NetworkConfig,
    ip_address: [u8; 4],
    port: u16,
    relayer_account_id: String,
    shared_storage_account_id: String,
    keys_filename: PathBuf,
    shared_storage_keys_filename: PathBuf,
    #[serde(default)]
    use_whitelisted_contracts: bool,
    whitelisted_contracts: Vec<AccountId>,
    #[serde(default)]
    use_whitelisted_senders: bool,
    whitelisted_senders: Vec<String>,
    #[serde(default)]
    use_redis: bool,
    redis_url: String,
    #[serde(default)]
    use_fastauth_features: bool,
    #[serde(default)]
    use_pay_with_ft: bool,
    burn_address: AccountId,
    #[serde(default)]
    use_shared_storage: bool,
    social_db_contract_id: AccountId,
    #[serde(default)]
    flametrace_performance: bool,
    #[serde(default)]
    use_exchange: bool,
}

fn create_redis_pool(config: &RelayerConfiguration) -> Pool<RedisConnectionManager> {
    let manager = RedisConnectionManager::new(config.redis_url.clone()).unwrap();
    Pool::builder().build(manager).unwrap()
}

#[cfg(feature = "shared_storage")]
fn create_shared_storage_pool(
    config: &RelayerConfiguration,
    rpc_client: Arc<near_fetch::Client>,
) -> SharedStoragePoolManager {
    let signer =
        InMemorySigner::from_file(&config.shared_storage_keys_filename).unwrap_or_else(|err| {
            panic!(
                "failed to get signing keys={:?}: {err:?}",
                config.shared_storage_keys_filename
            )
        });
    SharedStoragePoolManager::new(
        signer,
        Arc::clone(&rpc_client),
        config.social_db_contract_id.clone(),
        config.shared_storage_account_id.parse().unwrap(),
    )
}

#[derive(Debug)]
struct AppState {
    config: RelayerConfiguration,
    rpc_client: Arc<near_fetch::Client>,
    rpc_client_nofetch: Arc<near_jsonrpc_client::JsonRpcClient>,
    shared_storage_pool: Option<SharedStoragePoolManager>,
    redis_pool: Option<Pool<RedisConnectionManager>>,
}

// load config from toml and setup jsonrpc client
static LOCAL_CONF: Lazy<Config> = Lazy::new(|| {
    Config::builder()
        .add_source(ConfigFile::with_name("config.toml"))
        .build()
        .unwrap()
});
static SIGNER: Lazy<KeyRotatingSigner> = Lazy::new(|| {
    let path = LOCAL_CONF
        .get::<String>("keys_filename")
        .expect("Failed to read 'keys_filename' from config");
    let keys_file = std::fs::File::open(path).expect("Failed to open keys file");
    let signers: Vec<InMemorySigner> =
        serde_json::from_reader(keys_file).expect("Failed to parse keys file");

    KeyRotatingSigner::from_signers(signers)
});

#[derive(Clone, Debug, Deserialize, ToSchema)]
struct AccountIdAllowanceOauthSDAJson {
    #[schema(example = "example.near")]
    account_id: String,
    #[schema(example = 900_000_000)]
    allowance: u64,
    #[schema(
        example = "https://securetoken.google.com/pagoda-oboarding-dev:Op4h13AQozM4CikngfHiFVC2xhf2"
    )]
    oauth_token: String,
    // NOTE: imported SignedDelegateAction itself doesn't have a corresponding schema in the OpenAPI document
    #[schema(
        example = "{\"delegate_action\": {\"actions\": [{\"Transfer\": {\"deposit\": \"1\" }}], \"max_block_height\": 922790412, \"nonce\": 103066617000686, \"public_key\": \"ed25519:98GtfFzez3opomVpwa7i4m2nptHtc8Ha405XHMWszQtL\", \"receiver_id\": \"relayer.example.testnet\", \"sender_id\": \"example.testnet\" }, \"signature\": \"ed25519:4uJu8KapH98h8cQm4btE0DKnbiFXSZNT7McDw4LHy7pdAt4Mz8DfuyQZadGgFExo77or9152iwcw2q12rnFWa6bg\" }"
    )]
    signed_delegate_action: SignedDelegateAction,
}
impl Display for AccountIdAllowanceOauthSDAJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "account_id: {}, allowance in Gas: {}, oauth_token: {}, signed_delegate_action signature: {}",
            self.account_id, self.allowance, self.oauth_token, self.signed_delegate_action.signature
        ) // SignedDelegateAction doesn't implement display, so just displaying signature
    }
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
struct AccountIdAllowanceOauthJson {
    #[schema(example = "example.near")]
    account_id: String,
    #[schema(example = 900_000_000)]
    allowance: u64,
    #[schema(
        example = "https://securetoken.google.com/pagoda-oboarding-dev:Op4h13AQozM4CikngfHiFVC2xhf2"
    )]
    oauth_token: String,
}
impl Display for AccountIdAllowanceOauthJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "account_id: {}, allowance in Gas: {}, oauth_token: {}",
            self.account_id, self.allowance, self.oauth_token
        )
    }
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
struct AccountIdAllowanceJson {
    #[schema(example = "example.near")]
    account_id: String,
    #[schema(example = 900_000_000)]
    allowance: u64,
}
impl Display for AccountIdAllowanceJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "account_id: {}, allowance in Gas: {}",
            self.account_id, self.allowance
        )
    }
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
struct AccountIdJson {
    #[schema(example = "example.near")]
    account_id: String,
}
impl Display for AccountIdJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "account_id: {}", self.account_id)
    }
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
struct AllowanceJson {
    // TODO: LP use for return type of GET get_allowance
    #[schema(example = 900_000_000)]
    allowance_in_gas: u64,
}
impl Display for AllowanceJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "allowance in Gas: {}", self.allowance_in_gas)
    }
}

#[allow(dead_code)] // makes compiler happy - implicitly used as return types, so need to allow dead code
struct TransactionResult {
    status: FinalExecutionStatus,
    transaction_outcome: ExecutionOutcomeWithIdView,
    receipts_outcome: Vec<ExecutionOutcomeWithIdView>,
}

#[tokio::main]
async fn main() {
    // load config
    let config: RelayerConfiguration = Config::builder()
        .add_source(ConfigFile::with_name("config.toml"))
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap();

    // initialize tracing (aka logging)
    if config.flametrace_performance {
        setup_global_subscriber();
        info!("default tracing setup with flametrace performance ENABLED");
    } else {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .init();
        info!("default tracing setup with flametrace performance DISABLED");
    }

    // initialize RPC client and shared storage pool
    let rpc_client = Arc::new(config.network_config.rpc_client());
    let rpc_client_nofetch = Arc::new(config.network_config.raw_rpc_client());
    let mut shared_storage_pool = None;
    let mut redis_pool = None;

    // initialize our shared storage pool manager if using fastauth features or using shared storage
    if config.use_fastauth_features || config.use_shared_storage {
        #[cfg(any(feature = "fastauth_features", feature = "shared_storage"))]
        if config.use_fastauth_features || config.use_shared_storage {
            let pool = create_shared_storage_pool(&config, Arc::clone(&rpc_client));
            if let Err(err) = pool.check_and_spawn_pool().await {
                let err_msg = format!("Error initializing shared storage pool: {err}");
                error!("{err_msg}");
                tracing::error!(err_msg);
                return;
            } else {
                shared_storage_pool = Some(pool);
                info!("shared storage pool initialized");
            }
        }
    }

    // if fastauth enabled, initialize whitelisted senders with "infinite" allowance in relayer DB
    if config.use_fastauth_features {
        #[cfg(feature = "fastauth_features")]
        init_senders_infinite_allowance_fastauth(&config, &redis_pool.as_ref().unwrap()).await;
    }

    if config.use_redis {
        redis_pool = Some(create_redis_pool(&config));
    }

    let app_state = Arc::new(AppState {
        config: config.clone(),
        rpc_client,
        rpc_client_nofetch,
        shared_storage_pool,
        redis_pool,
    });

    //TODO: not secure, allow only for testnet, whitelist endpoint etc. for mainnet
    let cors_layer = tower_http::cors::CorsLayer::permissive();

    #[derive(OpenApi)]
    #[openapi(
        info(
            title = "relayer",
            description = "APIs for creating accounts, managing allowances, and relaying meta transactions. \
                    \n NOTE: the SignedDelegateAction is not supported by the openapi schema. \
                    \n Here's an example json of a SignedDelegateAction payload:\
                    \n ```{\"delegate_action\": {\"actions\": [{\"Transfer\": {\"deposit\": \"1\" }}], \"max_block_height\": 922790412, \"nonce\": 103066617000686, \"public_key\": \"ed25519:98GtfFzez3opomVpwa7i4m2nptHtc8Ha405XHMWszQtL\", \"receiver_id\": \"relayer.example.testnet\", \"sender_id\": \"example.testnet\" }, \"signature\": \"ed25519:4uJu8KapH98h8cQm4btE0DKnbiFXSZNT7McDw4LHy7pdAt4Mz8DfuyQZadGgFExo77or9152iwcw2q12rnFWa6bg\" }``` \
                    \n For more details on the SignedDelegateAction data structure, please see https://docs.rs/near-primitives/latest/near_primitives/delegate_action/struct.SignedDelegateAction.html or https://docs.near.org/develop/relayers/build-relayer#signing-a-delegated-transaction "
        ),
        paths(
            relay,
            send_meta_tx,
            create_account_atomic,
            get_allowance,
            update_allowance,
            update_all_allowances,
            register_account_and_allowance,
        ),
        components(
            schemas(
                RelayError,
                AllowanceJson,
                AccountIdJson,
                AccountIdAllowanceJson,
                AccountIdAllowanceOauthJson,
                AccountIdAllowanceOauthSDAJson,
            )
        ),
        tags((
            name = "relayer",
            description = "APIs for creating accounts, managing allowances, \
                                    and relaying meta transactions"
        )),
    )]
    struct ApiDoc;

    // build our application with a route
    let app = Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        // There is no need to create `RapiDoc::with_openapi` because the OpenApi is served
        // via SwaggerUi instead we only make rapidoc to point to the existing doc.
        .merge(RapiDoc::new("/api-docs/openapi.json").path("/rapidoc"))
        // Alternative to above
        // .merge(RapiDoc::with_openapi("/api-docs/openapi2.json", ApiDoc::openapi()).path("/rapidoc"))
        // `POST /relay` goes to `relay` handler function
        .route("/relay", post(relay))
        .route("/send_meta_tx", post(send_meta_tx))
        .route("/send_meta_tx_async", post(send_meta_tx_async))
        .route("/send_meta_tx_nopoll", post(send_meta_tx_nopoll))
        .route("/create_account_atomic", post(create_account_atomic))
        .route("/get_allowance", get(get_allowance))
        .route("/update_allowance", post(update_allowance))
        .route("/update_all_allowances", post(update_all_allowances))
        .route("/register_account", post(register_account_and_allowance))
        // See https://docs.rs/tower-http/0.1.1/tower_http/trace/index.html for more details.
        .layer(TraceLayer::new_for_http())
        .layer(cors_layer)
        .with_state(Arc::clone(&app_state));

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr: SocketAddr = SocketAddr::from((config.ip_address, config.port));
    info!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

fn setup_global_subscriber() -> impl Drop {
    let fmt_layer = tracing_subscriber::fmt::Layer::default();

    let (flame_layer, _guard) = FlameLayer::with_file("./tracing.folded").unwrap();

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(flame_layer)
        .init();
    _guard
}

#[cfg(feature = "fastauth_features")]
async fn init_senders_infinite_allowance_fastauth(
    config: &RelayerConfiguration,
    redis_pool: &Pool<RedisConnectionManager>,
) {
    let max_allowance = u64::MAX;
    for whitelisted_sender in config.whitelisted_senders.iter() {
        let redis_result =
            set_account_and_allowance_in_redis(redis_pool, whitelisted_sender, &max_allowance)
                .await;
        if let Err(err) = redis_result {
            error!(
                "Error setting allowance for account_id {} with allowance {} in Relayer DB: {:?}",
                whitelisted_sender, max_allowance, err,
            );
        } else {
            info!(
                "Set allowance for account_id {} with allowance {} in Relayer DB",
                whitelisted_sender, max_allowance,
            );
        }
    }
}

// NOTE: error in swagger-ui TypeError: Failed to execute 'fetch' on 'Window': Request with GET/HEAD method cannot have body.
#[utoipa::path(
    get,
    path = "/get_allowance",
    request_body = AccountIdJson,
    responses(
        (status = 200, description = "90000000000000", body = String),
        (status = 500, description = "Error getting allowance for account_id example.near in Relayer DB: err_msg", body = String)
    )
)]
#[instrument]
async fn get_allowance(
    State(state): State<Arc<AppState>>,
    account_id_json: Json<AccountIdJson>,
) -> impl IntoResponse {
    // convert str account_id val from json to AccountId so I can reuse get_remaining_allowance fn
    let Ok(account_id_val) = AccountId::from_str(&account_id_json.account_id) else {
        return (
            StatusCode::FORBIDDEN,
            format!("Invalid account_id: {}", account_id_json.account_id),
        )
            .into_response();
    };
    match get_remaining_allowance(&state.redis_pool.as_ref().unwrap(), &account_id_val).await {
        Ok(allowance) => (
            StatusCode::OK,
            allowance.to_string(), // TODO: LP return in json format
                                   // AllowanceJson {
                                   //     allowance_in_gas: allowance
                                   // }
        )
            .into_response(),
        Err(err) => {
            let err_msg = format!(
                "Error getting allowance for account_id {account_id_val} in Relayer DB: {err:?}"
            );
            error!("{err_msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response()
        }
    }
}

// TODO: LP how to get multiple 500 status messages to show up
#[utoipa::path(
    post,
    path = "/create_account_atomic",
    request_body = AccountIdAllowanceOauthSDAJson,
    responses(
        (status = 201, description = "Added Oauth token https://securetoken.google.com/pagoda-oboarding-dev:Op4h13AQozM4CikngfHiFVC2xhf2 for account_id example.near \
                            with allowance (in Gas) 90000000000000 to Relayer DB. \
                            Near onchain account creation response: {create_account_sda_result:?}", body = String),
        (status = 400, description = "Error: oauth_token https://securetoken.google.com/pagoda-oboarding-dev:Op4h13AQozM4CikngfHiFVC2xhf2 has already been used to register an account. You can only register 1 account per oauth_token", body = String),
        (status = 403, description = "Invalid account_id: invalid_account_id.near", body = String),
        (status = 500, description = "Error getting oauth_token for account_id example.near, oauth_token https://securetoken.google.com/pagoda-oboarding-dev:Op4h13AQozM4CikngfHiFVC2xhf2 in Relayer DB: err_msg", body = String),
        (status = 500, description = "Error creating account_id example.near with allowance 90000000000000 in Relayer DB:\nerr_msg", body = String),
        (status = 500, description = "Error allocating storage for account example.near: err_msg", body = String),
        (status = 500, description = "Error creating oauth token https://securetoken.google.com/pagoda-oboarding-dev:Op4h13AQozM4CikngfHiFVC2xhf2 in Relayer DB:\n{err:?}", body = String),
    ),
)]
#[instrument]
async fn create_account_atomic(
    State(state): State<Arc<AppState>>,
    account_id_allowance_oauth_sda: Json<AccountIdAllowanceOauthSDAJson>,
) -> impl IntoResponse {
    /*
    This function atomically creates an account, both in our systems (redis)
    and on chain created both an on chain account and adding that account to the storage pool

    Motivation for doing this is when calling /register_account_and_allowance and then /send_meta_tx and
    /register_account_and_allowance succeeds, but /send_meta_tx fails, then the account is now
    unable to use the relayer without manual intervention deleting the record from redis
     */

    // get individual vars from json object
    let account_id: &String = &account_id_allowance_oauth_sda.account_id;
    let allowance_in_gas: &u64 = &account_id_allowance_oauth_sda.allowance;
    let oauth_token: &String = &account_id_allowance_oauth_sda.oauth_token;
    let sda: &SignedDelegateAction = &account_id_allowance_oauth_sda.signed_delegate_action;

    /*
       do logic similar to register_account_and_allowance fn
       without updating redis or allocating shared storage
       if that fails, return error
       if it succeeds, then continue
    */

    // check if the oauth_token has already been used and is a key in Redis
    match get_oauth_token_in_redis(&state.redis_pool.as_ref().unwrap(), oauth_token).await {
        Ok(is_oauth_token_in_redis) => {
            if is_oauth_token_in_redis {
                let err_msg = format!(
                    "Error: oauth_token {oauth_token} has already been used to register an account. \
                    You can only register 1 account per oauth_token",
                );
                warn!("{err_msg}");
                return (StatusCode::BAD_REQUEST, err_msg).into_response();
            }
        }
        Err(err) => {
            let err_msg = format!(
                "Error getting oauth_token for account_id {account_id}, oauth_token {oauth_token} in Relayer DB: {err:?}",
            );
            error!("{err_msg}");
            return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
        }
    }
    let redis_result = set_account_and_allowance_in_redis(
        &state.redis_pool.as_ref().unwrap(),
        account_id,
        allowance_in_gas,
    )
    .await;
    let Ok(_) = redis_result else {
        let err_msg = format!(
            "Error creating account_id {account_id} with allowance {allowance_in_gas} in Relayer DB:\n{redis_result:?}"
        );
        error!("{err_msg}");
        return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
    };

    /*
       call process_signed_delegate_action fn
       if there's an error, then return error
       if it succeeds, then add oauth token to redis and allocate shared storage
       after updated redis and adding shared storage, finally return success msg
    */
    let create_account_sda_result = if state.config.use_fastauth_features {
        process_signed_delegate_action_big_timeout(state.as_ref(), sda.clone()).await
    } else {
        process_signed_delegate_action(state.as_ref(), sda).await
    };

    if let Err(err) = create_account_sda_result {
        return (err.status_code, err.message).into_response();
    }
    #[allow(unused_variables)] // makes compiler happy - used by err_msg
    let Ok(account_id) = account_id.parse::<AccountId>() else {
        let err_msg = format!("Invalid account_id: {account_id}");
        warn!("{err_msg}");
        return (StatusCode::BAD_REQUEST, err_msg).into_response();
    };

    // allocate shared storage for account_id if shared storage is being used
    if state.config.use_fastauth_features || state.config.use_shared_storage {
        #[cfg(any(feature = "fastauth_features", feature = "shared_storage"))]
        if let Err(err) = state
            .shared_storage_pool
            .as_ref()
            .unwrap()
            .allocate_default(account_id.clone())
            .await
        {
            let err_msg = format!("Error allocating storage for account {account_id}: {err:?}");
            error!("{err_msg}");
            return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
        }
    }

    // add oauth token to redis (key: oauth_token, val: true)
    match set_oauth_token_in_redis(&state.redis_pool.as_ref().unwrap(), oauth_token).await {
        Ok(_) => {
            let ok_msg = format!(
                "Added Oauth token {oauth_token:?} for account_id {account_id:?} \
                with allowance (in Gas) {allowance_in_gas:?} to Relayer DB. \
                Near onchain account creation response: {create_account_sda_result:?}"
            );
            info!("{ok_msg}");
            (StatusCode::CREATED, ok_msg).into_response()
        }
        Err(err) => {
            let err_msg =
                format!("Error creating oauth token {oauth_token:?} in Relayer DB:\n{err:?}",);
            error!("{err_msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response()
        }
    }
}

#[utoipa::path(
post,
    path = "/update_allowance",
    request_body = AccountIdAllowanceJson,
    responses(
        (status = 201, description = "Relayer DB updated for {account_id: example.near,allowance: 90000000000000}", body = String),
        (status = 500, description = "Error updating account_id example.near with allowance 90000000000000 in Relayer DB:\
                    \n{db_result:?}", body = String),
    ),
)]
#[instrument]
async fn update_allowance(
    State(state): State<Arc<AppState>>,
    account_id_allowance: Json<AccountIdAllowanceJson>,
) -> impl IntoResponse {
    let account_id: &String = &account_id_allowance.account_id;
    let allowance_in_gas: &u64 = &account_id_allowance.allowance;

    let redis_result = set_account_and_allowance_in_redis(
        &state.redis_pool.as_ref().unwrap(),
        account_id,
        allowance_in_gas,
    )
    .await;
    let Ok(_) = redis_result else {
        let err_msg = format!(
            "Error updating account_id {account_id} with allowance {allowance_in_gas} in Relayer DB:\
            \n{redis_result:?}"
        );
        warn!("{err_msg}");
        return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
    };
    let success_msg: String = format!("Relayer DB updated for {account_id_allowance:?}");
    info!("success_msg");
    (StatusCode::CREATED, success_msg).into_response()
}

#[utoipa::path(
    post,
    path = "/update_all_allowances",
    request_body = AllowanceJson,
    responses(
        (status = 200, description = "Updated 321 keys in Relayer DB", body = String),
        (status = 500, description = "Error updating allowance for key example.near: err_msg", body = String),
    ),
)]
#[instrument]
async fn update_all_allowances(
    State(state): State<Arc<AppState>>,
    Json(allowance_json): Json<AllowanceJson>,
) -> impl IntoResponse {
    let allowance_in_gas = allowance_json.allowance_in_gas;
    let redis_response = update_all_allowances_in_redis(
        &state.redis_pool.as_ref().unwrap(),
        &state.config.network_env,
        allowance_in_gas,
    )
    .await;
    match redis_response {
        Ok(response) => response.into_response(),
        Err(err) => (err.status_code, err.message).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/register_account_and_allowance",
    request_body = AccountIdAllowanceOauthJson,
    responses(
        (status = 201, description = "Added Oauth token {oauth_token: https://securetoken.google.com/pagoda-oboarding-dev:Op4h13AQozM4CikngfHiFVC2xhf2, account_id: example.near, allowance: 90000000000000 to Relayer DB", body = String),
        (status = 500, description = "Error: oauth_token https://securetoken.google.com/pagoda-oboarding-dev:Op4h13AQozM4CikngfHiFVC2xhf2 has already been used to register an account. \
                            You can only register 1 account per oauth_token", body = String),
    ),
)]
#[instrument]
async fn register_account_and_allowance(
    State(state): State<Arc<AppState>>,
    account_id_allowance_oauth: Json<AccountIdAllowanceOauthJson>,
) -> impl IntoResponse {
    let account_id: &String = &account_id_allowance_oauth.account_id;
    let allowance_in_gas: &u64 = &account_id_allowance_oauth.allowance;
    let oauth_token: &String = &account_id_allowance_oauth.oauth_token;
    // check if the oauth_token has already been used and is a key in Relayer DB
    match get_oauth_token_in_redis(&state.redis_pool.as_ref().unwrap(), oauth_token).await {
        Ok(is_oauth_token_in_redis) => {
            if is_oauth_token_in_redis {
                let err_msg = format!(
                    "Error: oauth_token {oauth_token} has already been used to register an account. \
                    You can only register 1 account per oauth_token",
                );
                warn!("{err_msg}");
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
        &state.redis_pool.as_ref().unwrap(),
        account_id,
        allowance_in_gas,
    )
    .await;
    let Ok(_) = redis_result else {
        let err_msg = format!(
            "Error creating account_id {account_id} with allowance {allowance_in_gas} in Relayer DB:\
            \n{redis_result:?}");
        error!("{err_msg}");
        return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
    };

    #[allow(unused_variables)] // makes compiler happy - used by err_msg
    let Ok(account_id) = account_id.parse::<AccountId>() else {
        let err_msg = format!("Invalid account_id: {account_id}");
        warn!("{err_msg}");
        return (StatusCode::BAD_REQUEST, err_msg).into_response();
    };
    #[cfg(feature = "shared_storage")]
    if let Some(ref shared_storage_pool) = state.shared_storage_pool {
        if let Err(err) = shared_storage_pool
            .allocate_default(account_id.clone())
            .await
        {
            let err_msg = format!("Error allocating storage for account {account_id}: {err:?}");
            error!("{err_msg}");
            return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
        }
    }

    // add oauth token to redis (key: oauth_token, val: true)
    match set_oauth_token_in_redis(&state.redis_pool.as_ref().unwrap(), oauth_token).await {
        Ok(_) => {
            let ok_msg = format!("Added Oauth token {account_id_allowance_oauth:?} to Relayer DB");
            info!("{ok_msg}");
            (StatusCode::CREATED, ok_msg).into_response()
        }
        Err(err) => {
            let err_msg =
                format!("Error creating oauth token {oauth_token:?} in Relayer DB:\n{err:?}",);
            error!("{err_msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response()
        }
    }
}

#[utoipa::path(
    post,
    path = "/relay",
    request_body = Vec<u8>,
    responses(
    (status = 201, description = "--DEPRECATED--Relayed and sent transaction ...", body = String),
    (status = 400, description = "--DEPRECATED--Error deserializing payload data object ...", body = String),
    (status = 500, description = "--DEPRECATED--Error signing transaction: ...", body = String),
    ),
)]
#[instrument]
async fn relay(State(state): State<Arc<AppState>>, data: Json<Vec<u8>>) -> impl IntoResponse {
    // deserialize SignedDelegateAction using borsh
    match SignedDelegateAction::try_from_slice(&data.0) {
        Ok(signed_delegate_action) => {
            match process_signed_delegate_action(state.as_ref(), &signed_delegate_action).await {
                Ok(response) => response.into_response(),
                Err(err) => (err.status_code, err.message).into_response(),
            }
        }
        Err(e) => {
            let err_msg = format!("Error deserializing payload data object: {e:?}");
            warn!("{err_msg}");
            (StatusCode::BAD_REQUEST, err_msg).into_response()
        }
    }
}

#[utoipa::path(
    post,
    path = "/send_meta_tx",
    request_body = SignedDelegateAction,
    responses(
        (status = 201, description = "Relayed and sent transaction ...", body = String),
        (status = 400, description = "Error deserializing payload data object ...", body = String),
        (status = 500, description = "Error signing transaction: ...", body = String),
    ),
)]
#[instrument]
async fn send_meta_tx(
    State(state): State<Arc<AppState>>,
    data: Json<SignedDelegateAction>,
) -> impl IntoResponse {
    let relayer_response = process_signed_delegate_action(
        &state, // deserialize SignedDelegateAction using serde json
        &data.0,
    )
    .await;
    match relayer_response {
        Ok(response) => response.into_response(),
        Err(err) => (err.status_code, err.message).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/send_meta_tx_nopoll",
    request_body = SignedDelegateAction,
    responses(
        (status = 201, description = "Relayed and sent transaction ...", body = String),
        (status = 400, description = "Error deserializing payload data object ...", body = String),
        (status = 500, description = "Error signing transaction: ...", body = String),
    ),
)]
async fn send_meta_tx_nopoll(
    State(state): State<Arc<AppState>>,
    data: Json<SignedDelegateAction>,
) -> impl IntoResponse {
    let relayer_response =
        process_signed_delegate_action_noretry_async(state.as_ref(), data.0).await;
    match relayer_response {
        Ok(response) => response.into_response(),
        Err(err) => (err.status_code, err.message).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/send_meta_tx_async",
    request_body = SignedDelegateAction,
    responses(
        (status = 200, description = "transaction hash", body = String),
        (status = 400, description = "Error deserializing payload data object ...", body = String),
        (status = 500, description = "Error signing transaction: ...", body = String),
    ),
)]
async fn send_meta_tx_async(
    State(state): State<Arc<AppState>>,
    data: Json<SignedDelegateAction>,
) -> impl IntoResponse {
    // Directly await the asynchronous operation without detaching it as a separate task.
    let relayer_response =
        process_signed_delegate_action_noretry_async(state.as_ref(), data.0).await;

    // Generate the response based on the outcome of the above operation.
    let response = match relayer_response {
        Ok(response) => {
            // `response` is the tx hash
            response.into_response()
        }
        Err(err) => (err.status_code, err.message).into_response(),
    };
    debug!("Async Relayer response: {:?}", response);

    response
}

#[instrument]
async fn process_signed_delegate_action(
    state: &AppState,
    signed_delegate_action: &SignedDelegateAction,
) -> Result<String, RelayError> {
    filter_and_send_signed_delegate_action(
        state,
        signed_delegate_action.clone(),
        |receiver_id, actions| async move {
            match state
                .rpc_client
                .send_tx(&*SIGNER, &receiver_id, actions)
                .await
            {
                Err(err) => {
                    let err_msg: String = format!("Error signing transaction: {:?}", err);
                    error!("{err_msg}");
                    Err(err_msg)
                }
                Ok(FinalExecutionOutcomeView {
                    status,
                    transaction_outcome,
                    receipts_outcome,
                    ..
                }) => Ok(TransactionResult {
                    status,
                    transaction_outcome,
                    receipts_outcome,
                }),
            }
        },
    )
    .await
}

// TODO remove this when `send_tx` jsonrpc method is live in 1.37 release
async fn process_signed_delegate_action_big_timeout(
    state: &AppState,
    signed_delegate_action: SignedDelegateAction,
) -> Result<String, RelayError> {
    filter_and_send_signed_delegate_action(
        state,
        signed_delegate_action,
        |receiver_id, actions| async move {
            let hash = state
                .rpc_client
                .send_tx_async(&*SIGNER, &receiver_id, actions)
                .await
                .map_err(|err| {
                    let err_msg: String = format!("Error signing transaction: {:?}", err);
                    error!("{err_msg}");
                    err_msg
                })?;

            let mut last_res = None;
            const MAX_RETRIES: usize = 300;
            for _retry_no in 1..=MAX_RETRIES {
                let second = Duration::from_secs(1);
                let res = match state
                    .rpc_client
                    .tx_async_status(SIGNER.account_id(), hash)
                    .await
                {
                    Ok(res) => res,
                    // The node is unstable, wait a second and try again
                    Err(_) => {
                        sleep(second).await;
                        continue;
                    }
                };
                last_res = Some(res.clone());
                match res.status {
                    FinalExecutionStatus::NotStarted => sleep(second).await,
                    FinalExecutionStatus::Started => sleep(second).await,
                    FinalExecutionStatus::Failure(_) => break,
                    FinalExecutionStatus::SuccessValue(_) => break,
                }
            }
            if let Some(FinalExecutionOutcomeView {
                status,
                transaction_outcome,
                receipts_outcome,
                ..
            }) = last_res
            {
                Ok(TransactionResult {
                    status,
                    transaction_outcome,
                    receipts_outcome,
                })
            } else {
                error!("Tried {MAX_RETRIES} times, failed {MAX_RETRIES} times");
                Err(format!("Failed after {MAX_RETRIES} retries"))
            }
        },
    )
    .await
}

async fn process_signed_delegate_action_noretry_async(
    state: &AppState,
    signed_delegate_action: SignedDelegateAction,
) -> Result<String, RelayError> {
    let result: Result<String, RelayError> = filter_and_send_signed_delegate_action_async(
        state,
        signed_delegate_action,
        |receiver_id, actions| async move {
            let (nonce, block_hash, _) = state
                .rpc_client
                .fetch_nonce(SIGNER.account_id(), SIGNER.public_key())
                .await
                .map_err(|e| format!("Error fetching nonce: {:?}", e))?;
            let txn_hash = state
                .rpc_client_nofetch
                .call(&RpcBroadcastTxAsyncRequest {
                    signed_transaction: Transaction {
                        nonce,
                        block_hash,
                        signer_id: SIGNER.account_id().clone(),
                        public_key: SIGNER.public_key().clone(),
                        receiver_id: receiver_id.clone(),
                        actions: actions.clone(),
                    }
                    .sign(SIGNER.current_signer()),
                })
                .await
                .map_err(|err| {
                    let err_msg: String = format!("Error signing transaction: {:?}", err);
                    error!("{err_msg}");
                    err_msg
                })?;
            Ok(txn_hash)
        },
    )
    .await;

    // No need to explicitly drop `signer` as it will be automatically dropped at the end of the scope
    match result {
        Ok(txn_hash) => Ok(txn_hash),
        Err(err) => Err(err),
    }
}

fn validate_signed_delegate_action(
    state: &AppState,
    signed_delegate_action: &SignedDelegateAction,
) -> Result<(), RelayError> {
    // the receiver of the txn is the sender of the signed delegate action
    let receiver_id = &signed_delegate_action.delegate_action.sender_id;
    let da_receiver_id = &signed_delegate_action.delegate_action.receiver_id;

    // if we are not using whitelisted contracts or senders, then no validation needed
    if !state.config.use_whitelisted_contracts && !state.config.use_whitelisted_senders {
        return Ok(());
    }

    // check that the delegate action receiver_id is in the whitelisted_contracts
    let is_whitelisted_da_receiver = state
        .config
        .whitelisted_contracts
        .iter()
        .any(|s| s.as_str() == da_receiver_id.as_str());
    if state.config.use_exchange.clone() {
        let non_delegate_actions: Vec<Action> =
            signed_delegate_action.delegate_action.get_actions();
        if state.config.use_whitelisted_senders {
            // check if the delegate action receiver_id (account sender_id) if a whitelisted delegate action receiver
            let is_whitelisted_sender = state
                .config
                .whitelisted_senders
                .iter()
                .any(|s| s == receiver_id.as_str());
            if !is_whitelisted_sender {
                return Err(RelayError {
                    status_code: StatusCode::BAD_REQUEST,
                    message: format!(
                        "Delegate Action Sender_id {receiver_id:?} is not whitelisted"
                    ),
                });
            }
        }
        for non_delegate_action in non_delegate_actions {
            match non_delegate_action.clone() {
                Action::FunctionCall(FunctionCallAction {
                    method_name,
                    deposit,
                    ..
                }) => {
                    debug!("method_name: {:?}", method_name);
                    debug!("deposit: {:?}", deposit);
                    if !is_whitelisted_da_receiver {
                        return Err(RelayError {
                            status_code: StatusCode::BAD_REQUEST,
                            message: format!(
                                "Delegate Action Sender_id {receiver_id:?} is not whitelisted"
                            ),
                        });
                    }
                    match method_name.as_str() {
                        FT_TRANSFER_METHOD_NAME => {
                            if deposit != FT_TRANSFER_ATTACHMENT_DEPOSIT_AMOUNT {
                                return Err(RelayError {
                                    status_code: StatusCode::BAD_REQUEST,
                                    message: "Ft transfer requires 1 yocto attached.".to_string(),
                                });
                            }
                        }
                        STORAGE_DEPOSIT_METHOD_NAME => {
                            if deposit != STORAGE_DEPOSIT_AMOUNT_FT {
                                return Err(RelayError {
                                    status_code: StatusCode::BAD_REQUEST,
                                    message: "Attached less or more than allowed storage_deposit amount allowed.".to_string(),
                                });
                            }
                        }
                        _ => {
                            return Err(RelayError {
                                status_code: StatusCode::BAD_REQUEST,
                                message: "Method name not allowed.".to_string(),
                            });
                        }
                    }
                }
                Action::CreateAccount(_) => debug!("CreateAccount action"),
                Action::Transfer(_) => {
                    return Err(RelayError {
                        status_code: StatusCode::BAD_REQUEST,
                        message: "Transfer action type is not allowed.".to_string(),
                    })
                }
                _ => {
                    return Err(RelayError {
                        status_code: StatusCode::BAD_REQUEST,
                        message: "This action type is not allowed.".to_string(),
                    })
                }
            }
        }
    }
    if !state.config.use_fastauth_features
        && !is_whitelisted_da_receiver
        && state.config.use_whitelisted_contracts
    {
        let err_msg = format!("Delegate Action receiver_id {da_receiver_id} is not whitelisted",);
        warn!("{err_msg}");
        return Err(RelayError {
            status_code: StatusCode::BAD_REQUEST,
            message: err_msg,
        });
    }
    // check the sender_id in whitelist if applicable
    if state.config.use_whitelisted_senders && !state.config.use_fastauth_features {
        // check if the delegate action receiver_id (account sender_id) if a whitelisted delegate action receiver
        let is_whitelisted_sender = state
            .config
            .whitelisted_senders
            .iter()
            .any(|s| s == receiver_id.as_str());
        if !is_whitelisted_sender {
            let err_msg = format!(
                "Delegate Action receiver_id {da_receiver_id} or sender_id {receiver_id} is not whitelisted",
            );
            warn!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::BAD_REQUEST,
                message: err_msg,
            });
        }
    }
    if !is_whitelisted_da_receiver
        && state.config.use_fastauth_features
        && state.config.use_whitelisted_contracts
    {
        // check if sender id and receiver id are the same AND (AddKey or DeleteKey action)
        let non_delegate_action = signed_delegate_action
            .delegate_action
            .actions
            .get(0)
            .ok_or_else(|| {
                let err_msg = "DelegateAction must have at least one NonDelegateAction";
                warn!("{err_msg}");
                RelayError {
                    status_code: StatusCode::BAD_REQUEST,
                    message: err_msg.to_string(),
                }
            })?;

        let contains_key_action = matches!(
            (*non_delegate_action).clone().into(),
            Action::AddKey(_) | Action::DeleteKey(_)
        );
        // check if the receiver_id (delegate action sender_id) if a whitelisted delegate action receiver
        let is_whitelisted_sender = state
            .config
            .whitelisted_senders
            .iter()
            .any(|s| s == receiver_id.as_str());
        if (receiver_id != da_receiver_id || !contains_key_action) && !is_whitelisted_sender {
            let err_msg = format!(
                "Delegate Action receiver_id {da_receiver_id} or sender_id {receiver_id} is not whitelisted OR \
                (they do not match AND the NonDelegateAction is not AddKey or DeleteKey)",
            );
            warn!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::BAD_REQUEST,
                message: err_msg,
            });
        }
    }

    // Check if the SignedDelegateAction includes a FunctionCallAction that transfers FTs to BURN_ADDRESS
    if state.config.use_pay_with_ft {
        let non_delegate_actions = signed_delegate_action.delegate_action.get_actions();
        let treasury_payments: Vec<Action> = non_delegate_actions
            .into_iter()
            .filter(|action| {
                if let Action::FunctionCall(FunctionCallAction { ref args, .. }) = action {
                    debug!("args: {:?}", args);

                    // convert to ascii lowercase
                    let args_ascii = args.to_ascii_lowercase();
                    debug!("args_ascii: {:?}", args_ascii);

                    // Convert to UTF-8 string
                    let args_str = String::from_utf8_lossy(&args_ascii);
                    debug!("args_str: {:?}", args_str);

                    // Parse to JSON (assuming args are serialized as JSON)
                    let args_json: Value = serde_json::from_str(&args_str).unwrap_or_else(|err| {
                        error!("Failed to parse JSON: {}", err);
                        // Provide a default Value
                        Value::Null
                    });
                    debug!("args_json: {:?}", args_json);

                    // get the receiver_id from the json without the escape chars
                    let receiver_id = args_json["receiver_id"].as_str().unwrap_or_default();
                    debug!("receiver_id: {receiver_id}");
                    let burn_address = state.config.burn_address.to_string();
                    debug!("BURN_ADDRESS.to_string(): {burn_address:?}");

                    // Check if receiver_id in args contain BURN_ADDRESS
                    if receiver_id == burn_address {
                        debug!("SignedDelegateAction contains the BURN_ADDRESS MATCH");
                        return true;
                    }
                }

                false
            })
            .collect();
        if treasury_payments.is_empty() {
            let err_msg = "No treasury payment found in this transaction";
            warn!("{err_msg}");
            Err(RelayError {
                status_code: StatusCode::BAD_REQUEST,
                message: err_msg.to_string(),
            })
        } else {
            Ok(())
        }
    } else {
        Ok(())
    }
}

// TODO refactor out common filtering code
async fn filter_and_send_signed_delegate_action_async<F>(
    state: &AppState,
    signed_delegate_action: SignedDelegateAction,
    _f: impl Fn(AccountId, Vec<Action>) -> F,
) -> Result<String, RelayError>
where
    F: Future<Output = Result<CryptoHash, String>>,
{
    debug!(
        "Deserialized SignedDelegateAction object: {:#?}",
        signed_delegate_action
    );

    let validation_result: Result<(), RelayError> =
        validate_signed_delegate_action(state, &signed_delegate_action);
    if let Err(err) = validation_result {
        return Err(err);
    }

    let receiver_id: &AccountId = &signed_delegate_action.delegate_action.receiver_id;
    let actions: Vec<Action> = vec![Action::Delegate(signed_delegate_action.clone())];
    let txn_hash: CryptoHash = state
        .rpc_client
        .send_tx_async(&*SIGNER, receiver_id, actions)
        .await
        .map_err(|err| {
            let err_msg = format!("Error signing transaction: {err:?}");
            error!("{err_msg}");
            RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: err_msg,
            }
        })?;

    // TODO we have no idea how much gas is being burnt in this txn - can't use async with redis

    Ok(txn_hash.to_string())
}

async fn filter_and_send_signed_delegate_action<F>(
    state: &AppState,
    signed_delegate_action: SignedDelegateAction,
    _f: impl Fn(AccountId, Vec<Action>) -> F,
) -> Result<String, RelayError>
where
    F: Future<Output = Result<TransactionResult, String>>,
{
    debug!(
        "Deserialized SignedDelegateAction object: {:#?}",
        signed_delegate_action
    );

    let validation_result: Result<(), RelayError> =
        validate_signed_delegate_action(state, &signed_delegate_action);
    if let Err(err) = validation_result {
        return Err(err);
    }

    let signer_account_id: &AccountId = &signed_delegate_action.delegate_action.sender_id;
    let receiver_id: &AccountId = &signed_delegate_action.delegate_action.receiver_id;
    let actions: Vec<Action> = vec![Action::Delegate(signed_delegate_action.clone())];

    // gas allowance redis specific validation
    if state.config.use_redis {
        // Check the sender's remaining gas allowance in Redis
        let end_user_account: &AccountId = &signed_delegate_action.delegate_action.sender_id;
        let remaining_allowance: u64 =
            get_remaining_allowance(&state.redis_pool.clone().unwrap(), end_user_account)
                .await
                .unwrap_or(0);
        if remaining_allowance < TXN_GAS_ALLOWANCE {
            let err_msg = format!(
                "AccountId {} does not have enough remaining gas allowance.",
                end_user_account.as_str()
            );
            error!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::BAD_REQUEST,
                message: err_msg,
            });
        }

        let execution = state
            .rpc_client
            .send_tx(&*SIGNER, receiver_id, actions)
            .await
            .map_err(|err| {
                let err_msg = format!("Error signing transaction: {err:?}");
                error!("{err_msg}");
                RelayError {
                    status_code: StatusCode::INTERNAL_SERVER_ERROR,
                    message: err_msg,
                }
            })?;

        let status = &execution.status;
        let response_msg = match status {
            FinalExecutionStatus::Failure(_) => "Error sending transaction",
            _ => "Relayed and sent transaction",
        };
        let status_msg = json!({
            "message": response_msg,
            "status": &execution.status,
            "Transaction Outcome": &execution.transaction_outcome,
            "Receipts Outcome": &execution.receipts_outcome,
        });

        let gas_used_in_yn =
            calculate_total_gas_burned(&execution.transaction_outcome, &execution.receipts_outcome);
        debug!("total gas burnt in yN: {}", gas_used_in_yn);
        let new_allowance = update_remaining_allowance(
            &state.redis_pool.clone().unwrap(),
            &signer_account_id,
            gas_used_in_yn,
            remaining_allowance,
        )
        .await
        .map_err(|err| {
            let err_msg = format!("Updating redis remaining allowance errored out: {err:?}");
            error!("{err_msg}");
            RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: err_msg,
            }
        })?;
        info!("Updated remaining allowance for account {signer_account_id}: {new_allowance}",);

        if let FinalExecutionStatus::Failure(_) = status {
            error!("Error message: \n{status_msg:?}");
            Err(RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: status_msg.to_string(),
            })
        } else {
            info!("Success message: \n{status_msg:?}");
            Ok(status_msg.to_string())
        }
    } else {
        let execution = state
            .rpc_client
            .send_tx(&*SIGNER, receiver_id, actions)
            .await
            .map_err(|err| {
                let err_msg = format!("Error signing transaction: {err:?}");
                error!("{err_msg}");
                RelayError {
                    status_code: StatusCode::INTERNAL_SERVER_ERROR,
                    message: err_msg,
                }
            })?;

        let status = &execution.status;
        let response_msg = match status {
            FinalExecutionStatus::Failure(_) => "Error sending transaction",
            _ => "Relayed and sent transaction",
        };
        let status_msg = json!({
            "message": response_msg,
            "status": &execution.status,
            "Transaction Outcome": &execution.transaction_outcome,
            "Receipts Outcome": &execution.receipts_outcome,
        });

        if let FinalExecutionStatus::Failure(_) = status {
            error!("Error message: \n{status_msg:?}");
            Err(RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: status_msg.to_string(),
            })
        } else {
            info!("Success message: \n{status_msg:?}");
            Ok(status_msg.to_string())
        }
    }
}

pub async fn get_redis_cnxn(
    redis_pool: &Pool<RedisConnectionManager>,
) -> Result<PooledConnection<RedisConnectionManager>, RedisError> {
    let conn_result = redis_pool.get();
    let conn: PooledConnection<RedisConnectionManager> = match conn_result {
        Ok(conn) => conn,
        Err(e) => {
            let err_msg = "Error getting Relayer DB connection from the pool";
            error!("{err_msg}");
            return Err(RedisError::from((IoError, err_msg, e.to_string())));
        }
    };
    Ok(conn)
}

pub async fn update_all_allowances_in_redis(
    redis_pool: &Pool<RedisConnectionManager>,
    network_env: &str,
    allowance_in_gas: u64,
) -> Result<String, RelayError> {
    // Get a connection to Redis from the pool
    let mut redis_conn = match get_redis_cnxn(redis_pool).await {
        Ok(conn) => conn,
        Err(e) => {
            let err_msg = format!("Error getting Relayer DB connection from the pool: {e}");
            error!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: err_msg,
            });
        }
    };

    // Fetch all keys that match the network env (.near for mainnet, .testnet for testnet, etc)
    let network = match network_env {
        "mainnet" => "near",
        a => a,
    };
    let pattern = format!("*.{network}");
    let keys: Vec<String> = match redis_conn.keys(pattern) {
        Ok(keys) => keys,
        Err(e) => {
            let err_msg = format!("Error fetching keys from Relayer DB: {e}");
            error!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: err_msg,
            });
        }
    };

    // Iterate through the keys and update their values to the provided allowance in gas
    for key in &keys {
        match redis_conn.set::<_, _, ()>(key, allowance_in_gas) {
            Ok(_) => info!("Updated allowance for key {}", key),
            Err(e) => {
                let err_msg = format!("Error updating allowance for key {key}: {e}");
                error!("{err_msg}");
                return Err(RelayError {
                    status_code: StatusCode::INTERNAL_SERVER_ERROR,
                    message: err_msg,
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
