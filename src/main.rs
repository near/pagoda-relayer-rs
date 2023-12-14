mod error;
mod redis_fns;
mod rpc_conf;
mod shared_storage;

use axum::{
    extract::Json,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use config::{Config, File as ConfigFile};
use near_crypto::InMemorySigner;
use near_fetch::signer::KeyRotatingSigner;
use near_primitives::borsh::BorshDeserialize;
use near_primitives::delegate_action::SignedDelegateAction;
use near_primitives::transaction::{Action, FunctionCallAction};
use near_primitives::types::AccountId;
use once_cell::sync::Lazy;
use r2d2::{Pool, PooledConnection};
use r2d2_redis::redis::{Commands, ErrorKind::IoError, RedisError};
use r2d2_redis::RedisConnectionManager;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::{Debug, Display, Formatter};
use std::net::SocketAddr;
use std::str::FromStr;
use std::string::ToString;
use std::{fmt, path::Path};
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, instrument, warn};
use tracing_flame::FlameLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use utoipa::{OpenApi, ToSchema};
use utoipa_rapidoc::RapiDoc;
use utoipa_swagger_ui::SwaggerUi;

use crate::error::RelayError;
use crate::redis_fns::{
    calculate_total_gas_burned, get_oauth_token_in_redis, get_remaining_allowance,
    set_account_and_allowance_in_redis, set_oauth_token_in_redis, update_remaining_allowance,
};
use crate::rpc_conf::NetworkConfig;
use crate::shared_storage::SharedStoragePoolManager;

// transaction cost in Gas (10^21yN or 10Tgas or 0.001N)
const TXN_GAS_ALLOWANCE: u64 = 10_000_000_000_000;
const YN_TO_GAS: u128 = 1_000_000_000;

// load config from toml and setup json rpc client
static LOCAL_CONF: Lazy<Config> = Lazy::new(|| {
    Config::builder()
        .add_source(ConfigFile::with_name("config.toml"))
        .build()
        .unwrap()
});
static NETWORK_ENV: Lazy<String> = Lazy::new(|| LOCAL_CONF.get("network").unwrap());
static RPC_CLIENT: Lazy<near_fetch::Client> = Lazy::new(|| {
    let network_config = NetworkConfig {
        rpc_url: LOCAL_CONF.get("rpc_url").unwrap(),
        rpc_api_key: LOCAL_CONF.get("rpc_api_key").unwrap(),
        wallet_url: LOCAL_CONF.get("wallet_url").unwrap(),
        explorer_transaction_url: LOCAL_CONF.get("explorer_transaction_url").unwrap(),
    };
    network_config.rpc_client()
});
static IP_ADDRESS: Lazy<[u8; 4]> = Lazy::new(|| LOCAL_CONF.get("ip_address").unwrap());
static PORT: Lazy<u16> = Lazy::new(|| LOCAL_CONF.get("port").unwrap());
static RELAYER_ACCOUNT_ID: Lazy<String> =
    Lazy::new(|| LOCAL_CONF.get("relayer_account_id").unwrap());
static SHARED_STORAGE_ACCOUNT_ID: Lazy<String> =
    Lazy::new(|| LOCAL_CONF.get("shared_storage_account_id").unwrap());
static SIGNER: Lazy<KeyRotatingSigner> = Lazy::new(|| {
    let path = LOCAL_CONF
        .get::<String>("keys_filename")
        .expect("Failed to read 'keys_filename' from config");
    let keys_file = std::fs::File::open(path).expect("Failed to open keys file");
    let signers: Vec<InMemorySigner> =
        serde_json::from_reader(keys_file).expect("Failed to parse keys file");

    KeyRotatingSigner::from_signers(signers)
});
static SHARED_STORAGE_KEYS_FILENAME: Lazy<String> =
    Lazy::new(|| LOCAL_CONF.get("shared_storage_keys_filename").unwrap());
static WHITELISTED_CONTRACTS: Lazy<Vec<String>> =
    Lazy::new(|| LOCAL_CONF.get("whitelisted_contracts").unwrap());
