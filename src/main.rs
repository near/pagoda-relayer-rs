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
use base64::engine::general_purpose::STANDARD_NO_PAD as BASE64_ENGINE;
use base64::Engine;
use config::{Config, File as ConfigFile};
use near_crypto::{InMemorySigner, Signer};
use near_fetch::signer::{ExposeAccountId, KeyRotatingSigner};
use near_jsonrpc_client::methods::broadcast_tx_async::RpcBroadcastTxAsyncRequest;

use near_primitives::borsh::{self, BorshDeserialize};
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{Action, FunctionCallAction, Transaction};
use near_primitives::types::{AccountId, Nonce};
use near_primitives::views::{
    ExecutionOutcomeWithIdView, FinalExecutionOutcomeView, FinalExecutionStatus,
};
use near_primitives::{action::delegate::SignedDelegateAction, views::TxExecutionStatus};
use once_cell::sync::Lazy;
use r2d2::{Pool, PooledConnection};
use r2d2_redis::redis::{Commands, ErrorKind::IoError, RedisError};
use r2d2_redis::RedisConnectionManager;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::path::PathBuf;
use std::str::FromStr;
use std::string::ToString;
use std::{net::SocketAddr, sync::Arc};
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

// CONSTANTS
// transaction cost in Gas (10^21yN or 10Tgas or 0.001N)
const TXN_GAS_ALLOWANCE: u64 = 10_000_000_000_000;
const YN_TO_GAS: u128 = 1_000_000_000;
const FT_TRANSFER_METHOD_NAME: &str = "ft_transfer";
const STORAGE_DEPOSIT_METHOD_NAME: &str = "storage_deposit";
const STORAGE_DEPOSIT_AMOUNT_FT: u128 = 1250000000000000000000;
const FT_TRANSFER_ATTACHMENT_DEPOSIT_AMOUNT: u128 = 1;

// CONFIGURATION
#[derive(Debug, Clone, Deserialize)]
struct RelayerConfiguration {
    #[serde(rename = "network", default = "default_network_env")]
    network_env: String,
    #[serde(flatten, default)]
    network_config: NetworkConfig,
    #[serde(default = "default_ip_address")]
    ip_address: [u8; 4],
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_relayer_account_id")]
    relayer_account_id: String,
    #[serde(default = "default_shared_storage_account_id")]
    shared_storage_account_id: String,
    #[serde(default = "default_keys_filename")]
    keys_filename: PathBuf,
    #[serde(default = "default_shared_storage_keys_filename")]
    shared_storage_keys_filename: PathBuf,
    #[serde(default)]
    use_whitelisted_contracts: bool,
    #[serde(default)]
    whitelisted_contracts: Vec<String>, // Assuming AccountId is a type alias for String
    #[serde(default)]
    use_whitelisted_senders: bool,
    #[serde(default)]
    whitelisted_senders: Vec<String>,
    #[serde(default)]
    use_redis: bool,
    #[serde(default = "default_redis_url")]
    redis_url: String,
    #[serde(default)]
    use_fastauth_features: bool,
    #[serde(default)]
    use_pay_with_ft: bool,
    #[serde(default = "default_burn_address")]
    burn_address: String, // Assuming AccountId is a type alias for String
    #[serde(default)]
    use_shared_storage: bool,
    #[serde(default = "default_social_db_contract_id")]
    social_db_contract_id: String, // Assuming AccountId is a type alias for String
    #[serde(default)]
    flametrace_performance: bool,
    #[serde(default)]
    use_exchange: bool,
}
// Default functions for fields requiring complex defaults
fn default_network_env() -> String {
    "testnet".to_string()
}
fn default_ip_address() -> [u8; 4] {
    [127, 0, 0, 1]
}
fn default_port() -> u16 {
    3030
}
fn default_relayer_account_id() -> String {
    "nomnomnom.testnet".to_string()
}
fn default_shared_storage_account_id() -> String {
    "shared_storage.testnet".to_string()
}
fn default_keys_filename() -> PathBuf {
    "./account_keys/nomnomnom.testnet.json".parse().unwrap()
}
fn default_shared_storage_keys_filename() -> PathBuf {
    "./account_keys/shared_storage.testnet.json"
        .parse()
        .unwrap()
}
fn default_redis_url() -> String {
    "redis://127.0.0.1:6379".to_string()
}
fn default_burn_address() -> String {
    "default_relayer.testnet".to_string()
} // Assuming AccountId is a type alias for String
fn default_social_db_contract_id() -> String {
    "social_db.testnet".to_string()
} // Assuming AccountId is a type alias for String

