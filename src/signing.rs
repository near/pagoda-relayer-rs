use std::str::FromStr;
use near_jsonrpc_client::JsonRpcClient;
use near_crypto::{PublicKey, SecretKey};
use near_primitives::transaction::SignedTransaction;
use near_primitives::types::AccountId;
use serde::{Deserialize, Serialize};


#[derive(Debug, Clone)]
pub struct SigningKeys {
    pub account_id: AccountId,
    pub signer_public_key: PublicKey,
    pub signer_private_key: SecretKey,
}

#[derive(Debug, Serialize, Deserialize)]
struct KeyFile {
    account_id: String,
    public_key: String,
    private_key: String,
}

fn read_key_file(filename: &str) -> Result<KeyFile, Box<dyn std::error::Error>> {
    let file_contents = std::fs::read_to_string(filename)?;
    let key_file = serde_json::from_str(&file_contents)?;
    Ok(key_file)
}

pub fn get_signing_keys(filename: &str) -> Result<SigningKeys, Box<dyn std::error::Error>> {
    let key_file = read_key_file(filename)?;
    let account_id = AccountId::from_str(&key_file.account_id).unwrap();
    let signer_public_key = PublicKey::from_str(&key_file.public_key)?;
    let signer_private_key = SecretKey::from_str(&key_file.private_key)?;
    Ok(SigningKeys {
        account_id,
        signer_public_key,
        signer_private_key,
    })
}

pub async fn sign_transaction(
    prepopulated_unsigned_transaction: near_primitives::transaction::Transaction,
    filename: &str,
    json_rpc_client: JsonRpcClient,
) -> color_eyre::eyre::Result<Option<SignedTransaction>> {
    let signing_keys = get_signing_keys(filename).unwrap();
    let signer_secret_key: SecretKey = signing_keys.signer_private_key.clone().into();
    let online_signer_access_key_response = json_rpc_client
        .call(near_jsonrpc_client::methods::query::RpcQueryRequest {
            block_reference: near_primitives::types::Finality::Final.into(),
            request: near_primitives::views::QueryRequest::ViewAccessKey {
                account_id: signing_keys.account_id.clone(),
                public_key: signing_keys.signer_public_key.clone().into(),
            },
        })
        .await
        .map_err(|err| {
            println!("\nYour transaction was not successfully signed.\n");
            color_eyre::Report::msg(format!(
                "Failed to fetch public key information for nonce: {:?}",
                err
            ))
        })?;
    let current_nonce =
        if let near_jsonrpc_primitives::types::query::QueryResponseKind::AccessKey(
            online_signer_access_key,
        ) = online_signer_access_key_response.kind
        {
            online_signer_access_key.nonce
        } else {
            return Err(color_eyre::Report::msg("Error current_nonce".to_string()));
        };
    let unsigned_transaction = near_primitives::transaction::Transaction {
        public_key: signing_keys.signer_public_key.clone().into(),
        block_hash: online_signer_access_key_response.block_hash,
        nonce: current_nonce + 1,
        ..prepopulated_unsigned_transaction
    };
    let signature = signer_secret_key.sign(unsigned_transaction.get_hash_and_size().0.as_ref());
    let signed_transaction = SignedTransaction::new(signature, unsigned_transaction);
    println!("\nYour transaction was signed successfully.");
    Ok(Option::from(signed_transaction))
}
