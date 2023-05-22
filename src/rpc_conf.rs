/*
For ApiKey data structure
*/

#[derive(Eq, Hash, Clone, Debug, PartialEq)]
pub struct ApiKey(pub near_jsonrpc_client::auth::ApiKey);

impl From<ApiKey> for near_jsonrpc_client::auth::ApiKey {
    fn from(api_key: ApiKey) -> Self {
        api_key.0
    }
}

impl std::fmt::Display for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::str::FromStr for ApiKey {
    type Err = color_eyre::Report;

    fn from_str(api_key: &str) -> Result<Self, Self::Err> {
        Ok(Self(near_jsonrpc_client::auth::ApiKey::new(api_key)?))
    }
}

impl serde::ser::Serialize for ApiKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        serializer.serialize_str(self.0.to_str().unwrap())
    }
}

impl<'de> serde::de::Deserialize<'de> for ApiKey {
    fn deserialize<D>(deserializer: D) -> Result<ApiKey, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(|err: color_eyre::Report| serde::de::Error::custom(err.to_string()))
    }
}

/*
For RPCConfig data structure for creating the RPC connection
*/

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RPCConfig {
    pub credentials_home_dir: std::path::PathBuf,
    pub networks: linked_hash_map::LinkedHashMap<String, NetworkConfig>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NetworkConfig {
    pub network_name: String,
    pub rpc_url: url::Url,
    pub rpc_api_key: Option<ApiKey>,
    pub wallet_url: url::Url,
    pub explorer_transaction_url: url::Url,
}

impl Default for RPCConfig {
    fn default() -> Self {
        let home_dir = dirs::home_dir().expect("Impossible to get your home dir!");
        let mut credentials_home_dir = std::path::PathBuf::from(&home_dir);
        credentials_home_dir.push(".near-credentials");

        let mut networks = linked_hash_map::LinkedHashMap::new();
        networks.insert(
            "mainnet".to_string(),
            NetworkConfig {
                network_name: "mainnet".to_string(),
                rpc_url: "https://archival-rpc.mainnet.near.org".parse().unwrap(),
                wallet_url: "https://wallet.mainnet.near.org".parse().unwrap(),
                explorer_transaction_url: "https://explorer.mainnet.near.org/transactions/"
                    .parse()
                    .unwrap(),
                rpc_api_key: None,
            },
        );
        networks.insert(
            "testnet".to_string(),
            NetworkConfig {
                network_name: "testnet".to_string(),
                rpc_url: "https://archival-rpc.testnet.near.org".parse().unwrap(),
                wallet_url: "https://wallet.testnet.near.org".parse().unwrap(),
                explorer_transaction_url: "https://explorer.testnet.near.org/transactions/"
                    .parse()
                    .unwrap(),
                rpc_api_key: None,
            },
        );
        networks.insert(
            "betanet".to_string(),
            NetworkConfig {
                network_name: "betanet".to_string(),
                rpc_url: "https://rpc.betanet.near.org".parse().unwrap(),
                wallet_url: "https://wallet.betanet.near.org".parse().unwrap(),
                explorer_transaction_url: "https://explorer.betanet.near.org/transactions/"
                    .parse()
                    .unwrap(),
                rpc_api_key: None,
            },
        );
        networks.insert(
            "shardnet".to_string(),
            NetworkConfig {
                network_name: "shardnet".to_string(),
                rpc_url: "https://rpc.shardnet.near.org".parse().unwrap(),
                wallet_url: "https://wallet.shardnet.near.org".parse().unwrap(),
                explorer_transaction_url: "https://explorer.shardnet.near.org/transactions/"
                    .parse()
                    .unwrap(),
                rpc_api_key: None,
            },
        );
        Self {
            credentials_home_dir,
            networks,
        }
    }
}

impl NetworkConfig {
    pub fn json_rpc_client(&self) -> near_jsonrpc_client::JsonRpcClient {
        let mut json_rpc_client =
            near_jsonrpc_client::JsonRpcClient::connect(self.rpc_url.as_ref());
        if let Some(rpc_api_key) = &self.rpc_api_key {
            json_rpc_client =
                json_rpc_client.header(near_jsonrpc_client::auth::ApiKey::from(rpc_api_key.clone()))
        };
        json_rpc_client
    }
}

pub fn rpc_transaction_error(
    err: near_jsonrpc_client::errors::JsonRpcError<
        near_jsonrpc_client::methods::broadcast_tx_commit::RpcTransactionError,
    >,
) -> Result<(), color_eyre::Report> {
    match &err {
        near_jsonrpc_client::errors::JsonRpcError::TransportError(_rpc_transport_error) => {
            println!("Transport error transaction.\nPlease wait. The next try to send this transaction is happening right now ...");
        }
        near_jsonrpc_client::errors::JsonRpcError::ServerError(rpc_server_error) => match rpc_server_error {
            near_jsonrpc_client::errors::JsonRpcServerError::HandlerError(rpc_transaction_error) => match rpc_transaction_error {
                near_jsonrpc_client::methods::broadcast_tx_commit::RpcTransactionError::TimeoutError => {
                    println!("Timeout error transaction.\nPlease wait. The next try to send this transaction is happening right now ...");
                }
                near_jsonrpc_client::methods::broadcast_tx_commit::RpcTransactionError::InvalidTransaction { context } => {
                    return Err(color_eyre::eyre::eyre!("Invalid Transaction Error. Context: {}", context.to_string()));
                }
                near_jsonrpc_client::methods::broadcast_tx_commit::RpcTransactionError::DoesNotTrackShard => {
                    return Err(color_eyre::eyre::eyre!("RPC Server Error"));
                }
                near_jsonrpc_client::methods::broadcast_tx_commit::RpcTransactionError::RequestRouted{transaction_hash} => {
                    return Err(color_eyre::eyre::eyre!("RPC Server Error for transaction with hash {}\n", transaction_hash));
                }
                near_jsonrpc_client::methods::broadcast_tx_commit::RpcTransactionError::UnknownTransaction{requested_transaction_hash} => {
                    return Err(color_eyre::eyre::eyre!("RPC Server Error for transaction with hash {}\n", requested_transaction_hash));
                }
                near_jsonrpc_client::methods::broadcast_tx_commit::RpcTransactionError::InternalError{debug_info} => {
                    return Err(color_eyre::eyre::eyre!("RPC Server Error: {}", debug_info));
                }
            }
            near_jsonrpc_client::errors::JsonRpcServerError::RequestValidationError(rpc_request_validation_error) => {
                return Err(color_eyre::eyre::eyre!("Incompatible request with the server: {:#?}",  rpc_request_validation_error));
            }
            near_jsonrpc_client::errors::JsonRpcServerError::InternalError{ info } => {
                println!("Internal server error: {}.\nPlease wait. The next try to send this transaction is happening right now ...", info.clone().unwrap_or_default());
            }
            near_jsonrpc_client::errors::JsonRpcServerError::NonContextualError(rpc_error) => {
                return Err(color_eyre::eyre::eyre!("Unexpected response: {}", rpc_error));
            }
            near_jsonrpc_client::errors::JsonRpcServerError::ResponseStatusError(json_rpc_server_response_status_error) => match json_rpc_server_response_status_error {
                near_jsonrpc_client::errors::JsonRpcServerResponseStatusError::Unauthorized => {
                    return Err(color_eyre::eyre::eyre!("JSON RPC server requires authentication. Please, authenticate near CLI with the JSON RPC server you use."));
                }
                near_jsonrpc_client::errors::JsonRpcServerResponseStatusError::TooManyRequests => {
                    println!("JSON RPC server is currently busy.\nPlease wait. The next try to send this transaction is happening right now ...");
                }
                near_jsonrpc_client::errors::JsonRpcServerResponseStatusError::Unexpected{status} => {
                    return Err(color_eyre::eyre::eyre!("JSON RPC server responded with an unexpected status code: {}", status));
                }
            }
        }
    }
    Ok(())
}
