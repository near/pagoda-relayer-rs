mod error;
mod redis_fns;
mod rpc_conf;
mod shared_storage;

#[cfg(test)]
use axum::body::{BoxBody, HttpBody};
use axum::extract::State;
#[cfg(test)]
use axum::response::Response;
use axum::{
    extract::Json,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
#[cfg(test)]
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as BASE64_ENGINE, Engine as _};
#[cfg(test)]
use bytes::BytesMut;
use near_crypto::InMemorySigner;
#[cfg(test)]
use near_crypto::{KeyType, PublicKey, Signature};
use near_fetch::signer::KeyRotatingSigner;
use near_primitives::borsh::BorshDeserialize;
#[cfg(test)]
use near_primitives::borsh::BorshSerialize;
use near_primitives::delegate_action::SignedDelegateAction;
#[cfg(test)]
use near_primitives::delegate_action::{DelegateAction, NonDelegateAction};
#[cfg(test)]
use near_primitives::transaction::TransferAction;
use near_primitives::transaction::{Action, FunctionCallAction};
use near_primitives::types::AccountId;
#[cfg(test)]
use near_primitives::types::{BlockHeight, Nonce};
use r2d2::{Pool, PooledConnection};
use r2d2_redis::redis::{Commands, ErrorKind::IoError, RedisError};
use r2d2_redis::RedisConnectionManager;
use rpc_conf::ApiKey;
use serde::Deserialize;
use serde_json::{json, Value};
#[cfg(test)]
use std::mem;
use std::str::FromStr;
use std::sync::Arc;
use std::{collections::HashMap, net::SocketAddr};
use std::{collections::HashSet, string::ToString};
use std::{fmt, path::Path};
use std::{
    fmt::{Debug, Display, Formatter},
    fs,
};
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::log::error;
use tracing::{debug, info, warn};
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

// Silly workaround for this ancient bug
// https://github.com/serde-rs/serde/issues/368
const fn bool<const V: bool>() -> bool {
    V
}

#[derive(Deserialize, Debug)]
struct ConfigFile {
    network: String,
    rpc_url: url::Url,
    rpc_api_key: Option<ApiKey>,
    wallet_url: url::Url,
    explorer_transaction_url: url::Url,
    ip_address: [u8; 4],
    port: u16,
    relayer_account_id: AccountId,
    shared_storage_account_id: Option<AccountId>,
    keys_filenames: Vec<String>,
    shared_storage_keys_filename: Option<String>,
    whitelisted_contracts: Vec<AccountId>,
    #[serde(default = "bool::<false>")]
    use_whitelisted_delegate_action_receiver_ids: bool,
    whitelisted_delegate_action_receiver_ids: Vec<AccountId>,
    #[serde(default = "bool::<false>")]
    use_redis: bool,
    redis_url: Option<String>,
    #[serde(default = "bool::<false>")]
    use_fastauth_features: bool,
    #[serde(default = "bool::<false>")]
    use_pay_with_ft: bool,
    burn_address: Option<AccountId>,
    #[serde(default = "bool::<false>")]
    use_shared_storage: bool,
    social_db_contract_id: Option<AccountId>,
}

impl ConfigFile {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let contents =
            fs::read_to_string(path).map_err(|e| format!("failed to read path, {}", e))?;
        toml::from_str(&contents).map_err(|e| format!("failed to parse file, {}", e))
    }
}

#[derive(Clone, Debug)]
struct PayWithFT {
    burn_address: AccountId,
}

pub enum Allowances {
    Redis {
        connection: Pool<RedisConnectionManager>,
    },
    // This should only be used in testing really because it doesn't persist values between runs
    InMemory {
        key_value: RwLock<HashMap<AccountId, u64>>,
    },
}

impl Allowances {
    fn new(redis_pool: Option<Pool<RedisConnectionManager>>) -> Self {
        match redis_pool {
            Some(connection) => Allowances::Redis { connection },
            None => Allowances::InMemory {
                key_value: RwLock::new(HashMap::new()),
            },
        }
    }

