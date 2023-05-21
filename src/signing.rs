use serde::{Deserialize, Serialize};

use near_crypto::{PublicKey, SecretKey};
use near_jsonrpc_client::JsonRpcClient;
use near_jsonrpc_primitives::types;
use near_primitives::transaction::SignedTransaction;
use near_primitives::types::AccountId;
use near_primitives::views::QueryRequest;

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
    Ok(serde_json::from_reader(std::fs::File::open(filename)?)?)
}

pub fn get_signing_keys(filename: &str) -> Result<SigningKeys, Box<dyn std::error::Error>> {
    let key_file = read_key_file(filename)?;
    let account_id = key_file.account_id.parse()?;
    let signer_public_key = key_file.public_key.parse()?;
    let signer_private_key = key_file.private_key.parse()?;
    Ok(SigningKeys {
        account_id,
        signer_public_key,
        signer_private_key,
    })
}

pub async fn sign_transaction(
    prepopulated_unsigned_transaction: near_primitives::transaction::Transaction,
    filename: &str,
    json_rpc_client: &JsonRpcClient,
) -> color_eyre::eyre::Result<SignedTransaction> {
    let signing_keys = get_signing_keys(filename).unwrap();
    let signer_secret_key: SecretKey = signing_keys.signer_private_key.clone();
    let online_signer_access_key_response = json_rpc_client
        .call(near_jsonrpc_client::methods::query::RpcQueryRequest {
            block_reference: near_primitives::types::Finality::Final.into(),
            request: QueryRequest::ViewAccessKey {
                account_id: signing_keys.account_id,
                public_key: signing_keys.signer_public_key.clone(),
            },
        })
        .await
        .map_err(|err| {
            println!("\nYour transaction was not successfully signed.\n");
            color_eyre::eyre::eyre!(
                "Failed to fetch public key information for nonce: {:?}",
                err
            )
        })?;
    let current_nonce =
        if let types::query::QueryResponseKind::AccessKey(online_signer_access_key) =
            online_signer_access_key_response.kind
        {
            online_signer_access_key.nonce
        } else {
            return Err(color_eyre::eyre::eyre!("Error current_nonce"));
        };
    let unsigned_transaction = near_primitives::transaction::Transaction {
        public_key: signing_keys.signer_public_key,
        block_hash: online_signer_access_key_response.block_hash,
        nonce: current_nonce + 1,
        ..prepopulated_unsigned_transaction
    };
    let signature = signer_secret_key.sign(unsigned_transaction.get_hash_and_size().0.as_ref());
    let signed_transaction = SignedTransaction::new(signature, unsigned_transaction);
    println!("\nYour transaction was signed successfully.");
    Ok(signed_transaction)
}