static USE_WHITELISTED_DELEGATE_ACTION_RECEIVER_IDS: Lazy<bool> = Lazy::new(|| {
    LOCAL_CONF
        .get("use_whitelisted_delegate_action_receiver_ids")
        .unwrap_or(false)
});
static WHITELISTED_DELEGATE_ACTION_RECEIVER_IDS: Lazy<Vec<String>> = Lazy::new(|| {
    LOCAL_CONF
        .get("whitelisted_delegate_action_receiver_ids")
        .unwrap()
});
static USE_REDIS: Lazy<bool> = Lazy::new(|| LOCAL_CONF.get("use_redis").unwrap_or(false));
static REDIS_POOL: Lazy<Pool<RedisConnectionManager>> = Lazy::new(|| {
    let redis_cnxn_url: String = LOCAL_CONF.get("redis_url").unwrap();
    let manager = RedisConnectionManager::new(redis_cnxn_url).unwrap();
    Pool::builder().build(manager).unwrap()
});
static USE_FASTAUTH_FEATURES: Lazy<bool> =
    Lazy::new(|| LOCAL_CONF.get("use_fastauth_features").unwrap_or(false));
static USE_PAY_WITH_FT: Lazy<bool> =
    Lazy::new(|| LOCAL_CONF.get("use_pay_with_ft").unwrap_or(false));
static BURN_ADDRESS: Lazy<AccountId> = Lazy::new(|| LOCAL_CONF.get("burn_address").unwrap());
static USE_SHARED_STORAGE: Lazy<bool> =
    Lazy::new(|| LOCAL_CONF.get("use_shared_storage").unwrap_or(false));
static SHARED_STORAGE_POOL: Lazy<SharedStoragePoolManager> = Lazy::new(|| {
    let social_db_id: String = LOCAL_CONF.get("social_db_contract_id").unwrap();
    let signer = InMemorySigner::from_file(Path::new(SHARED_STORAGE_KEYS_FILENAME.as_str()))
        .unwrap_or_else(|err| {
            panic!("failed to get signing keys={SHARED_STORAGE_KEYS_FILENAME:?}: {err:?}")
        });
    SharedStoragePoolManager::new(
        signer,
        &RPC_CLIENT,
        social_db_id.parse().unwrap(),
        SHARED_STORAGE_ACCOUNT_ID.parse().unwrap(),
    )
});
static FLAMETRACE_PERFORMANCE: Lazy<bool> =
    Lazy::new(|| LOCAL_CONF.get("flametrace_performance").unwrap_or(false));

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

