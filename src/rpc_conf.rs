/*
For ApiKey data structure
*/

use std::fmt::Debug;

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
    pub rpc_url: url::Url,
    pub rpc_api_key: Option<ApiKey>,
}

impl NetworkConfig {
    pub fn rpc_client(&self) -> near_fetch::Client {
        near_fetch::Client::from_client(self.raw_rpc_client())
    }

    pub fn raw_rpc_client(&self) -> near_jsonrpc_client::JsonRpcClient {
        let mut json_rpc_client =
            near_jsonrpc_client::JsonRpcClient::connect(self.rpc_url.as_ref());
        if let Some(rpc_api_key) = &self.rpc_api_key {
            json_rpc_client = json_rpc_client.header(rpc_api_key.0.clone());
        };
        json_rpc_client
    }
}