    async fn get(&self, account_id: &AccountId) -> Result<u64, RedisError> {
        match self {
            Allowances::Redis { connection } => {
                get_remaining_allowance(connection, account_id).await
            }
            Allowances::InMemory { key_value } => {
                Ok(key_value.read().await.get(account_id).cloned().unwrap_or(0))
            }
        }
    }

    async fn set(&self, account_id: AccountId, allowance: u64) -> Result<(), RedisError> {
        match self {
            Allowances::Redis { connection } => {
                set_account_and_allowance_in_redis(connection, account_id.as_str(), &allowance)
                    .await
            }
            Allowances::InMemory { key_value } => {
                key_value.write().await.insert(account_id, allowance);
                Ok(())
            }
        }
    }

    async fn keys(&self, network: &str) -> Result<Vec<AccountId>, String> {
        // Fetch all keys that match the network env (.near for mainnet, .testnet for testnet, etc)
        // DMD: I think this should be replaced because it doesn't support alternate TLDs or implicit accounts
        let top_level_account: &str = if network != "mainnet" {
            network
        } else {
            "near"
        };
        match self {
            Allowances::Redis { connection } => {
                let pattern = format!("*.{}", top_level_account);
                let mut redis_conn = get_redis_cnxn(connection).await.map_err(|e| {
                    format!("Error getting Relayer DB connection from the pool: {e}")
                })?;

                let fetched_keys: Vec<String> = redis_conn
                    .keys(pattern)
                    .map_err(|e| format!("Error fetching keys from Relayer DB: {}", e))?;

                fetched_keys
                    .into_iter()
                    .map(|k| {
                        AccountId::from_str(&k).map_err(|e| {
                            let err_msg = format!("Couldn't parse returned key, {e}");
                            error!("{err_msg}");
                            err_msg
                        })
                    })
                    .collect()
            }
            Allowances::InMemory { key_value } => {
                let top_level_account = AccountId::from_str(top_level_account).map_err(|e| {
                    format!("Config network account is misconfigured and can't be parsed: {e}")
                })?;
                let key_value = key_value.read().await;
                let res = key_value
                    .keys()
                    .filter(|a| a.is_sub_account_of(&top_level_account))
                    .cloned()
                    .collect();
                Ok(res)
            }
        }
    }
}

enum OauthTokens {
    Redis {
        connection: Pool<RedisConnectionManager>,
    },
    InMemory {
        key_value: RwLock<HashSet<String>>,
    },
}

impl OauthTokens {
    fn new(redis_pool: Option<Pool<RedisConnectionManager>>) -> Self {
        match redis_pool {
            Some(connection) => OauthTokens::Redis { connection },
            None => OauthTokens::InMemory {
                key_value: RwLock::new(HashSet::new()),
            },
        }
    }

    /// TODO are these tokens, or accounts derived from tokens?
    pub async fn insert(&self, token: String) -> Result<bool, RedisError> {
        match self {
            OauthTokens::Redis { connection } => set_oauth_token_in_redis(connection, token).await,
            OauthTokens::InMemory { key_value } => Ok(key_value.write().await.insert(token)),
        }
    }
    pub async fn contains(&self, token: &str) -> Result<bool, RedisError> {
        match self {
            OauthTokens::Redis { connection } => get_oauth_token_in_redis(connection, token).await,
            OauthTokens::InMemory { key_value } => Ok(key_value.read().await.contains(token)),
        }
    }
}

pub struct DConfig {
    port: u16,
    ip_address: [u8; 4],
    relayer_account_id: AccountId,
    shared_storage_pool: Option<SharedStoragePoolManager>,
    use_fastauth_features: bool,
    whitelisted_delegate_action_receiver_ids: Vec<AccountId>,
    network: String,
    allowances: Allowances,
    oauth_tokens: OauthTokens,
    rpc_client: Arc<near_fetch::Client>,
    pay_with_ft: Option<PayWithFT>,
    use_whitelisted_delegate_action_receiver_ids: bool,
    whitelisted_contracts: Vec<AccountId>,
    signer: KeyRotatingSigner,
}