#[tokio::main]
async fn main() {
    // initialize tracing (aka logging)
    if *FLAMETRACE_PERFORMANCE {
        setup_global_subscriber();
        info!("default tracing setup with flametrace performance ENABLED");
    } else {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .init();
        info!("default tracing setup with flametrace performance DISABLED");
    }

    // initialize our shared storage pool manager if using fastauth features or using shared storage
    if *USE_FASTAUTH_FEATURES || *USE_SHARED_STORAGE {
        if let Err(err) = SHARED_STORAGE_POOL.check_and_spawn_pool().await {
            let err_msg = format!("Error initializing shared storage pool: {err}");
            error!("{err_msg}");
            tracing::error!(err_msg);
            return;
        } else {
            info!("shared storage pool initialized");
        }
    }

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

    // if fastauth enabled, initialize whitelisted senders with "infinite" allowance in relayer DB
    if *USE_FASTAUTH_FEATURES {
        init_senders_infinite_allowance_fastauth().await;
    }

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
    let addr: SocketAddr = SocketAddr::from((*IP_ADDRESS, *PORT));
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

async fn init_senders_infinite_allowance_fastauth() {
    let max_allowance = u64::MAX;
    for whitelisted_sender in WHITELISTED_DELEGATE_ACTION_RECEIVER_IDS.iter() {
        let redis_result =
            set_account_and_allowance_in_redis(whitelisted_sender, &max_allowance).await;
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
async fn get_allowance(account_id_json: Json<AccountIdJson>) -> impl IntoResponse {
    // convert str account_id val from json to AccountId so I can reuse get_remaining_allowance fn
    let Ok(account_id_val) = AccountId::from_str(&account_id_json.account_id) else {
        return (
            StatusCode::FORBIDDEN,
            format!("Invalid account_id: {}", account_id_json.account_id),
        )
            .into_response();
    };
    match get_remaining_allowance(&account_id_val).await {
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
    match get_oauth_token_in_redis(oauth_token).await {
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
    let redis_result = set_account_and_allowance_in_redis(account_id, allowance_in_gas).await;
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
    let create_account_sda_result = process_signed_delegate_action(sda).await;
    if let Err(err) = create_account_sda_result {
        return (err.status_code, err.message).into_response();
    }
    let Ok(account_id) = account_id.parse::<AccountId>() else {
        let err_msg = format!("Invalid account_id: {account_id}");
        warn!("{err_msg}");
        return (StatusCode::BAD_REQUEST, err_msg).into_response();
    };

    // allocate shared storage for account_id if shared storage is being used
    if *USE_FASTAUTH_FEATURES || *USE_SHARED_STORAGE {
        if let Err(err) = SHARED_STORAGE_POOL
            .allocate_default(account_id.clone())
            .await
        {
            let err_msg = format!("Error allocating storage for account {account_id}: {err:?}");
            error!("{err_msg}");
            return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
        }
    }

    // add oauth token to redis (key: oauth_token, val: true)
    match set_oauth_token_in_redis(oauth_token).await {
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
async fn update_allowance(account_id_allowance: Json<AccountIdAllowanceJson>) -> impl IntoResponse {
    let account_id: &String = &account_id_allowance.account_id;
    let allowance_in_gas: &u64 = &account_id_allowance.allowance;

    let redis_result = set_account_and_allowance_in_redis(account_id, allowance_in_gas).await;

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
async fn update_all_allowances(Json(allowance_json): Json<AllowanceJson>) -> impl IntoResponse {
    let allowance_in_gas = allowance_json.allowance_in_gas;
    let redis_response = update_all_allowances_in_redis(allowance_in_gas).await;
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
    account_id_allowance_oauth: Json<AccountIdAllowanceOauthJson>,
) -> impl IntoResponse {
    let account_id: &String = &account_id_allowance_oauth.account_id;
    let allowance_in_gas: &u64 = &account_id_allowance_oauth.allowance;
    let oauth_token: &String = &account_id_allowance_oauth.oauth_token;
    // check if the oauth_token has already been used and is a key in Relayer DB
    match get_oauth_token_in_redis(oauth_token).await {
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
    let redis_result = set_account_and_allowance_in_redis(account_id, allowance_in_gas).await;

    let Ok(_) = redis_result else {
        let err_msg = format!(
            "Error creating account_id {account_id} with allowance {allowance_in_gas} in Relayer DB:\
            \n{redis_result:?}");
        error!("{err_msg}");
        return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
    };

    let Ok(account_id) = account_id.parse::<AccountId>() else {
        let err_msg = format!("Invalid account_id: {account_id}");
        warn!("{err_msg}");
        return (StatusCode::BAD_REQUEST, err_msg).into_response();
    };
    if let Err(err) = SHARED_STORAGE_POOL
        .allocate_default(account_id.clone())
        .await
    {
        let err_msg = format!("Error allocating storage for account {account_id}: {err:?}");
        error!("{err_msg}");
        return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
    }

    // add oauth token to redis (key: oauth_token, val: true)
    match set_oauth_token_in_redis(oauth_token).await {
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
        (status = 201, description = "Relayed and sent transaction ...", body = String),
        (status = 400, description = "Error deserializing payload data object ...", body = String),
        (status = 500, description = "Error signing transaction: ...", body = String),
    ),
)]
#[instrument]
async fn relay(data: Json<Vec<u8>>) -> impl IntoResponse {
    // deserialize SignedDelegateAction using borsh
    match SignedDelegateAction::try_from_slice(&data.0) {
        Ok(signed_delegate_action) => {
            match process_signed_delegate_action(&signed_delegate_action).await {
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
async fn send_meta_tx(data: Json<SignedDelegateAction>) -> impl IntoResponse {
    let relayer_response = process_signed_delegate_action(
        // deserialize SignedDelegateAction using serde json
        &data.0,
    )
    .await;
    match relayer_response {
        Ok(response) => response.into_response(),
        Err(err) => (err.status_code, err.message).into_response(),
    }
}

#[instrument]
async fn process_signed_delegate_action(
    signed_delegate_action: &SignedDelegateAction,
) -> Result<String, RelayError> {
    debug!(
        "Deserialized SignedDelegateAction object: {:#?}",
        signed_delegate_action
    );

    // create Transaction from SignedDelegateAction
    let signer_account_id: AccountId = RELAYER_ACCOUNT_ID.parse().unwrap();
    // the receiver of the txn is the sender of the signed delegate action
    let receiver_id = &signed_delegate_action.delegate_action.sender_id;
    let da_receiver_id = &signed_delegate_action.delegate_action.receiver_id;

    // check that the delegate action receiver_id is in the whitelisted_contracts
    let is_whitelisted_da_receiver = WHITELISTED_CONTRACTS
        .iter()
        .any(|s| s == da_receiver_id.as_str());
    if !*USE_FASTAUTH_FEATURES && !is_whitelisted_da_receiver {
        let err_msg = format!("Delegate Action receiver_id {da_receiver_id} is not whitelisted",);
        warn!("{err_msg}");
        return Err(RelayError {
            status_code: StatusCode::BAD_REQUEST,
            message: err_msg,
        });
    }
    // check the sender_id in whitelist if applicable
    if *USE_WHITELISTED_DELEGATE_ACTION_RECEIVER_IDS && !*USE_FASTAUTH_FEATURES {
        // check if the delegate action receiver_id (account sender_id) if a whitelisted delegate action receiver
        let is_whitelisted_sender = WHITELISTED_DELEGATE_ACTION_RECEIVER_IDS
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
    if !is_whitelisted_da_receiver && *USE_FASTAUTH_FEATURES {
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
        let is_whitelisted_sender = WHITELISTED_DELEGATE_ACTION_RECEIVER_IDS
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
    if *USE_PAY_WITH_FT {
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
                    let burn_address = BURN_ADDRESS.to_string();
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
            return Err(RelayError {
                status_code: StatusCode::BAD_REQUEST,
                message: err_msg.to_string(),
            });
        }
    }

    if *USE_REDIS {
        // Check the sender's remaining gas allowance in Redis
        let end_user_account: &AccountId = &signed_delegate_action.delegate_action.sender_id;
        let remaining_allowance: u64 = get_remaining_allowance(end_user_account).await.unwrap_or(0);
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

        let actions = vec![Action::Delegate(signed_delegate_action.clone())];
        let execution = RPC_CLIENT
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
            near_primitives::views::FinalExecutionStatus::Failure(_) => "Error sending transaction",
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
        let new_allowance =
            update_remaining_allowance(&signer_account_id, gas_used_in_yn, remaining_allowance)
                .await
                .map_err(|err| {
                    let err_msg =
                        format!("Updating redis remaining allowance errored out: {err:?}");
                    error!("{err_msg}");
                    RelayError {
                        status_code: StatusCode::INTERNAL_SERVER_ERROR,
                        message: err_msg,
                    }
                })?;
        info!("Updated remaining allowance for account {signer_account_id}: {new_allowance}",);

        if let near_primitives::views::FinalExecutionStatus::Failure(_) = status {
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
        let actions = vec![Action::Delegate(signed_delegate_action.clone())];
        let execution = RPC_CLIENT
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
            near_primitives::views::FinalExecutionStatus::Failure(_) => "Error sending transaction",
            _ => "Relayed and sent transaction",
        };
        let status_msg = json!({
            "message": response_msg,
            "status": &execution.status,
            "Transaction Outcome": &execution.transaction_outcome,
            "Receipts Outcome": &execution.receipts_outcome,
        });

        if let near_primitives::views::FinalExecutionStatus::Failure(_) = status {
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

pub async fn get_redis_cnxn() -> Result<PooledConnection<RedisConnectionManager>, RedisError> {
    let conn_result = REDIS_POOL.get();
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

pub async fn update_all_allowances_in_redis(allowance_in_gas: u64) -> Result<String, RelayError> {
    // Get a connection to Redis from the pool
    let mut redis_conn = match get_redis_cnxn().await {
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
    let network = match &(&*NETWORK_ENV)[..] {
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

/**
--------------------------- Testing below here ---------------------------
 */
#[cfg(test)]
mod test {
    use axum::body::{BoxBody, HttpBody};
    use axum::response::Response;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as BASE64_ENGINE, Engine};
    use bytes::BytesMut;
    use near_crypto::{KeyType, PublicKey, Signature};
    use near_primitives::{
        borsh::BorshSerialize,
        delegate_action::{DelegateAction, NonDelegateAction},
        transaction::TransferAction,
        types::{BlockHeight, Nonce},
    };

    use super::*;

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
                    .into_iter()
                    .map(|a| NonDelegateAction::try_from(a).unwrap())
                    .collect(),
                nonce: nonce as Nonce,
                max_block_height: max_block_height as BlockHeight,
                public_key,
            },
            signature,
        }
    }

    #[cfg(test)]
    async fn read_body_to_string(
        mut body: BoxBody,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // helper fn to convert the awful BoxBody dtype into a String so I can view the darn msg
        let mut bytes = BytesMut::new();
        while let Some(chunk) = body.data().await {
            bytes.extend_from_slice(&chunk?);
        }
        Ok(String::from_utf8(bytes.to_vec())?)
    }

    #[tokio::test]
    async fn test_base64_encode_args() {
        // NOTE this is how you encode the "args" in a function call
        // replace with your own "args" to base64 encode
        let ft_transfer_args_json = json!(
            {"receiver_id":"guest-book.testnet","amount":"10"}
        );
        let add_message_args_json = json!(
            {"text":"funny_joke"}
        );
        let ft_transfer_args_b64 = BASE64_ENGINE.encode(ft_transfer_args_json.to_string());
        let add_message_args_b64 = BASE64_ENGINE.encode(add_message_args_json.to_string());
        println!("ft_transfer_args_b64: {ft_transfer_args_b64}");
        println!("add_message_args_b64: {add_message_args_b64}");
    }

    #[tokio::test]
    // NOTE: uncomment ignore locally to run test bc redis doesn't work in github action build env
    #[ignore]
    async fn test_send_meta_tx() {
        // tests assume testnet in config
        // Test Transfer Action
        let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
        let sender_id: String = String::from("relayer_test0.testnet");
        let receiver_id: String = String::from("relayer_test1.testnet");
        let nonce: i32 = 1;
        let max_block_height = 2_000_000_000;

        // simulate calling the '/update_allowance' function with sender_id & allowance
        let allowance_in_gas: u64 = u64::MAX;
        set_account_and_allowance_in_redis(&sender_id, &allowance_in_gas)
            .await
            .expect("Failed to update account and allowance in redis");

        // Call the `/send_meta_tx` function happy path
        let signed_delegate_action = create_signed_delegate_action(
            sender_id.clone(),
            receiver_id.clone(),
            actions.clone(),
            nonce,
            max_block_height,
        );
        let json_payload = Json(signed_delegate_action);
        println!("SignedDelegateAction Json Serialized (no borsh): {json_payload:?}");
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
        let max_block_height = 2_000_000_123;

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
        println!(
            "SignedDelegateAction Json Serialized (no borsh) receiver_id not in whitelist: {non_whitelist_json_payload:?}"
        );
        let err_response = send_meta_tx(non_whitelist_json_payload)
            .await
            .into_response();
        let err_response_status = err_response.status();
        let body: BoxBody = err_response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        println!("Response body: {body_str:?}");
        assert!(
            err_response_status == StatusCode::BAD_REQUEST
                || err_response_status == StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_relay_with_load() {
        // tests assume testnet in config
        // Test Transfer Action

        let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
        let account_id0: String = "nomnomnom.testnet".to_string();
        let account_id1: String = "relayer_test0.testnet".to_string();
        let mut sender_id: String = String::new();
        let mut receiver_id: String = String::new();
        let mut nonce: i32 = 1;
        let max_block_height = 2_000_000_000;

        let num_tests = 100;
        let mut response_statuses = vec![];
        let mut response_bodies = vec![];

        // fire off all post requests in rapid succession and save the response status codes
        for i in 0..num_tests {
            if i % 2 == 0 {
                sender_id.push_str(&account_id0);
                receiver_id.push_str(&account_id1);
            } else {
                sender_id.push_str(&account_id1);
                receiver_id.push_str(&account_id0);
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
            let response = relay(Json(json_payload)).await.into_response();
            response_statuses.push(response.status());
            let body: BoxBody = response.into_body();
            let body_str: String = read_body_to_string(body).await.unwrap();
            response_bodies.push(body_str);

            // increment nonce & reset sender, receiver strs
            nonce += 1;
            sender_id.clear();
            receiver_id.clear();
        }

        // all responses should be successful
        for i in 0..response_statuses.len() {
            let response_status = response_statuses[i];
            println!("{response_status}");
            println!("{}", response_bodies[i]);
            assert_eq!(response_status, StatusCode::OK);
        }
    }
}