impl Default for RelayerConfiguration {
    fn default() -> Self {
        Self {
            network_env: default_network_env(),
            network_config: NetworkConfig {
                rpc_url: "https://rpc.testnet.near.org".parse().unwrap(),
                rpc_api_key: None,
            },
            ip_address: default_ip_address(),
            port: default_port(),
            relayer_account_id: default_relayer_account_id(),
            shared_storage_account_id: default_shared_storage_account_id(),
            keys_filename: default_keys_filename(),
            shared_storage_keys_filename: default_shared_storage_keys_filename(),
            use_whitelisted_contracts: false,
            whitelisted_contracts: vec![],
            use_whitelisted_senders: false,
            whitelisted_senders: vec![],
            use_redis: false,
            redis_url: default_redis_url(),
            use_fastauth_features: false,
            use_pay_with_ft: false,
            use_shared_storage: false,
            burn_address: default_burn_address(),
            social_db_contract_id: default_social_db_contract_id(),
            flametrace_performance: false,
            use_exchange: false,
        }
    }
}
impl RelayerConfiguration {
    fn check_and_announce_defaults(&self) {
        let default = RelayerConfiguration::default();

        if self.network_env == default.network_env {
            println!("Using DEFAULT network environment: {}", default.network_env);
        }

        if (self.network_config.rpc_url == default.network_config.rpc_url)
            && (self.network_config.rpc_api_key == default.network_config.rpc_api_key)
        {
            println!(
                "Using DEFAULT network configuration with rpc_url: {}",
                self.network_config.rpc_url
            );
        }

        if self.ip_address == default.ip_address {
            println!("Using DEFAULT IP address: {:?}", default.ip_address);
        }

        if self.port == default.port {
            println!("Using DEFAULT port: {}", default.port);
        }

        if self.relayer_account_id == default.relayer_account_id {
            println!(
                "Using DEFAULT relayer account ID: {}",
                default.relayer_account_id
            );
        }

        if self.shared_storage_account_id == default.shared_storage_account_id {
            println!(
                "Using DEFAULT shared storage account ID: {}",
                default.shared_storage_account_id
            );
        }

        if self.keys_filename == default.keys_filename {
            println!("Using DEFAULT keys filename: {:?}", default.keys_filename);
        }

        if self.shared_storage_keys_filename == default.shared_storage_keys_filename {
            println!(
                "Using DEFAULT shared storage keys filename: {:?}",
                default.shared_storage_keys_filename
            );
        }

        if self.use_whitelisted_contracts == default.use_whitelisted_contracts {
            println!(
                "Using DEFAULT for use_whitelisted_contracts: {}",
                default.use_whitelisted_contracts
            );
        }

        if self.whitelisted_contracts.is_empty() {
            println!("Using DEFAULT (empty) whitelisted_contracts list");
        }

        if self.use_whitelisted_senders == default.use_whitelisted_senders {
            println!(
                "Using DEFAULT for use_whitelisted_senders: {}",
                default.use_whitelisted_senders
            );
        }

        if self.whitelisted_senders.is_empty() {
            println!("Using DEFAULT (empty) whitelisted_senders list");
        }

        if self.use_redis == default.use_redis {
            println!("Using DEFAULT for use_redis: {}", default.use_redis);
        }

        if self.redis_url == default.redis_url {
            println!("Using DEFAULT Redis URL: {}", default.redis_url);
        }

        if self.use_fastauth_features == default.use_fastauth_features {
            println!(
                "Using DEFAULT for use_fastauth_features: {}",
                default.use_fastauth_features
            );
        }

        if self.use_pay_with_ft == default.use_pay_with_ft {
            println!(
                "Using DEFAULT for use_pay_with_ft: {}",
                default.use_pay_with_ft
            );
        }

        if self.use_shared_storage == default.use_shared_storage {
            println!(
                "Using DEFAULT for use_shared_storage: {}",
                default.use_shared_storage
            );
        }

        if self.burn_address == default.burn_address {
            println!("Using DEFAULT burn address: {}", default.burn_address);
        }

        if self.social_db_contract_id == default.social_db_contract_id {
            println!(
                "Using DEFAULT social db contract ID: {}",
                default.social_db_contract_id
            );
        }

        if self.flametrace_performance == default.flametrace_performance {
            println!(
                "Using DEFAULT for flametrace_performance: {}",
                default.flametrace_performance
            );
        }

        if self.use_exchange == default.use_exchange {
            println!("Using DEFAULT for use_exchange: {}", default.use_exchange);
        }
    }
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
        config.social_db_contract_id.parse().unwrap(),
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
#[derive(Deserialize)]
struct SignerConfig {
    account_id: String,
    public_key: String,
    secret_key: String,
}

// load config from toml and setup jsonrpc client
static LOCAL_CONF: Lazy<Config> = Lazy::new(|| {
    Config::builder()
        .add_source(ConfigFile::with_name("config.toml"))
        .build()
        .unwrap()
});
static ROTATING_SIGNER: Lazy<KeyRotatingSigner> = Lazy::new(|| {
    let path = LOCAL_CONF
        .get::<String>("keys_filename")
        .expect("Failed to read 'keys_filename' from config");
    let keys_file = std::fs::File::open(path).expect("Failed to open keys file");
    let signers: Vec<InMemorySigner> =
        serde_json::from_reader(keys_file).expect("Failed to parse keys file");

    KeyRotatingSigner::from_signers(signers)
});
static MAP_SIGNER: Lazy<HashMap<String, InMemorySigner>> = Lazy::new(|| {
    let path = LOCAL_CONF
        .get::<String>("keys_filename")
        .expect("Failed to read 'keys_filename' from config");
    let keys_file = std::fs::File::open(path).expect("Failed to open keys file");
    let signers_configs: Vec<SignerConfig> =
        serde_json::from_reader(keys_file).expect("Failed to parse keys file");

    let mut signers: HashMap<String, InMemorySigner> = HashMap::new();

    for signer_config in signers_configs {
        let signer = InMemorySigner::from_secret_key(
            signer_config.account_id.parse().unwrap(),
            signer_config.secret_key.parse().unwrap(),
        );

        // Use the public key string as the key for the hashmap
        signers.insert(signer_config.public_key.clone(), signer);
    }
    signers
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

#[derive(Clone, Debug, Deserialize, ToSchema)]
struct PublicKeyAndSDAJson {
    #[schema(example = "ed25519:89GtfFzez3opomVpwa7i4m3nptHtc7Ha514XHMWszQtL")]
    public_key: String,
    signed_delegate_action: SignedDelegateAction,
    nonce: Option<u64>,
    block_hash: Option<String>,
}
impl Display for PublicKeyAndSDAJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "public_key: {}\nsigned_delegate_action signature: {}\nnonce: {}\nblock_hash: {}",
            self.public_key,
            self.signed_delegate_action.signature,
            self.nonce.unwrap(),
            self.block_hash.clone().unwrap()
        )
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
    let toml_config = Config::builder()
        .add_source(ConfigFile::with_name("config.toml"))
        .build()
        .unwrap();

    let config: RelayerConfiguration = toml_config.try_deserialize().unwrap();

    // After deserialization, check for defaults and notify
    config.check_and_announce_defaults();

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

    if config.use_redis {
        redis_pool = Some(create_redis_pool(&config));
    }

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
        init_senders_infinite_allowance_fastauth(&config, redis_pool.as_ref().unwrap()).await;
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
            send_meta_tx_async,
            send_meta_tx_nopoll,
            sign_meta_tx,
            sign_meta_tx_no_filter,
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
                PublicKeyAndSDAJson,
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
        .route("/sign_meta_tx", post(sign_meta_tx))
        .route("/sign_meta_tx_no_filter", post(sign_meta_tx_no_filter))
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
        (status = 403, description = "Invalid account_id", body = String),
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
    match get_remaining_allowance(state.redis_pool.as_ref().unwrap(), &account_id_val).await {
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
    match get_oauth_token_in_redis(state.redis_pool.as_ref().unwrap(), oauth_token).await {
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
        state.redis_pool.as_ref().unwrap(),
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
        process_signed_delegate_action(state.as_ref(), sda, Some(TxExecutionStatus::IncludedFinal))
            .await
    } else {
        process_signed_delegate_action(state.as_ref(), sda, None).await
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
    match set_oauth_token_in_redis(state.redis_pool.as_ref().unwrap(), oauth_token).await {
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
        state.redis_pool.as_ref().unwrap(),
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
        state.redis_pool.as_ref().unwrap(),
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
    match get_oauth_token_in_redis(state.redis_pool.as_ref().unwrap(), oauth_token).await {
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
        state.redis_pool.as_ref().unwrap(),
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
    match set_oauth_token_in_redis(state.redis_pool.as_ref().unwrap(), oauth_token).await {
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
            match process_signed_delegate_action(state.as_ref(), &signed_delegate_action, None)
                .await
            {
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
        (status = 200, description = "Relayed and sent transaction ...", body = String),
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
        &data.0, None,
    )
    .await;
    match relayer_response {
        Ok(response) => response.into_response(),
        Err(err) => (err.status_code, err.message).into_response(),
    }
}

#[utoipa::path(
post,
    path = "/sign_meta_tx",
    request_body = PublicKeyAndSDAJson,
    responses(
        (status = 200, description = "Relayed and sent transaction ...", body = String),
        (status = 400, description = "Error deserializing payload data object ...", body = String),
        (status = 500, description = "Error signing transaction: ...", body = String),
    ),
)]
#[instrument]
async fn sign_meta_tx(
    State(state): State<Arc<AppState>>,
    data: Json<PublicKeyAndSDAJson>,
) -> impl IntoResponse {
    let sda = &data.0.signed_delegate_action;
    let sda_validation_result = validate_signed_delegate_action(&state, sda);
    if sda_validation_result.is_err() {
        error!("Error validating signed delegate action: {sda_validation_result:?}");
        return (
            StatusCode::BAD_REQUEST,
            sda_validation_result.unwrap_err().message,
        )
            .into_response();
    }
    let relayer_response = create_signed_meta_tx(
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
    path = "/sign_meta_tx_no_filter",
    request_body = PublicKeyAndSDAJson,
    responses(
        (status = 200, description = "Relayed and sent transaction ...", body = String),
        (status = 400, description = "Error deserializing payload data object ...", body = String),
        (status = 500, description = "Error signing transaction: ...", body = String),
    ),
)]
#[instrument]
async fn sign_meta_tx_no_filter(
    State(state): State<Arc<AppState>>,
    data: Json<PublicKeyAndSDAJson>,
) -> impl IntoResponse {
    let relayer_response = create_signed_meta_tx(
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

async fn fetch_nonce_and_block_hash(
    state: &AppState,
    signer: &InMemorySigner,
) -> Result<(Nonce, CryptoHash), RelayError> {
    let result = state
        .rpc_client
        .fetch_nonce(signer.account_id(), &signer.public_key())
        .await;

    match result {
        Ok((nonce, block_hash, _)) => Ok((nonce, block_hash)),
        Err(e) => Err(RelayError {
            status_code: StatusCode::INTERNAL_SERVER_ERROR, // Adjust the status code as appropriate
            message: format!("Error fetching nonce: {:?}", e),
        }),
    }
}

#[instrument]
async fn create_signed_meta_tx(
    state: &AppState,
    pk_and_sda: &PublicKeyAndSDAJson,
) -> Result<String, RelayError> {
    let public_key: String = pk_and_sda.public_key.clone();
    let temp_signer = MAP_SIGNER.get(&*public_key).ok_or_else(|| RelayError {
        status_code: StatusCode::BAD_REQUEST,
        message: format!(
            "Signer {:?} not found. Please add it to the json file in account_keys",
            public_key.clone()
        ),
    })?;
    let signer: &InMemorySigner = temp_signer;
    let receiver_id: &AccountId = &pk_and_sda.signed_delegate_action.delegate_action.sender_id;
    let actions: Vec<Action> = vec![Action::Delegate(Box::new(
        pk_and_sda.signed_delegate_action.clone(),
    ))];
    let nonce: u64;
    let block_hash: CryptoHash;

    if let (Some(nonce_param), Some(block_hash_param)) = (pk_and_sda.nonce, &pk_and_sda.block_hash)
    {
        nonce = nonce_param;
        block_hash = CryptoHash::from_str(&block_hash_param.clone()).map_err(|_e| RelayError {
            status_code: StatusCode::BAD_REQUEST,
            message: format!("block hash {:?} is invalid", block_hash_param.clone()),
        })?;
    } else {
        (nonce, block_hash) = fetch_nonce_and_block_hash(state, signer).await?;
    }

    let signed_meta_tx = Transaction {
        nonce,
        block_hash,
        signer_id: signer.account_id().clone(),
        public_key: public_key.clone().parse().unwrap(),
        receiver_id: receiver_id.clone(),
        actions: actions.clone(),
    }
    .sign(signer);

    let signed_meta_tx_b64: String = BASE64_ENGINE.encode(borsh::to_vec(&signed_meta_tx).unwrap());
    Ok(json!({"signed_transaction": signed_meta_tx_b64}).to_string())
}

// Single unified function to send a transaction
#[instrument]
async fn process_signed_delegate_action(
    state: &AppState,
    signed_delegate_action: &SignedDelegateAction,
    wait_until: Option<TxExecutionStatus>,
) -> Result<String, RelayError> {
    // Store as a direct value which will be cloned inside the closure.
    let wait_until_param = wait_until.unwrap_or(TxExecutionStatus::ExecutedOptimistic);

    filter_and_send_signed_delegate_action(
        state,
        signed_delegate_action.clone(),
        move |receiver_id, actions| {
            // 'move' is used here to take ownership of wait_until_param in the closure
            let wait_until_clone = Some(wait_until_param.clone()); // Clone here for each invocation
            async move {
                match state
                    .rpc_client
                    .send_tx(&*ROTATING_SIGNER, &receiver_id, actions, wait_until_clone)
                    .await
                {
                    Err(err) => {
                        let err_msg = format!("Error signing transaction: {:?}", err);
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
            }
        },
    )
    .await
}

// Notice we got rid of the old `big_timeout` function

// Uses the old deprecated RPC call to send a transaction
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
                .fetch_nonce(ROTATING_SIGNER.account_id(), ROTATING_SIGNER.public_key())
                .await
                .map_err(|e| format!("Error fetching nonce: {:?}", e))?;
            let txn_hash = state
                .rpc_client_nofetch
                .call(&RpcBroadcastTxAsyncRequest {
                    signed_transaction: Transaction {
                        nonce,
                        block_hash,
                        signer_id: ROTATING_SIGNER.account_id().clone(),
                        public_key: ROTATING_SIGNER.public_key().clone(),
                        receiver_id: receiver_id.clone(),
                        actions: actions.clone(),
                    }
                    .sign(ROTATING_SIGNER.current_signer()),
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
    // TODO: Is this true? What about validating method name? Or Pay With FT?
    if !state.config.use_whitelisted_contracts && !state.config.use_whitelisted_senders {
        return Ok(());
    }

    // check that the delegate action receiver_id is in the whitelisted_contracts
    let is_whitelisted_da_receiver = state
        .config
        .whitelisted_contracts
        .iter()
        .any(|s| s.as_str() == da_receiver_id.as_str());
    if state.config.use_exchange {
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
                Action::FunctionCall(boxed_function_call_action) => {
                    let FunctionCallAction {
                        method_name,
                        deposit,
                        ..
                    } = *boxed_function_call_action;

                    debug!("method_name: {:?}", method_name);
                    debug!("deposit: {:?}", deposit);
                    if state.config.use_whitelisted_contracts && !is_whitelisted_da_receiver {
                        return Err(RelayError {
                            status_code: StatusCode::BAD_REQUEST,
                            message: format!(
                                "Delegate Action Receiver_id {da_receiver_id:?} is not whitelisted"
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
            .first()
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
                if let Action::FunctionCall(ref function_call_action) = action {
                    let FunctionCallAction { ref args, .. } = **function_call_action;
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
    validation_result?;

    // the receiver of the txn is the sender of the signed delegate action
    let receiver_id: &AccountId = &signed_delegate_action.delegate_action.sender_id;
    let actions: Vec<Action> = vec![Action::Delegate(Box::new(signed_delegate_action.clone()))];
    let txn_hash: CryptoHash = state
        .rpc_client
        .send_tx_async(
            &*ROTATING_SIGNER,
            receiver_id,
            actions,
            Some(TxExecutionStatus::Included),
        )
        .await
        .map_err(|err| {
            let err_msg = format!("Error signing transaction: {err:?}");
            error!("{err_msg}");
            RelayError {
                status_code: StatusCode::INTERNAL_SERVER_ERROR,
                message: err_msg,
            }
        })?;

    // TODO we have no idea how much gas is being burnt in this txn - shouldn't use async with redis

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
    validation_result?;

    // the receiver of the txn is the sender of the signed delegate action
    let receiver_id: &AccountId = &signed_delegate_action.delegate_action.sender_id;
    let actions: Vec<Action> = vec![Action::Delegate(Box::new(signed_delegate_action.clone()))];

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
            .send_tx(
                &*ROTATING_SIGNER,
                receiver_id,
                actions,
                Some(TxExecutionStatus::ExecutedOptimistic),
            )
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
            receiver_id,
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
        info!("Updated remaining allowance for account {receiver_id}: {new_allowance}",);

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
            .send_tx(
                &*ROTATING_SIGNER,
                receiver_id,
                actions,
                Some(TxExecutionStatus::ExecutedOptimistic),
            )
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

/* -------------------------------------- UNIT TESTS --------------------------------------
===========================================================================================
*/

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        create_redis_pool, get_redis_cnxn, update_all_allowances_in_redis,
        validate_signed_delegate_action, AppState, RelayerConfiguration,
    };

    use axum::body::{BoxBody, HttpBody};
    use axum::response::Response;
    use axum::{
        extract::{Json, State},
        http::StatusCode,
    };
    use bytes::BytesMut;

    use near_crypto::KeyType::ED25519;
    use near_crypto::{InMemorySigner, PublicKey, Signature};

    use near_primitives::account::{AccessKey, AccessKeyPermission};
    use near_primitives::action::delegate::{
        DelegateAction, NonDelegateAction, SignedDelegateAction,
    };
    use near_primitives::signable_message::{SignableMessage, SignableMessageType};
    use near_primitives::transaction::{Action, AddKeyAction, FunctionCallAction, TransferAction};
    use near_primitives::types::Balance;
    use near_primitives::types::{BlockHeight, Nonce};

    use r2d2::Pool;
    use r2d2_redis::RedisConnectionManager;
    use serde_json::json;

    use std::sync::Arc;

    /// Helper function to create a test Redis pool
    async fn create_test_redis_pool() -> Pool<RedisConnectionManager> {
        let manager = RedisConnectionManager::new("redis://127.0.0.1:6379").unwrap();
        Pool::builder().build(manager).unwrap()
    }

    /// Helper function to create a test AppState
    async fn create_app_state(
        use_whitelisted_contracts: bool,
        use_whitelisted_senders: bool,
        whitelisted_contracts: Option<Vec<String>>,
        whitelisted_senders: Option<Vec<String>>,
        _use_exchange: bool,
    ) -> AppState {
        let config = RelayerConfiguration {
            use_whitelisted_contracts,
            use_whitelisted_senders,
            whitelisted_contracts: whitelisted_contracts.unwrap_or_default(),
            whitelisted_senders: whitelisted_senders.unwrap_or_default(),
            use_exchange: _use_exchange,
            ..Default::default()
        };
        let rpc_client = Arc::new(config.network_config.rpc_client());
        let rpc_client_nofetch = Arc::new(config.network_config.raw_rpc_client());
        let redis_pool = Some(create_test_redis_pool().await);

        AppState {
            config,
            rpc_client,
            rpc_client_nofetch,
            shared_storage_pool: None,
            redis_pool,
        }
    }

    /// Helper function to create a test Arc State.
    /// Complementary to create_app_state fn
    fn convert_app_state_to_arc_app_state(app_state: AppState) -> State<Arc<AppState>> {
        let shared_state = Arc::new(app_state);
        State(shared_state.clone())
    }

    /// Helper function to create a test SignedDelegateAction
    fn create_signed_delegate_action(
        sender_id: Option<&str>,
        receiver_id: Option<&str>,
        actions: Option<Vec<Action>>,
        nonce: Option<u64>,
    ) -> SignedDelegateAction {
        // dw, it's just a testnet account
        let seed: String =
            "nuclear egg couch off antique brave cake wrap orchard snake prosper one".to_string();
        let mut sender_account_id: AccountId = "relayer_test0.testnet".parse().unwrap();
        let public_key = PublicKey::from_seed(ED25519, &seed.clone());
        let signer = InMemorySigner::from_seed(sender_account_id.clone(), ED25519, &seed.clone());

        let mut receiver_account_id: AccountId = "relayer_test1.testnet".parse().unwrap();

        let mut actions_vec = vec![Action::Transfer(TransferAction {
            deposit: 0.00000001 as Balance,
        })];

        if sender_id.is_some() {
            sender_account_id = sender_id.unwrap().parse().unwrap();
        }
        if receiver_id.is_some() {
            receiver_account_id = receiver_id.unwrap().parse().unwrap();
        }
        if actions.is_some() {
            actions_vec = actions.unwrap();
        }

        let delegate_action = DelegateAction {
            sender_id: sender_account_id.clone(),
            receiver_id: receiver_account_id,
            actions: actions_vec
                .into_iter()
                .map(|a| NonDelegateAction::try_from(a).unwrap())
                .collect(),
            nonce: nonce.unwrap_or(0),
            max_block_height: 2000000000 as BlockHeight,
            public_key,
        };

        let signable = SignableMessage::new(&delegate_action, SignableMessageType::DelegateAction);
        SignedDelegateAction {
            signature: signable.sign(&signer),
            delegate_action,
        }
    }

    #[tokio::test]
    /// Tests that validate_signed_delegate_action returns Ok when no whitelisting is used
    async fn test_validate_signed_delegate_action_no_whitelisting() {
        let app_state = create_app_state(false, false, None, None, false).await;
        let signed_delegate_action = create_signed_delegate_action(None, None, None, None);

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(result.is_ok());
    }

    #[tokio::test]
    /// Tests that validate_signed_delegate_action returns an error when the receiver is not whitelisted
    async fn test_validate_signed_delegate_action_receiver_not_whitelisted() {
        let app_state = create_app_state(true, false, None, None, false).await;
        let signed_delegate_action = create_signed_delegate_action(None, None, None, None);

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(result.is_err());
    }

    #[tokio::test]
    /// Tests that validate_signed_delegate_action returns an error when the sender is not whitelisted
    async fn test_validate_signed_delegate_action_sender_not_whitelisted() {
        let mut app_state = create_app_state(false, true, None, None, false).await;
        app_state.config.use_whitelisted_senders = true;
        let signed_delegate_action = create_signed_delegate_action(None, None, None, None);

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_validate_signed_delegate_action_only_receiver_whitelisted() {
        let app_state = create_app_state(
            true,  // Receiver (contract) whitelisting enabled
            false, // Sender whitelisting disabled
            Some(vec!["whitelisted_receiver.testnet".to_string()]), // Whitelisted receivers (contracts)
            None,                                                   // Whitelisted senders not used
            false,                                                  // use_exchange disabled
        )
        .await;
        let signed_delegate_action = create_signed_delegate_action(
            Some("sender.testnet"),               // sender_id
            Some("whitelisted_receiver.testnet"), // receiver_id matches the whitelisted receiver (contract)
            None,                                 // Default actions
            None,                                 // Default nonce
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(
            result.is_ok(),
            "Expected OK validation for actions with whitelisted receiver."
        );
    }

    #[tokio::test]
    async fn test_validate_signed_delegate_action_only_sender_whitelisted() {
        let app_state = create_app_state(
            false, // Receiver (contract) whitelisting disabled
            true,  // Sender whitelisting enabled
            None,  // Whitelisted receivers (contracts) not used
            Some(vec!["whitelisted_sender.testnet".to_string()]), // Whitelisted senders
            false, // use_exchange disabled
        )
        .await;
        let signed_delegate_action = create_signed_delegate_action(
            Some("whitelisted_sender.testnet"), // sender_id matches the whitelisted sender
            Some("receiver.testnet"),           // receiver_id
            None,                               // Default actions
            None,                               // Default nonce
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(
            result.is_ok(),
            "Expected OK validation for actions with whitelisted sender."
        );
    }

    #[tokio::test]
    async fn test_validate_signed_delegate_action_both_whitelisted() {
        let app_state = create_app_state(
            true, // Receiver (contract) whitelisting enabled
            true, // Sender whitelisting enabled
            Some(vec!["whitelisted_receiver.testnet".to_string()]), // Whitelisted receivers (contracts)
            Some(vec!["whitelisted_sender.testnet".to_string()]),   // Whitelisted senders
            false,                                                  // Pay with FT disabled
        )
        .await;
        let signed_delegate_action = create_signed_delegate_action(
            Some("whitelisted_sender.testnet"), // sender_id matches the whitelisted sender
            Some("whitelisted_receiver.testnet"), // receiver_id matches the whitelisted receiver (contract)
            None,                                 // Default actions
            None,                                 // Default nonce
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(
            result.is_ok(),
            "Expected OK validation for actions with both sender and receiver whitelisted."
        );
    }

    #[tokio::test]
    async fn test_validate_action_receiver_not_in_whitelist() {
        let app_state = create_app_state(
            true,  // Receiver (contract) whitelisting enabled
            false, // Sender whitelisting disabled
            Some(vec!["whitelisted_receiver.testnet".to_string()]), // Whitelisted receivers (contracts)
            None,                                                   // Whitelisted senders not used
            false,                                                  // Pay with FT disabled
        )
        .await;
        let signed_delegate_action = create_signed_delegate_action(
            Some("sender.testnet"),                   // sender_id
            Some("non_whitelisted_receiver.testnet"), // receiver_id is not in the whitelisted receivers
            None,                                     // Default actions
            None,                                     // Default nonce
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(
            result.is_err(),
            "Expected validation failure due to receiver not being in whitelist."
        );
    }

    #[tokio::test]
    async fn test_validate_action_sender_not_in_whitelist() {
        let app_state = create_app_state(
            false, // Receiver (contract) whitelisting disabled
            true,  // Sender whitelisting enabled
            None,  // Whitelisted receivers not used
            Some(vec!["whitelisted_sender.testnet".to_string()]), // Whitelisted senders
            false, // use_exchange disabled
        )
        .await;
        let signed_delegate_action = create_signed_delegate_action(
            Some("non_whitelisted_sender.testnet"), // sender_id is not in the whitelisted senders
            Some("receiver.testnet"),               // receiver_id
            None,                                   // Default actions
            None,                                   // Default nonce
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(
            result.is_err(),
            "Expected validation failure due to sender not being in whitelist."
        );
    }

    #[tokio::test]
    async fn test_validate_action_both_sender_receiver_not_in_whitelist() {
        let app_state = create_app_state(
            true, // Receiver (contract) whitelisting enabled
            true, // Sender whitelisting enabled
            Some(vec!["whitelisted_receiver.testnet".to_string()]), // Whitelisted receivers
            Some(vec!["whitelisted_sender.testnet".to_string()]), // Whitelisted senders
            false, // Pay with FT disabled
        )
        .await;
        let signed_delegate_action = create_signed_delegate_action(
            Some("non_whitelisted_sender.testnet"), // sender_id is not in the whitelisted senders
            Some("non_whitelisted_receiver.testnet"), // receiver_id is not in the whitelisted receivers
            None,                                     // Default actions
            None,                                     // Default nonce
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(result.is_err(), "Expected validation failure due to both sender and receiver not being in their respective whitelists.");
    }

    #[tokio::test]
    async fn test_valid_function_call_with_whitelisted_sender() {
        let app_state = create_app_state(
            false,                                    // use_whitelisted_contracts
            true,                                     // use_whitelisted_senders
            None,                                     // whitelisted_contracts
            Some(vec!["sender.testnet".to_string()]), // whitelisted_senders
            true,                                     // use_exchange
        )
        .await;

        // Simulate a valid `ft_transfer` function call action
        let actions = vec![Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: FT_TRANSFER_METHOD_NAME.to_string(),
            args: BASE64_ENGINE
                .encode("{\"receiver_id\":\"receiver.testnet\",\"amount\":\"10\"}")
                .into_bytes(),
            gas: 30_000_000_000_000,
            deposit: FT_TRANSFER_ATTACHMENT_DEPOSIT_AMOUNT,
        }))];

        let signed_delegate_action = create_signed_delegate_action(
            Some("sender.testnet"),   // Matching the whitelisted sender
            Some("exchange.testnet"), // Receiver (not relevant in this case)
            Some(actions),
            None,
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);
        assert!(
            result.is_ok(),
            "Expected validation to pass for a valid function call with a whitelisted sender."
        );
    }

    #[tokio::test]
    async fn test_invalid_method_name_with_whitelisted_sender() {
        let app_state = create_app_state(
            false,
            true,
            None,
            Some(vec!["sender.testnet".to_string()]),
            true,
        )
        .await;

        // Using an invalid method name
        let actions = vec![Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "invalid_method".to_string(),
            args: BASE64_ENGINE.encode("{}").into_bytes(),
            gas: 30_000_000_000_000,
            deposit: 1,
        }))];

        let signed_delegate_action = create_signed_delegate_action(
            Some("sender.testnet"),
            Some("exchange.testnet"),
            Some(actions),
            None,
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);
        assert!(result.is_err(), "Expected validation to fail for an invalid method name, even with a whitelisted sender.");
    }

    #[tokio::test]
    async fn test_valid_method_name_incorrect_deposit() {
        let app_state = create_app_state(
            false,
            true,
            None,
            Some(vec!["sender.testnet".to_string()]),
            true,
        )
        .await;

        // Valid method name but incorrect deposit amount
        let actions = vec![Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: FT_TRANSFER_METHOD_NAME.to_string(),
            args: BASE64_ENGINE
                .encode("{\"receiver_id\":\"receiver.testnet\",\"amount\":\"10\"}")
                .into_bytes(),
            gas: 30_000_000_000_000,
            deposit: 0, // Incorrect deposit amount
        }))];

        let signed_delegate_action = create_signed_delegate_action(
            Some("sender.testnet"),
            Some("exchange.testnet"),
            Some(actions),
            None,
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);
        assert!(
            result.is_err(),
            "Expected validation to fail for a correct method name with incorrect deposit amount."
        );
    }

    #[tokio::test]
    async fn test_non_whitelisted_sender_valid_method_and_deposit() {
        let app_state = create_app_state(
            false,
            true,
            None,
            Some(vec!["whitelisted_sender.testnet".to_string()]),
            true,
        )
        .await;

        // Non-whitelisted sender but valid method and deposit
        let actions = vec![Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: FT_TRANSFER_METHOD_NAME.to_string(),
            args: BASE64_ENGINE
                .encode("{\"receiver_id\":\"receiver.testnet\",\"amount\":\"10\"}")
                .into_bytes(),
            gas: 30_000_000_000_000,
            deposit: FT_TRANSFER_ATTACHMENT_DEPOSIT_AMOUNT,
        }))];

        let signed_delegate_action = create_signed_delegate_action(
            Some("non_whitelisted_sender.testnet"), // Non-whitelisted sender
            Some("exchange.testnet"),
            Some(actions),
            None,
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);
        assert!(result.is_err(), "Expected validation to fail for a non-whitelisted sender, despite valid method and deposit.");
    }

    #[tokio::test]
    /// Tests that validate_signed_delegate_action fails when the invalid method name is provided
    /// use_exchange, use_whitelisted_senders are both true
    async fn test_validation_fails_for_invalid_method_name_use_exchange() {
        let state = create_app_state(false, true, None, None, true).await;
        let action = create_signed_delegate_action(
            None,
            None,
            Some(vec![Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: "invalid_method".to_string(),
                args: vec![],
                gas: 30000000000000,
                deposit: 1,
            }))]),
            None,
        );
        assert!(validate_signed_delegate_action(&state, &action).is_err());
    }

    #[tokio::test]
    /// Tests that validate_signed_delegate_action fails when the invalid action type is provided
    /// use_exchange, use_whitelisted_senders are both true
    async fn test_validation_fails_for_disallowed_action_type_exchange() {
        let state = create_app_state(false, true, None, None, true).await;
        let action = create_signed_delegate_action(
            None, None, None, None, // default action type is transfer, which is not allowed
        );
        assert!(validate_signed_delegate_action(&state, &action).is_err());
    }

    #[tokio::test]
    async fn test_validate_signed_delegate_action_ft_transfer_to_burn_address() {
        let mut app_state = create_app_state(
            false,                                                // use_whitelisted_contracts
            true,                                                 // use_whitelisted_senders
            None,                                                 // whitelisted_contracts
            Some(vec!["whitelisted_sender.testnet".to_string()]), // whitelisted_senders
            false,                                                // use_exchange
        )
        .await;
        app_state.config.use_pay_with_ft = true;
        app_state.config.burn_address = "burn_address.testnet".to_string();

        // Create a SignedDelegateAction simulating an FT transfer to the burn address
        let signed_delegate_action = create_signed_delegate_action(
            Some("whitelisted_sender.testnet"),   // Mock sender
            Some(&app_state.config.burn_address), // Use the configured burn address
            Some(vec![Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: "ft_transfer".to_string(),
                args: json!({
                    "receiver_id": app_state.config.burn_address,
                    "amount": "1000000000000000000" // Example amount
                })
                .to_string()
                .into_bytes(),
                gas: 300000000000000,
                deposit: 1, // Simulated deposit for the action
            }))]),
            None,
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);
        println!("{result:?}");
        assert!(
            result.is_ok(),
            "Expected FT transfer to burn address to be valid."
        );
    }

    #[tokio::test]
    async fn test_validate_signed_delegate_action_ft_transfer_to_non_burn_address() {
        let mut app_state = create_app_state(
            false,                                                // use_whitelisted_contracts
            true,                                                 // use_whitelisted_senders
            None,                                                 // whitelisted_contracts
            Some(vec!["whitelisted_sender.testnet".to_string()]), // whitelisted_senders
            false,                                                // use_exchange
        )
        .await;
        app_state.config.use_pay_with_ft = true;

        // Create a SignedDelegateAction simulating an FT transfer to a non-burn address
        let signed_delegate_action = create_signed_delegate_action(
            Some("whitelisted_sender.testnet"), // Mock sender
            Some("non_burn_address.testnet"),   // Use a non-burn address
            Some(vec![Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: "ft_transfer".to_string(),
                args: json!({
                    "receiver_id": "non_burn_address.testnet",
                    "amount": "1000000000000000000" // Example amount
                })
                .to_string()
                .into_bytes(),
                gas: 300000000000000,
                deposit: 1, // Simulated deposit for the action
            }))]),
            None,
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);
        assert!(
            result.is_err(),
            "Expected FT transfer to non-burn address to be invalid."
        );
    }

    #[tokio::test]
    async fn test_validate_signed_delegate_action_fastauth_with_contract_whitelisting_valid_scenario(
    ) {
        let mut app_state = create_app_state(
            true,                                                   // use_whitelisted_contracts enabled
            false, // use_whitelisted_senders disabled for this test
            Some(vec!["whitelisted_contract.testnet".to_string()]), // Assuming this is irrelevant for this test due to fastauth specifics
            None,  // No whitelisted_senders specified
            false, // use_exchange disabled
        )
        .await;
        app_state.config.use_fastauth_features = true;

        // Simulate a valid SignedDelegateAction for AddKey by the same account to itself
        let sender_id = "user_with_fastauth.testnet";
        let receiver_id = sender_id; // Matching sender and receiver for fastauth scenario
        let actions = vec![Action::AddKey(Box::new(AddKeyAction {
            public_key: "ed25519:3GTVh8BQjY3t9ZUpzwCSMbFqWVTswei8uMBQBBnS5H6p"
                .parse()
                .unwrap(),
            access_key: AccessKey {
                nonce: 0,
                permission: AccessKeyPermission::FullAccess,
            },
        }))];

        let signed_delegate_action =
            create_signed_delegate_action(Some(sender_id), Some(receiver_id), Some(actions), None);

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(
            result.is_ok(),
            "Expected OK validation for valid fastauth scenario with contract whitelisting."
        );
    }

    #[tokio::test]
    async fn test_validate_signed_delegate_action_fastauth_with_contract_whitelisting_invalid_scenario(
    ) {
        let mut app_state = create_app_state(
            true,                                                   // use_whitelisted_contracts enabled
            false, // use_whitelisted_senders disabled for this test
            Some(vec!["whitelisted_contract.testnet".to_string()]), // Assuming this is irrelevant for this test due to fastauth specifics
            None,  // No whitelisted_senders specified
            false, // use_exchange disabled
        )
        .await;
        app_state.config.use_fastauth_features = true;

        // Simulate an invalid SignedDelegateAction where sender and receiver IDs do not match
        // and the action is not a self-action (e.g., transferring tokens), making it invalid under fastauth
        let sender_id = "user_without_fastauth.testnet"; // Not matching receiver, simulating a non-self action
        let receiver_id = "another_account.testnet";
        let actions = vec![Action::Transfer(TransferAction { deposit: 1000 })]; // Non-key management action

        let signed_delegate_action =
            create_signed_delegate_action(Some(sender_id), Some(receiver_id), Some(actions), None);

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(
            result.is_err(),
            "Expected error validation for invalid fastauth scenario with contract whitelisting."
        );
    }

    #[tokio::test]
    async fn test_both_whitelists_enabled_without_relevant_entries() {
        let app_state = create_app_state(
            true,                                               // use_whitelisted_contracts
            true,                                               // use_whitelisted_senders
            Some(vec!["allowed_contract.testnet".to_string()]), // whitelisted_contracts
            Some(vec!["allowed_sender.testnet".to_string()]),   // whitelisted_senders
            false,                                              // use_exchange
        )
        .await;
        let signed_delegate_action = create_signed_delegate_action(
            Some("unlisted_sender.testnet"), // sender_id not in whitelisted_senders
            Some("unlisted_contract.testnet"), // receiver_id not in whitelisted_contracts
            None,                            // Default actions
            None,                            // Default nonce
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(
            result.is_err(),
            "Expected validation to fail when both sender and receiver are not whitelisted."
        );
    }

    #[tokio::test]
    async fn test_exchange_feature_with_sender_whitelist() {
        let app_state = create_app_state(
            false,                                             // use_whitelisted_contracts
            true,                                              // use_whitelisted_senders
            None,                                              // whitelisted_contracts
            Some(vec!["exchange_sender.testnet".to_string()]), // whitelisted_senders
            true,                                              // use_exchange
        )
        .await;
        let actions = vec![Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: FT_TRANSFER_METHOD_NAME.to_string(),
            args: BASE64_ENGINE
                .encode("{\"receiver_id\":\"valid_receiver.testnet\",\"amount\":\"1000\"}")
                .into_bytes(),
            gas: 300_000_000_000_000,
            deposit: FT_TRANSFER_ATTACHMENT_DEPOSIT_AMOUNT,
        }))];
        let signed_delegate_action = create_signed_delegate_action(
            Some("exchange_sender.testnet"), // sender_id in whitelisted_senders
            Some("exchange.testnet"),        // receiver_id, arbitrary for this test
            Some(actions),
            None,
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);