impl DConfig {
    pub fn production() -> Result<DConfig, String> {
        Self::validate_configuration(Path::new("config.toml"))
    }

    /// Checks if the user has any errors in their config file.
    ///
    /// Validates the config file matches the types.
    /// Checks that the flags match the keys present.
    /// Checks all referenced files and urls exist.
    pub fn validate_configuration(file: &Path) -> Result<DConfig, String> {
        Self::validate_configuration_internal(file)
            .map_err(|e| format!("In file {}: {}", file.display(), e))
    }

    /// Does the work of parsing the raw config, but doesn't attach a filename
    fn validate_configuration_internal(file: &Path) -> Result<DConfig, String> {
        let ConfigFile {
            network,
            rpc_url,
            rpc_api_key,
            wallet_url,
            explorer_transaction_url,
            ip_address,
            port,
            relayer_account_id,
            shared_storage_account_id,
            keys_filenames,
            shared_storage_keys_filename,
            whitelisted_contracts,
            use_whitelisted_delegate_action_receiver_ids,
            whitelisted_delegate_action_receiver_ids,
            use_redis,
            redis_url,
            use_fastauth_features,
            use_pay_with_ft,
            burn_address,
            use_shared_storage,
            social_db_contract_id,
        } = ConfigFile::from_file(file)?;

        let rpc_client: Arc<near_fetch::Client> = Arc::new({
            let network_config = NetworkConfig {
                rpc_url,
                rpc_api_key,
                wallet_url,
                explorer_transaction_url,
            };
            network_config.rpc_client()
        });

        let signer: KeyRotatingSigner = {
            KeyRotatingSigner::from_signers(keys_filenames.iter().map(|path| {
                InMemorySigner::from_file(Path::new(path)).unwrap_or_else(|err| {
                    panic!("failed to read signing keys from {path}: {err:?}")
                })
            }))
        };

        /// We use the pattern use_* in our config files to enable and require certain config keys
        /// If the use_flag is true, we require the key if it's false we forbid it
        #[must_use]
        fn flagged_so_expect<A>(
            use_flag_name: &str,
            use_flag: bool,
            config_key_name: &str,
            config_key: Option<A>,
        ) -> Result<Option<A>, String> {
            match (use_flag, config_key) {
                (true, Some(a)) => Ok(Some(a)),
                (false, None) => Ok(None),
                (true, None) => Err(format!(
                    "field '{}' is true so expected field '{}'",
                    use_flag_name, config_key_name
                )),
                (false, Some(_)) => Err(format!(
                    "field '{}' is missing or false so didn't expect field '{}'",
                    use_flag_name, config_key_name
                )),
            }
        }

        // Parse redis options
        let redis_url = flagged_so_expect("use_redis", use_redis, "redis_url", redis_url)?;

        let redis_pool: Option<Pool<RedisConnectionManager>> = redis_url.map(|redis_url| {
            let manager = RedisConnectionManager::new(redis_url).unwrap();
            Pool::builder().build(manager).unwrap()
        });

        let allowances = Allowances::new(redis_pool.clone());
        let oauth_tokens = OauthTokens::new(redis_pool.clone());
        // Parse shared_storage options
        let shared_storage_account_id = flagged_so_expect(
            "use_shared_storage",
            use_shared_storage,
            "shared_storage_account_id",
            shared_storage_account_id,
        )?;

        let shared_storage_keys_filename = flagged_so_expect(
            "use_shared_storage",
            use_shared_storage,
            "shared_storage_keys_filename",
            shared_storage_keys_filename,
        )?;

        let shared_storage_pool = match (
            social_db_contract_id,
            shared_storage_account_id,
            shared_storage_keys_filename,
        ) {
            (
                Some(social_db_contract_id),
                Some(shared_storage_account_id),
                Some(shared_storage_keys_filename),
            ) => {
                let signer =
                    InMemorySigner::from_file(Path::new(shared_storage_keys_filename.as_str()))
                        .map_err(|err| {
                            format!(
                            "failed to get signing keys={shared_storage_keys_filename:?}: {err:?}"
                        )
                        })?;
                Some(SharedStoragePoolManager::new(
                    signer,
                    rpc_client.clone(),
                    social_db_contract_id,
                    shared_storage_account_id,
                ))
            }
            (None, None, None) => None,
            _ => unreachable!("'flagged_so_expect' should make this impossible to reach"),
        };

        // Parse pay with FT options
        let burn_address = flagged_so_expect(
            "use_pay_with_ft",
            use_pay_with_ft,
            "burn_address",
            burn_address,
        )?;

        let pay_with_ft = burn_address.map(|burn_address| (PayWithFT { burn_address }));

        Ok(DConfig {
            whitelisted_delegate_action_receiver_ids,
            use_fastauth_features,
            shared_storage_pool,
            relayer_account_id,
            ip_address,
            port,
            network,
            allowances,
            oauth_tokens,
            rpc_client,
            pay_with_ft,
            use_whitelisted_delegate_action_receiver_ids,
            whitelisted_contracts,
            signer,
        })
    }
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
struct AccountIdAllowanceOauthSDAJson {
    #[schema(example = "example.near")]
    account_id: AccountId,
    #[schema(example = 900000000)]
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
    // TODO work out if changing this from 'String' changes the schema
    #[schema(example = "example.near")]
    account_id: AccountId,
    #[schema(example = 900000000)]
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
    // TODO check this doesn't change the schema
    #[schema(example = "example.near")]
    account_id: AccountId,
    #[schema(example = 900000000)]
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
    #[schema(example = 900000000)]
    allowance_in_gas: u64,
}
impl Display for AllowanceJson {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "allowance in Gas: {}", self.allowance_in_gas)
    }
}

