mod conf;
mod common;

use axum::{extract, response::IntoResponse, Router, routing::post};
use config::{Config, File};
use mockall::{mock};
use mockers::Mock;
use near_crypto::{KeyType, PublicKey, Signature};
use near_jsonrpc_client::methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest;
use near_primitives::borsh::{BorshDeserialize, BorshSerialize};
use near_primitives::delegate_action::{DelegateAction, NonDelegateAction, SignedDelegateAction};
use near_primitives::transaction::{Action, CreateAccountAction, SignedTransaction, Transaction, TransferAction};
use near_primitives::types::{AccountId, BlockHeight, Nonce};
use near_primitives::views::{ActionView, ExecutionOutcomeView, ExecutionOutcomeWithIdView, ExecutionStatusView, SignedTransactionView};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::str::FromStr;
use crate::common::rpc_transaction_error;
use crate::conf::RPCConfig;


// load network config from yaml and setup json rpc client
// TODO add RPC api key to config file and JsonRpcClient
static JSON_RPC_CLIENT: Lazy<near_jsonrpc_client::JsonRpcClient> = Lazy::new(|| {
    let conf: Config = Config::builder()
        .add_source(File::with_name("config.toml"))
        .build()
        .unwrap();
    let network_name: String = conf.get("network").unwrap();
    let rpc_config = RPCConfig::default();
    let network_config = rpc_config.networks.get(&network_name).unwrap();
    let json_rpc_client = network_config.json_rpc_client();
    json_rpc_client
});


#[tokio::main]
async fn main() {
    // initialize tracing
    tracing_subscriber::fmt::init();

    // build our application with a route
    let app = Router::new()
        // `POST /relay` goes to `relay` handler function
        .route("/relay", post(relay));

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from(([127, 0, 0, 1], 3030));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn relay(
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
            let transaction = Transaction{
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
                let transaction_info_result = JSON_RPC_CLIENT
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
                        Err(report) => return report.to_string().into_response(),
                    },
                };
            };

            // build response json
            let mut success_msg_json: Map<String, Value> = Map::new();
            success_msg_json.insert("message".to_string(),
                                    json!("Successfully relayed and sent transaction."));
            success_msg_json.insert("Status".to_string(),
                                    json!(transaction_info.status));
            success_msg_json.insert("Transaction Outcome Logs".to_string(),
                                    json!(transaction_info.transaction_outcome.outcome.logs.join("\n")));

            let success_msg_str = serde_json::to_string(&success_msg_json).unwrap();
            success_msg_str.into_response()
        },
        Err(e) => {
            let err_msg = String::from(
                format!("{}: {:?}",
                        "Error deserializing payload data object".to_string(),
                        e.to_string()
                )
            );
            println!("{}", err_msg);
            err_msg.into_response()
        },
    }

}

#[tokio::test]
async fn test_relay() {
    // Test Transfer Action and a CreateAccount Action
    const SENDER_ID: String = "aaa".parse().unwrap();
    const RECEIVER_ID: String = "bbb".parse().unwrap();
    const NONCE: i32 = 1;
    const MAX_BLOCK_HEIGHT: i32 = 2;
    const PUBLIC_KEY: PublicKey = PublicKey::empty(KeyType::ED25519);
    const SIGNATURE: Signature = Signature::empty(KeyType::ED25519);

    fn create_signed_delegate_action(actions: Vec<Action>) -> SignedDelegateAction {
        SignedDelegateAction {
            delegate_action: DelegateAction {
                sender_id: SENDER_ID.parse().unwrap(),
                receiver_id: RECEIVER_ID.parse().unwrap(),
                actions: actions
                    .iter()
                    .map(|a| NonDelegateAction::try_from(a.clone()).unwrap())
                    .collect(),
                nonce: NONCE as Nonce,
                max_block_height: MAX_BLOCK_HEIGHT as BlockHeight,
                public_key: PUBLIC_KEY,
            },
            signature: SIGNATURE,
        }
    }

    let relayer_account_id = AccountId::from_str(&"relayer.near".to_string());
    let actions = vec![
        Action::CreateAccount(CreateAccountAction {}),
        Action::Transfer(TransferAction { deposit: 100 })
    ];
    let signed_delegate_action = create_signed_delegate_action(actions.clone());
    let serialized_sda = signed_delegate_action.try_to_vec().unwrap();
    let json_payload = serde_json::to_string(&serialized_sda).unwrap();

    // Create a mock response for the JSON RPC call
    let transaction_info = near_primitives::views::FinalExecutionOutcomeView{
        status: Default::default(),
        transaction: SignedTransactionView {
            signer_id: SENDER_ID.parse().unwrap(),
            public_key: PUBLIC_KEY,
            nonce: NONCE as Nonce,
            receiver_id: RECEIVER_ID.parse().unwrap(),
            actions: actions.iter().map(Action::view).collect(),
            signature: SIGNATURE,
            hash: Default::default()
        },
        transaction_outcome: ExecutionOutcomeWithIdView {
            proof: vec![],
            block_hash: Default::default(),
            id: Default::default(),
            outcome: ExecutionOutcomeView {
                logs: vec![],
                receipt_ids: vec![],
                gas_burnt: 0,
                tokens_burnt: 0,
                executor_id: relayer_account_id,
                status: ExecutionStatusView::Unknown,
                metadata: Default::default()
            }
        },
        receipts_outcome: vec![]
    };

    let json_response = serde_json::to_string(&transaction_info).unwrap();
    let mock_response = near_jsonrpc_client::types::JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: 0,
        result: Some(json!(json_response)),
        error: None,
    };

    // Create a mock JSON RPC client that returns the mock response
    let json_rpc_client = near_jsonrpc_client::JsonRpcClient::connect("http://example.com");
    let call_mock = Mock::new().returns(Ok(mock_response.clone()));
    let call_mock_clone = call_mock.clone();
    mock(json_rpc_client).with(move |mock| {
        mock.expect_call()
            .times(1)
            .return_once(move |_| call_mock_clone.call(()))
    });

    // Call the `relay` function with the mock payload and JSON RPC client
    let response = relay(extract::Json(Vec::from(json_payload))).await.into_response();
    let response_body = response.into_body().into_string().await.unwrap();

    // Verify that the response body contains the expected transaction outcome logs
    assert!(response_body.contains("test log message"));
}
