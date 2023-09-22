use near_crypto::{KeyType, PublicKey};
use near_jsonrpc_client::methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest;
use near_jsonrpc_client::methods::query::RpcQueryRequest;
use near_jsonrpc_client::JsonRpcClient;
use near_jsonrpc_primitives::types::query::QueryResponseKind;
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{Action, Transaction};
use near_primitives::types::{AccountId, BlockReference};
use near_primitives::views::{FinalExecutionOutcomeView, QueryRequest};
use serde::de::DeserializeOwned;
use tracing::debug;

use crate::rpc_conf::rpc_transaction_error;
use crate::signing::sign_transaction;

pub async fn send_tx(
    rpc_client: &JsonRpcClient,
    keys_filename: &str,
    signer_id: &AccountId,
    receiver_id: &AccountId,
    actions: Vec<Action>,
    method_name: &str,
) -> anyhow::Result<FinalExecutionOutcomeView> {
    let unsigned_tx = Transaction {
        public_key: PublicKey::empty(KeyType::ED25519), // gets replaced when signing txn
        block_hash: CryptoHash::default(),              // gets replaced when signing txn
        nonce: 0,                                       // gets replaced when signing txn
        actions,
        signer_id: signer_id.clone(),
        receiver_id: receiver_id.clone(),
    };

    let mut sleep_time_ms = 100;
    let mut counter = 5;
    loop {
        let signed_tx = sign_transaction(unsigned_tx.clone(), keys_filename, rpc_client)
            .await
            .map_err(|err| {
                anyhow::anyhow!("Failed to sign transaction for {method_name}: {}", err)
            })?;

        let tx_result = rpc_client
            .call(RpcBroadcastTxCommitRequest {
                signed_transaction: signed_tx.clone(),
            })
            .await;
        match tx_result {
            Ok(execution) => {
                debug!("Successfully processed {method_name} tx");
                debug!("Transaction outcome: {:?}", execution.transaction_outcome);
                debug!("Receipts outcome: {:?}", execution.receipts_outcome);
                break Ok(execution);
            }
            Err(err) => match rpc_transaction_error(err) {
                Ok(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(sleep_time_ms)).await;
                    sleep_time_ms *= 2; // exponential backoff

                    counter -= 1;
                    if counter <= 0 {
                        anyhow::bail!("Failed to process {method_name} tx. Exceeded retry count!");
                    }
                }
                Err(report) => {
                    anyhow::bail!("Error sending transaction to RPC: {report:?}");
                }
            },
        };
    }
}

pub async fn view<T: DeserializeOwned>(
    rpc_client: &JsonRpcClient,
    receiver_id: &AccountId,
    method_name: &str,
    args: serde_json::Value,
) -> anyhow::Result<T> {
    let resp = rpc_client
        .call(RpcQueryRequest {
            block_reference: BlockReference::latest(),
            request: QueryRequest::CallFunction {
                account_id: receiver_id.clone(),
                method_name: method_name.into(),
                args: args.to_string().into_bytes().into(),
            },
        })
        .await?;

    let resp = match resp.kind {
        QueryResponseKind::CallResult(result) => result,
        _ => Err(anyhow::anyhow!("View function returned invalid data"))?,
    };

    Ok(serde_json::from_slice(&resp.result)?)
}