async fn init_senders_infinite_allowance_fastauth(
    allowances: &Allowances,
    whitelisted_delegate_action_receiver_ids: &[AccountId],
) {
    let max_allowance = u64::MAX;
    for whitelisted_sender in whitelisted_delegate_action_receiver_ids.iter() {
        let redis_result = allowances
            .set(whitelisted_sender.clone(), max_allowance)
            .await;
        if let Err(err) = redis_result {
            error!(
                "Error setting allowance for account_id {} with allowance {} in Relayer DB: {:?}",
                whitelisted_sender, max_allowance, err,
            );
        } else {
            info!(
                "Set allowance for account_id {} with allowance {} in Relayer DB",
                whitelisted_sender.clone().as_str(),
                max_allowance,
            );
        }
    }
}

#[tokio::main]
async fn main() {
    // TODO Handle this error better
    let config = Arc::new(DConfig::production().unwrap());

    // initialize tracing (aka logging)
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .init();

    // initialize our shared storage pool manager if using fastauth features or using shared storage
    if config.use_fastauth_features.clone() {
        if let Some(shared_storage_pool) = &config.shared_storage_pool {
            if let Err(err) = shared_storage_pool.check_and_spawn_pool().await {
                let err_msg = format!("Error initializing shared storage pool: {err}");
                error!("{err_msg}");
                tracing::error!(err_msg);
                return;
            } else {
                info!("shared storage pool initialized")
            }
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
    if config.use_fastauth_features {
        init_senders_infinite_allowance_fastauth(
            &config.allowances,
            &config.whitelisted_delegate_action_receiver_ids,
        )
        .await;
    }

    // build our application with a route
    let app: Router = Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        // There is no need to create `RapiDoc::with_openapi` because the OpenApi is served
        // via SwaggerUi instead we only make rapidoc to point to the existing doc.
        .merge(RapiDoc::new("/api-docs/openapi.json").path("/rapidoc"))
        // Alternative to above
        .merge(RapiDoc::with_openapi("/api-docs/openapi2.json", ApiDoc::openapi()).path("/rapidoc"))
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
        .layer(cors_layer)
        .with_state(config.clone());

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr: SocketAddr = SocketAddr::from((config.ip_address.clone(), config.port.clone()));
    info!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
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
async fn get_allowance(
    State(config): State<Arc<DConfig>>,
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
    match config.allowances.get(&account_id_val).await {
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
                "Error getting allowance for account_id {} in Relayer DB: {:?}",
                account_id_val, err
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
async fn create_account_atomic(
    config: State<Arc<DConfig>>,
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
    let account_id = &account_id_allowance_oauth_sda.account_id;
    let allowance_in_gas: u64 = account_id_allowance_oauth_sda.allowance;
    let oauth_token: &String = &account_id_allowance_oauth_sda.oauth_token;
    let sda: SignedDelegateAction = account_id_allowance_oauth_sda
        .signed_delegate_action
        .clone();

    /*
       do logic similar to register_account_and_allowance fn
       without updating redis or allocating shared storage
       if that fails, return error
       if it succeeds, then continue
    */

    // check if the oauth_token has already been used and is a key in Redis
    match config.oauth_tokens.contains(oauth_token).await {
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
    let redis_result = config
        .allowances
        .set(account_id.clone(), allowance_in_gas)
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
    let create_account_sda_result = process_signed_delegate_action(&config, sda).await;
    if create_account_sda_result.is_err() {
        let err: RelayError = create_account_sda_result.err().unwrap();
        return (err.status_code, err.message).into_response();
    }
    let Ok(account_id) = account_id.parse::<AccountId>() else {
        let err_msg = format!("Invalid account_id: {account_id}");
        warn!("{err_msg}");
        return (StatusCode::BAD_REQUEST, err_msg).into_response();
    };

    // allocate shared storage for account_id if shared storage is being used
    if let Some(shared_storage_pool) = &config.shared_storage_pool {
        if config.use_fastauth_features.clone() {
            if let Err(err) = shared_storage_pool
                .allocate_default(account_id.clone())
                .await
            {
                let err_msg = format!("Error allocating storage for account {account_id}: {err:?}");
                error!("{err_msg}");
                return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
            }
        }
    }

    // add oauth token to redis (key: oauth_token, val: true)
    match &config.oauth_tokens.insert(oauth_token.clone()).await {
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
async fn update_allowance(
    State(config): State<Arc<DConfig>>,
    account_id_allowance: Json<AccountIdAllowanceJson>,
) -> impl IntoResponse {
    let account_id = &account_id_allowance.account_id;
    let allowance_in_gas: u64 = account_id_allowance.allowance;

    let redis_result = config
        .allowances
        .set(account_id.clone(), allowance_in_gas)
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
async fn update_all_allowances(
    State(config): State<Arc<DConfig>>,
    Json(allowance_json): Json<AllowanceJson>,
) -> impl IntoResponse {
    let allowance_in_gas = allowance_json.allowance_in_gas;
    let redis_response = update_all_allowances_in_redis(&config, allowance_in_gas).await;
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
async fn register_account_and_allowance(
    State(config): State<Arc<DConfig>>,
    account_id_allowance_oauth: Json<AccountIdAllowanceOauthJson>,
) -> impl IntoResponse {
    let account_id = &account_id_allowance_oauth.account_id;
    let allowance_in_gas: &u64 = &account_id_allowance_oauth.allowance;
    let oauth_token: &String = &account_id_allowance_oauth.oauth_token;
    // check if the oauth_token has already been used and is a key in Relayer DB
    match config.oauth_tokens.insert(oauth_token.clone()).await {
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
    let redis_result = config
        .allowances
        .set(account_id.clone(), *allowance_in_gas)
        .await;

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

    if let Some(shared_storage_pool) = &config.shared_storage_pool {
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
    match config.oauth_tokens.insert(oauth_token.clone()).await {
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
async fn relay(State(config): State<Arc<DConfig>>, data: Json<Vec<u8>>) -> impl IntoResponse {
    // deserialize SignedDelegateAction using borsh
    match SignedDelegateAction::try_from_slice(&data.0) {
        Ok(signed_delegate_action) => {
            match process_signed_delegate_action(&config, signed_delegate_action).await {
                Ok(response) => response.into_response(),
                Err(err) => (err.status_code, err.message).into_response(),
            }
        }
        Err(e) => {
            let err_msg = format!(
                "{}: {:?}",
                "Error deserializing payload data object",
                e.to_string(),
            );
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
async fn send_meta_tx(
    config: State<Arc<DConfig>>,
    data: Json<SignedDelegateAction>,
) -> impl IntoResponse {
    let relayer_response = process_signed_delegate_action(
        &config, // deserialize SignedDelegateAction using serde json
        data.0,
    )
    .await;
    match relayer_response {
        Ok(response) => response.into_response(),
        Err(err) => (err.status_code, err.message).into_response(),
    }
}

async fn process_signed_delegate_action(
    config: &DConfig,
    signed_delegate_action: SignedDelegateAction,
) -> Result<String, RelayError> {
    debug!(
        "Deserialized SignedDelegateAction object: {:#?}",
        signed_delegate_action
    );

    // create Transaction from SignedDelegateAction
    let signer_account_id = &config.relayer_account_id;
    // the receiver of the txn is the sender of the signed delegate action
    let receiver_id = signed_delegate_action.delegate_action.sender_id.clone();
    let da_receiver_id = signed_delegate_action.delegate_action.receiver_id.clone();

    // check that the delegate action receiver_id is in the whitelisted_contracts
    let is_whitelisted_da_receiver = config
        .whitelisted_contracts
        .iter()
        .any(|s| s == &da_receiver_id);
    if !is_whitelisted_da_receiver {
        let err_msg = format!(
            "Delegate Action receiver_id {} is not whitelisted",
            da_receiver_id.as_str(),
        );
        warn!("{err_msg}");
        return Err(RelayError {
            status_code: StatusCode::BAD_REQUEST,
            message: err_msg,
        });
    }
    // check the sender_id in whitelist if applicable
    if config.use_whitelisted_delegate_action_receiver_ids.clone()
        && !config.use_fastauth_features.clone()
    {
        // check if the delegate action receiver_id (account sender_id) if a whitelisted delegate action receiver
        let is_whitelisted_sender = config
            .whitelisted_delegate_action_receiver_ids
            .iter()
            .any(|s| s == &receiver_id);
        if !is_whitelisted_sender {
            let err_msg = format!(
                "Delegate Action receiver_id {} or sender_id {} is not whitelisted",
                da_receiver_id.as_str(),
                receiver_id.as_str(),
            );
            warn!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::BAD_REQUEST,
                message: err_msg,
            });
        }
    }
    if !is_whitelisted_da_receiver.clone() && config.use_fastauth_features.clone() {
        // check if sender id and receiver id are the same AND (AddKey or DeleteKey action)
        let non_delegate_action = signed_delegate_action
            .delegate_action
            .actions
            .get(0)
            .ok_or_else(|| {
                let err_msg = format!("DelegateAction must have at least one NonDelegateAction");
                warn!("{err_msg}");
                RelayError {
                    status_code: StatusCode::BAD_REQUEST,
                    message: err_msg,
                }
            })?;
        let contains_key_action = matches!(
            (*non_delegate_action).clone().into(),
            Action::AddKey(_) | Action::DeleteKey(_)
        );
        // check if the receiver_id (delegate action sender_id) if a whitelisted delegate action receiver
        let is_whitelisted_sender = config
            .whitelisted_delegate_action_receiver_ids
            .iter()
            .any(|s| s == &receiver_id);
        if (receiver_id != da_receiver_id || !contains_key_action) && !is_whitelisted_sender {
            let err_msg = format!(
                "Delegate Action receiver_id {} or sender_id {} is not whitelisted OR \
                (they do not match AND the NonDelegateAction is not AddKey or DeleteKey)",
                da_receiver_id.as_str(),
                receiver_id.as_str(),
            );
            warn!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::BAD_REQUEST,
                message: err_msg,
            });
        }
    }

    // Check if the SignedDelegateAction includes a FunctionCallAction that transfers FTs to BURN_ADDRESS
    if let Some(PayWithFT { burn_address }) = &config.pay_with_ft {
        let non_delegate_actions = signed_delegate_action.delegate_action.get_actions();
        let treasury_payments: Vec<Action> = non_delegate_actions
            .iter()
            .filter_map(|action| {
                if let Action::FunctionCall(FunctionCallAction { args, .. }) = action {
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
                    debug!("BURN_ADDRESS.to_string(): {:?}", burn_address.to_string());

                    // Check if receiver_id in args contain BURN_ADDRESS
                    if receiver_id == burn_address.to_string() {
                        debug!("SignedDelegateAction contains the BURN_ADDRESS MATCH");
                        return Some(action.clone());
                    }
                }
                None
            })
            .collect();
        if treasury_payments.is_empty() {
            let err_msg = format!("No treasury payment found in this transaction",);
            warn!("{err_msg}");
            return Err(RelayError {
                status_code: StatusCode::BAD_REQUEST,
                message: err_msg,
            });
        }
    }

    // Check the sender's remaining gas allowance in Redis
    let end_user_account: &AccountId = &signed_delegate_action.delegate_action.sender_id;
    let remaining_allowance: u64 = config.allowances.get(end_user_account).await.unwrap_or(0);
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

    let actions = vec![Action::Delegate(signed_delegate_action)];
    let execution = config
        .rpc_client
        .send_tx(&config.signer, &receiver_id, actions)
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
        calculate_total_gas_burned(execution.transaction_outcome, execution.receipts_outcome);
    debug!("total gas burnt in yN: {}", gas_used_in_yn);
    let new_allowance = update_remaining_allowance(
        &config.allowances,
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

    match status {
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

pub async fn get_redis_cnxn(
    redis_pool: &Pool<RedisConnectionManager>,
) -> Result<PooledConnection<RedisConnectionManager>, RedisError> {
    let conn_result = redis_pool.get();
    let conn: PooledConnection<RedisConnectionManager> = match conn_result {
        Ok(conn) => conn,
        Err(e) => {
            let err_msg = format!("Error getting Relayer DB connection from the pool");
            error!("{err_msg}");
            return Err(RedisError::from((
                IoError,
                "Error getting Relayer DB connection from the pool",
                e.to_string(),
            )));
        }
    };
    Ok(conn)
}

pub async fn update_all_allowances_in_redis(
    config: &DConfig,
    allowance_in_gas: u64,
) -> Result<String, RelayError> {
    // Fetch all the keys associated with the network
    let keys = config.allowances.keys(&config.network).await.map_err(|e| {
        let err_msg = format!("Error fetching key allowances {}", e);
        error!("{err_msg}");
        RelayError {
            status_code: StatusCode::INTERNAL_SERVER_ERROR,
            message: err_msg,
        }
    })?;

    let num_keys = keys.len();

    // Iterate through the keys and update their values to the provided allowance in gas
    for key in keys {
        match config.allowances.set(key.clone(), allowance_in_gas).await {
            Ok(_) => info!("Updated allowance for key {}", key),
            Err(e) => {
                let err_msg = format!("Error updating allowance for key {}: {}", key, e);
                error!("{err_msg}");
                return Err(RelayError {
                    status_code: StatusCode::INTERNAL_SERVER_ERROR,
                    message: err_msg,
                });
            }
        }
    }

    // Return a success response
    let success_msg: String = format!("Updated {num_keys:?} keys in Relayer DB");
    info!("{success_msg}");
    Ok(success_msg)
}

/**
--------------------------- Testing below here ---------------------------
 */
#[cfg(test)]
fn create_signed_delegate_action(
    sender_id: AccountId,
    receiver_id: AccountId,
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
    // TODO make this a test config
    let config = Arc::new(DConfig::production().unwrap());

    // tests assume testnet in config
    // Test Transfer Action
    let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
    let sender_id = AccountId::from_str("relayer_test0.testnet").unwrap();
    let receiver_id = AccountId::from_str("relayer_test1.testnet").unwrap();
    let nonce: i32 = 1;
    let max_block_height = 2000000000;

    // simulate calling the '/update_allowance' function with sender_id & allowance
    let allowance_in_gas: u64 = u64::MAX;
    config
        .allowances
        .set(sender_id.clone(), allowance_in_gas)
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
    println!(
        "SignedDelegateAction Json Serialized (no borsh): {:?}",
        json_payload
    );
    let response: Response = send_meta_tx(State(config), json_payload)
        .await
        .into_response();
    let response_status: StatusCode = response.status();
    let body: BoxBody = response.into_body();
    let body_str: String = read_body_to_string(body).await.unwrap();
    println!("Response body: {body_str:?}");
    assert_eq!(response_status, StatusCode::OK);
}

#[tokio::test]
async fn test_send_meta_tx_no_gas_allowance() {
    let config = Arc::new(DConfig::production().unwrap());
    let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
    let sender_id = AccountId::from_str("relayer_test0.testnet").unwrap();
    let receiver_id = AccountId::from_str("arrr_me_not_in_whitelist").unwrap();
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
    println!(
        "SignedDelegateAction Json Serialized (no borsh) receiver_id not in whitelist: {:?}",
        non_whitelist_json_payload
    );
    let err_response = send_meta_tx(State(config), non_whitelist_json_payload)
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

    let config = Arc::new(DConfig::production().unwrap());
    let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
    let mut sender_id = AccountId::from_str("nomnomnom.testnet").unwrap();
    let mut receiver_id = AccountId::from_str("relayer_test0.testnet").unwrap();
    let max_block_height = 2000000000;

    let num_tests = 100;
    let mut response_statuses = vec![];
    let mut response_bodies = vec![];

    // fire off all post requests in rapid succession and save the response status codes
    for i in 0..num_tests {
        // Swap sender and reciever addresses each run
        mem::swap(&mut sender_id, &mut receiver_id);
        // Call the `relay` function happy path
        let signed_delegate_action = create_signed_delegate_action(
            sender_id.clone(),
            receiver_id.clone(),
            actions.clone(),
            // The plus 1 is to preserve the existing behaviour of this test
            i + 1,
            max_block_height.clone(),
        );
        let json_payload = signed_delegate_action.try_to_vec().unwrap();
        let response = relay(State(config.clone()), Json(Vec::from(json_payload)))
            .await
            .into_response();
        response_statuses.push(response.status());
        let body: BoxBody = response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        response_bodies.push(body_str);
    }

    // all responses should be successful
    for i in 0..response_statuses.len() {
        let response_status = response_statuses[i].clone();
        println!("{}", response_status);
        println!("{}", response_bodies[i]);
        assert_eq!(response_status, StatusCode::OK);
    }
}

#[test]
fn parse_configs() {
    let configs = [
        "config.toml",
        // "examples/configs/basic_whitelist.toml",
        "examples/configs/fastauth.toml",
        "examples/configs/pay_with_ft.toml",
        "examples/configs/redis.toml",
        "examples/configs/shared_storage.toml",
        "examples/configs/whitelist_senders.toml",
    ];

    for c in configs {
        println!("Parsing {c}");
        if let Err(e) = DConfig::validate_configuration(Path::new(c)) {
            println!("{}, {}", e, c)
        }
    }
}
