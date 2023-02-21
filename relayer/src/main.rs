mod config;
mod common;

use axum::{response::IntoResponse, routing::post, Router, extract};
use std::net::SocketAddr;
use near_jsonrpc_client::methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest;
use near_primitives::borsh::BorshDeserialize;
use near_primitives::delegate_action::{NonDelegateAction, SignedDelegateAction};
use near_primitives::transaction::{Action, SignedTransaction};
use crate::common::rpc_transaction_error;

// TODO read from local config instead of hardcoding
static NETWORK_CONFIG: config::NetworkConfig = config::NetworkConfig {
    network_name: "testnet".to_string(),
    rpc_url: "https://archival-rpc.testnet.near.org".parse().unwrap(),
    wallet_url: "https://wallet.testnet.near.org".parse().unwrap(),
    explorer_transaction_url: "https://explorer.testnet.near.org/transactions/"
        .parse()
        .unwrap(),
    rpc_api_key: None,
};

#[tokio::main]
async fn main() {
    // initialize tracing
    tracing_subscriber::fmt::init();

    // build our application with a route
    let app = Router::new()
        // `POST /relay` goes to `create_relay`
        .route("/relay", post(create_relay));

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from(([127, 0, 0, 1], 3030));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn create_relay(
    data: extract::Json<Vec<u8>>,
) -> impl IntoResponse {
    // deserialize SignedDelegateAction using borsh
    match SignedDelegateAction::try_from_slice(&data.0) {
        Ok(signed_delegate_action) => {
            println!("Deserialized SignedDelegateAction object: {:#?}", signed_delegate_action);

            // filter out Transfer Action types (FT transfers or NFT OK)
            let filtered_actions: Vec<NonDelegateAction> = signed_delegate_action.delegate_action.actions
                .into_iter()
                .map(|nda| Action::try_from(nda.clone()).unwrap())
                .filter(|action| {
                    if let Action::Transfer(_) = action {
                        false // exclude Transfer Action types
                    } else {
                        true // include all other Action variants
                    }
                })
                .map(|a| NonDelegateAction::try_from(a.clone()).unwrap())
                .collect();

            // create Transaction, SignedTransaction from SignedDelegateAction
            let transaction = near_primitives::transaction::Transaction{
                signer_id: signed_delegate_action.delegate_action.sender_id,
                public_key: signed_delegate_action.delegate_action.public_key,
                nonce: signed_delegate_action.delegate_action.nonce,
                receiver_id: signed_delegate_action.delegate_action.receiver_id,
                block_hash: Default::default(),
                actions: filtered_actions
                    .into_iter()
                    .map(|a| Action::try_from(a.clone()).unwrap())
                    .collect()
            };
            let signed_transaction = SignedTransaction::new(
                signed_delegate_action.signature,
                transaction
            );

            // create json_rpc_client, send the SignedTransaction
            println!("Sending transaction ...");
            let transaction_info = loop {
                let transaction_info_result = NETWORK_CONFIG.json_rpc_client()
                    // TODO error[E0308]: mismatched types expected `SignedTransaction`, found a different `SignedTransaction`
                    // 0.15.0 is still around and keeps getting added to cargo.lock due to near-jsonrpc-client dependency
                    .call(RpcBroadcastTxCommitRequest{signed_transaction: signed_transaction.clone()})
                    .await;
                match transaction_info_result {
                    Ok(response) => {
                        break response;
                    }
                    Err(err) => match rpc_transaction_error(err) {
                        Ok(_) => {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await
                        }
                        Err(report) => return Err(report).expect("REASON").into_response(),
                    },
                };
            };

            "Successfully relayed and sent transaction".into_response()
        },
        Err(e) => {
            println!("Error deserializing payload data object: {:?}", e);
            "Error deserializing payload data object".into_response()
        },
    }

}