        assert!(
            result.is_ok(),
            "Expected validation to pass for a whitelisted sender performing a valid exchange operation."
        );
    }

    #[tokio::test]
    async fn test_pay_with_ft_and_exchange_conflict() {
        let mut app_state = create_app_state(
            false,                                       // use_whitelisted_contracts
            true,                                        // use_whitelisted_senders
            None,                                        // whitelisted_contracts
            Some(vec!["ft_sender.testnet".to_string()]), // whitelisted_senders
            true,                                        // use_exchange enabled
        )
        .await;
        app_state.config.use_pay_with_ft = true;
        app_state.config.burn_address = "burn_address.testnet".to_string();

        // Attempting an FT transfer not to the burn address
        let actions = vec![Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: FT_TRANSFER_METHOD_NAME.to_string(),
            args: BASE64_ENGINE
                .encode("{\"receiver_id\":\"another_address.testnet\",\"amount\":\"1000\"}")
                .into_bytes(),
            gas: 300_000_000_000_000,
            deposit: FT_TRANSFER_ATTACHMENT_DEPOSIT_AMOUNT,
        }))];
        let signed_delegate_action = create_signed_delegate_action(
            Some("ft_sender.testnet"),    // sender_id in whitelisted_senders
            Some("some_service.testnet"), // receiver_id, arbitrary for this test
            Some(actions),
            None,
        );

        let result = validate_signed_delegate_action(&app_state, &signed_delegate_action);
        println!("{result:?}");

        assert!(
            result.is_err(),
            "Expected validation to fail due to conflict between use_pay_with_ft and the exchange operation not targeting the burn address."
        );
    }

    #[tokio::test]
    /// Tests that get_redis_cnxn returns a valid Redis connection
    async fn test_get_redis_cnxn() {
        let redis_pool = create_test_redis_pool().await;
        let result = get_redis_cnxn(&redis_pool).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_allowance() {
        let redis_pool = create_test_redis_pool().await;
        let account_id = "test_account.testnet";
        let allowance: u64 = 90000000000000;

        // Set an allowance for the test account
        set_account_and_allowance_in_redis(&redis_pool, account_id, &allowance)
            .await
            .unwrap();

        // Attempt to get the allowance for the test account
        let result = get_remaining_allowance(&redis_pool, &account_id.parse().unwrap()).await;

        assert_eq!(result.unwrap(), allowance);

        // Clean up
        let _: () = redis_pool.get().unwrap().del(account_id).unwrap();
    }

    #[tokio::test]
    async fn test_update_allowance() {
        let redis_pool = create_test_redis_pool().await;
        let account_id = "test_update_account.testnet";
        let initial_allowance: u64 = 50000000000000;
        let updated_allowance: u64 = 100000000000000;

        // Set an initial allowance for the account
        set_account_and_allowance_in_redis(&redis_pool, account_id, &initial_allowance)
            .await
            .unwrap();

        // Update the allowance for the account
        set_account_and_allowance_in_redis(&redis_pool, account_id, &updated_allowance)
            .await
            .unwrap();

        // Retrieve and assert the updated allowance
        let result = get_remaining_allowance(&redis_pool, &account_id.parse().unwrap()).await;
        assert_eq!(result.unwrap(), updated_allowance);

        // Clean up
        let _: () = redis_pool.get().unwrap().del(account_id).unwrap();
    }

    #[tokio::test]
    /// Tests that update_all_allowances_in_redis updates all keys with the provided allowance
    async fn test_update_all_allowances_in_redis() {
        let redis_pool = create_test_redis_pool();
        let allowance_in_gas = 1000000000;
        let network_env = "testnet";

        let result =
            update_all_allowances_in_redis(&redis_pool.await, network_env, allowance_in_gas).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_register_account_and_allowance() {
        let app_state = create_app_state(false, false, None, None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);

        let account_id = "test_register_account.testnet";
        let allowance: u64 = 90000000000000;
        let oauth_token = "unique_oauth_token";
        let redis_pool = create_test_redis_pool().await;

        let account_id_and_allowance_oauth_json = AccountIdAllowanceOauthJson {
            account_id: account_id.to_string(),
            allowance,
            oauth_token: oauth_token.to_string(),
        };

        // Register the account with an allowance and an OAuth token
        register_account_and_allowance(axum_state, axum::Json(account_id_and_allowance_oauth_json))
            .await
            .into_response();

        // Verify the allowance was set
        let allowance_result =
            get_remaining_allowance(&redis_pool, &account_id.parse().unwrap()).await;
        assert_eq!(allowance_result.unwrap(), allowance);

        // Verify the OAuth token was set
        let oauth_token_result: bool = redis_pool.get().unwrap().exists(oauth_token).unwrap();
        assert!(oauth_token_result);

        // Clean up
        let _: () = redis_pool.get().unwrap().del(account_id).unwrap();
        let _: () = redis_pool.get().unwrap().del(oauth_token).unwrap();
    }

    #[test]
    /// Tests that create_redis_pool creates a valid Redis pool
    fn test_create_redis_pool() {
        let config = RelayerConfiguration::default();
        let redis_pool = create_redis_pool(&config);

        assert!(redis_pool.get().is_ok());
    }

    // TODO Add more tests for other relevant functions here...

    fn create_empty_signed_delegate_action(
        sender_id: String,
        receiver_id: String,
        actions: Vec<Action>,
        nonce: i32,
        max_block_height: i32,
    ) -> SignedDelegateAction {
        let max_block_height: i32 = max_block_height;
        let public_key: PublicKey = PublicKey::empty(ED25519);
        let signature: Signature = Signature::empty(ED25519);
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

    #[tokio::test]
    // NOTE: uncomment ignore locally to run test bc redis doesn't work in github action build env
    async fn test_send_meta_tx_invalid_signature() {
        // tests assume testnet in config
        // Test Transfer Action
        let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
        let sender_id: String = String::from("relayer_test0.testnet");
        let receiver_id: String = String::from("relayer_test1.testnet");
        let nonce: i32 = 1;
        let max_block_height = 2_000_000_000;

        // simulate calling the '/update_allowance' function with sender_id & allowance
        let allowance_in_gas: u64 = u64::MAX;
        set_account_and_allowance_in_redis(
            &create_test_redis_pool().await,
            &sender_id,
            &allowance_in_gas,
        )
        .await
        .expect("Failed to update account and allowance in redis");

        // Call the `/send_meta_tx` function happy path
        let signed_delegate_action = create_empty_signed_delegate_action(
            sender_id.clone(),
            receiver_id.clone(),
            actions.clone(),
            nonce,
            max_block_height,
        );
        let json_payload = Json(signed_delegate_action);
        println!("SignedDelegateAction Json Serialized (no borsh): {json_payload:?}");
        let app_state = create_app_state(false, false, None, None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);
        let response: Response = send_meta_tx(axum_state, json_payload).await.into_response();
        let response_status: StatusCode = response.status();
        let body: BoxBody = response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        println!("Response body: {body_str:?}");
        assert_eq!(response_status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body_str.contains("DelegateActionInvalidSignature"))
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
        let sda2 = create_empty_signed_delegate_action(
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
        let app_state = create_app_state(false, false, None, None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);
        let err_response = send_meta_tx(axum_state, non_whitelist_json_payload)
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
    async fn test_sign_meta_tx() {
        let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
        let sender_id: String = String::from("nomnomnom.testnet");
        let receiver_id: String = String::from("relayer_test0.testnet");
        let nonce: i64 = 103066617000686;
        let max_block_height = 122790412;
        let pk_str: String = "ed25519:89GtfFzez3opomVpwa7i4m3nptHtc7Ha514XHMWszQtL".to_string();
        let public_key: PublicKey = PublicKey::from_str(&pk_str).unwrap();
        let signature: Signature = "ed25519:5uJu7KapH89h9cQm5btE1DKnbiFXSZNT7McDw5LHy8pdAt5Mz9DfuyQZadGgFExo88or9152iwcw2q12rnFWa6bg".parse().unwrap();

        // Call the `send_meta_tx` function with a sender that has no gas allowance
        // (and a receiver_id that isn't in whitelist)
        let sda = SignedDelegateAction {
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
        };
        let signing_pk = "ed25519:89GtfFzez3opomVpwa7i4m3nptHtc7Ha514XHMWszQtL".to_string();
        let json_payload = Json(PublicKeyAndSDAJson {
            public_key: signing_pk,
            signed_delegate_action: sda,
            nonce: Some(103066617000687),
            block_hash: Some("D2xZEsPM2Z2xRiFwTyr1V5neqP4ukqzMPthpWrhUdgwV".to_string()),
        });
        println!("PublicKeyAndSDAJson: {json_payload:?}");
        let app_state = create_app_state(true, false, Some(vec![]), None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);
        let response = sign_meta_tx(axum_state, json_payload).await.into_response();
        let response_status = response.status();
        let body: BoxBody = response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        println!("Response body: {body_str:?}");
        assert_eq!(response_status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_sign_meta_tx_no_filter() {
        let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
        let sender_id: String = String::from("nomnomnom.testnet");
        let receiver_id: String = String::from("relayer_test0.testnet");
        let nonce: i64 = 103066617000686;
        let max_block_height = 122790412;
        let pk_str: String = "ed25519:89GtfFzez3opomVpwa7i4m3nptHtc7Ha514XHMWszQtL".to_string();
        let public_key: PublicKey = PublicKey::from_str(&pk_str).unwrap();
        let signature: Signature = "ed25519:5uJu7KapH89h9cQm5btE1DKnbiFXSZNT7McDw5LHy8pdAt5Mz9DfuyQZadGgFExo88or9152iwcw2q12rnFWa6bg".parse().unwrap();

        // Call the `send_meta_tx` function with a sender that has no gas allowance
        // (and a receiver_id that isn't in whitelist)
        let sda = SignedDelegateAction {
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
        };
        let signing_pk = "ed25519:89GtfFzez3opomVpwa7i4m3nptHtc7Ha514XHMWszQtL".to_string();
        let json_payload = Json(PublicKeyAndSDAJson {
            public_key: signing_pk,
            signed_delegate_action: sda,
            nonce: Some(103066617000687),
            block_hash: Some("D2xZEsPM2Z2xRiFwTyr1V5neqP4ukqzMPthpWrhUdgwV".to_string()),
        });
        println!("PublicKeyAndSDAJson: {json_payload:?}");
        let app_state = create_app_state(false, false, None, None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);
        let response = sign_meta_tx_no_filter(axum_state, json_payload)
            .await
            .into_response();
        let response_status = response.status();
        let body: BoxBody = response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        println!("Response body: {body_str:?}");
        assert_eq!(response_status, StatusCode::OK);
        assert!(body_str.contains("EQAAAG5vbW5vbW5vbS50ZXN0bmV0AGogbDAp74I4+7jfoIe+ssj2bahcCgpzBdwynck4R24F7zIYEb1dAAARAAAAbm9tbm9tbm9tLnRlc3RuZXSyzKQgixHJ+s7OQ03sEfSo/Wex0Po/Sb5/R6qnM8GZ5AEAAAAIEQAAAG5vbW5vbW5vbS50ZXN0bmV0FQAAAHJlbGF5ZXJfdGVzdDAudGVzdG5ldAEAAAADAQAAAAAAAAAAAAAAAAAAAO4yGBG9XQAADKJRBwAAAAAAaiBsMCnvgjj7uN+gh76yyPZtqFwKCnMF3DKdyThHbgUA9S1L4ZRdR2HJKxgMQdTUtBgSy1rOZZ0KB7tMbBamki9StU3Pt9xPFCRc/VcXgK9n7PhHdF/KlH+BZXdc+xw3BwAW5VUjtd6PQXvDcAduSjLJyYEUg8e+NtKOZHhWmr+MTAyPuKUSDYzxdj6y/ynCOcmufg0GAn/cXyBGOMy6HkIH"));
    }

    /// Not actually a unit test of a specific fn, just tests or helper fns for specific functionality
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
    async fn test_relay() {
        // Call the `/relay` function happy path
        let app_state = create_app_state(false, false, None, None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);
        let account_id: AccountId = "relayer_test0.testnet".parse().unwrap();
        let public_key: PublicKey =
            PublicKey::from_str("ed25519:AMypJZjcMYwHCx2JFSwXAPuygDS5sy1vRNc2aoh3EjTN").unwrap();

        let (nonce, _block_hash, _) = &axum_state
            .rpc_client
            .fetch_nonce(&account_id, &public_key)
            .await
            .unwrap();

        let signed_delegate_action = create_signed_delegate_action(None, None, None, Some(*nonce));
        assert!(signed_delegate_action.verify());

        let serialized_signed_delegate_action = borsh::to_vec(&signed_delegate_action).unwrap();
        let json_payload = Json(serialized_signed_delegate_action);

        let response = relay(axum_state, json_payload).await.into_response();
        let response_status = response.status();
        let body: BoxBody = response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        println!("Response body: {body_str:?}");
        assert_eq!(response_status, StatusCode::OK);
        assert!(body_str.contains("Relayed and sent transaction"));
    }

    #[tokio::test]
    async fn test_relay_with_bad_signature() {
        let app_state = create_app_state(false, false, None, None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);
        let account_id: AccountId = "relayer_test0.testnet".parse().unwrap();
        let public_key: PublicKey =
            PublicKey::from_str("ed25519:AMypJZjcMYwHCx2JFSwXAPuygDS5sy1vRNc2aoh3EjTN").unwrap();

        let (nonce, _block_hash, _) = &axum_state
            .rpc_client
            .fetch_nonce(&account_id, &public_key)
            .await
            .unwrap();

        let mut signed_delegate_action =
            create_signed_delegate_action(None, None, None, Some(*nonce));
        signed_delegate_action.signature = "ed25519:5uJu7KapH89h9cQm5btE1DKnbiFXSZNT7McDw5LHy8pdAt5Mz9DfuyQZadGgFExo88or9152iwcw2q12rnFWa6bg".parse().unwrap();

        let serialized_signed_delegate_action = borsh::to_vec(&signed_delegate_action).unwrap();
        let json_payload = Json(serialized_signed_delegate_action);

        let response = relay(axum_state, json_payload).await.into_response();
        let response_status = response.status();
        let body: BoxBody = response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        println!("Response body: {body_str:?}");
        assert_eq!(response_status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body_str.contains("DelegateActionInvalidSignature"));
    }

    #[tokio::test]
    async fn test_relay_with_bad_nonce() {
        let app_state = create_app_state(false, false, None, None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);

        let nonce = 69;
        let signed_delegate_action = create_signed_delegate_action(None, None, None, Some(nonce));
        assert!(signed_delegate_action.verify());

        let serialized_signed_delegate_action = borsh::to_vec(&signed_delegate_action).unwrap();
        let json_payload = Json(serialized_signed_delegate_action);

        let response = relay(axum_state, json_payload).await.into_response();
        let response_status = response.status();
        let body: BoxBody = response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        println!("Response body: {body_str:?}");
        assert_eq!(response_status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body_str.contains("DelegateActionInvalidNonce"));
    }

    #[tokio::test]
    async fn test_relay_bad_payload() {
        let app_state = create_app_state(false, false, None, None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);

        let json_payload = Json(vec![]);
        let response = relay(axum_state, json_payload).await.into_response();
        let response_status = response.status();
        let body: BoxBody = response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        println!("Response body: {body_str:?}");
        assert_eq!(response_status, StatusCode::BAD_REQUEST);
        assert!(body_str.contains("Error deserializing payload data object"));
    }

    #[tokio::test]
    async fn test_send_meta_tx_async() {
        // Setup test environment
        let app_state = create_app_state(false, false, None, None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);

        // Create a valid SignedDelegateAction as done in test_relay()
        let nonce = 1; // Simplified for example; in practice, fetch or mock a valid nonce
        let signed_delegate_action = create_signed_delegate_action(None, None, None, Some(nonce));
        assert!(signed_delegate_action.verify()); // Optional, verify the signature for correctness

        // Serialize the SignedDelegateAction
        let json_payload = Json(signed_delegate_action);

        // Call the /send_meta_tx_async endpoint
        let response = send_meta_tx_async(axum_state, json_payload)
            .await
            .into_response();

        // Verify the response status is OK (200)
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_send_meta_tx_nopoll() {
        // Setup test environment
        let app_state = create_app_state(false, false, None, None, false).await;
        let axum_state: State<Arc<AppState>> = convert_app_state_to_arc_app_state(app_state);

        // Create a valid SignedDelegateAction as done in test_relay()
        let nonce = 1; // Simplified for example; in practice, fetch or mock a valid nonce
        let signed_delegate_action = create_signed_delegate_action(None, None, None, Some(nonce));
        assert!(signed_delegate_action.verify()); // Optional, verify the signature for correctness

        // Serialize the SignedDelegateAction
        let json_payload = Json(signed_delegate_action);

        // Call the /send_meta_tx_nopoll endpoint
        let response = send_meta_tx_nopoll(axum_state, json_payload)
            .await
            .into_response();

        // Verify the response status is OK (200)
        assert_eq!(response.status(), StatusCode::OK);
    }
}
