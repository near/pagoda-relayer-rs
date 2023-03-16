mod rpc_conf;
mod signing;

#[cfg(test)]
use axum::Json;
use axum::{extract, http::StatusCode, response::IntoResponse, Router, routing::post};
use config::{Config, File};
#[cfg(test)]
use near_crypto::{KeyType, Signature};
use near_crypto::PublicKey;
use near_jsonrpc_client::methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest;
#[cfg(test)]
use near_primitives::borsh::BorshSerialize;
use near_primitives::borsh::BorshDeserialize;
#[cfg(test)]
use near_primitives::delegate_action::{DelegateAction, NonDelegateAction};
use near_primitives::delegate_action::SignedDelegateAction;
#[cfg(test)]
use near_primitives::transaction::TransferAction;
use near_primitives::transaction::{Action, Transaction};
#[cfg(test)]
use near_primitives::types::{BlockHeight, Nonce};
use once_cell::sync::Lazy;
use serde_json::json;
use std::net::SocketAddr;
use std::str::FromStr;
use near_primitives::types::{BlockReference, Finality};
//use tower_http::{classify::ServerErrorsFailureClass, trace::TraceLayer};
use tracing::{debug, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use rpc_conf::rpc_transaction_error;
use signing::sign_transaction;
use crate::rpc_conf::{NetworkConfig, RPCConfig};


// load config from toml and setup json rpc client
static LOCAL_CONF: Lazy<Config> = Lazy::new(|| {
    let conf: Config = Config::builder()
        .add_source(File::with_name("config.toml"))
        .build()
        .unwrap();
    conf
});
static JSON_RPC_CLIENT: Lazy<near_jsonrpc_client::JsonRpcClient> = Lazy::new(|| {
    let network_name: String = LOCAL_CONF.get("network").unwrap();
    let rpc_config = RPCConfig::default();

    // optional overrides
    if LOCAL_CONF.get::<bool>("override_rpc_conf").unwrap() {
        let network_config = NetworkConfig {
            network_name,
            rpc_url: LOCAL_CONF.get("rpc_url").unwrap(),
            rpc_api_key: LOCAL_CONF.get("rpc_api_key").unwrap(),
            wallet_url: LOCAL_CONF.get("wallet_url").unwrap(),
            explorer_transaction_url: LOCAL_CONF.get("explorer_transaction_url").unwrap(),
        };
        network_config.json_rpc_client()
    } else {
        let network_config = rpc_config.networks.get(&network_name).unwrap();
        network_config.json_rpc_client()
    }

});
static IP_ADDRESS: Lazy<[u8; 4]> = Lazy::new(|| {
    let ip_address: [u8; 4] = LOCAL_CONF.get("addr").unwrap();
    ip_address
});
static PORT: Lazy<u16> = Lazy::new(|| {
    let port: u16 = LOCAL_CONF.get("port").unwrap();
    port
});
static RELAYER_ACCOUNT_ID: Lazy<String> = Lazy::new(|| {
    let relayer_account_id: String = LOCAL_CONF.get("relayer_account_id").unwrap();
    relayer_account_id
});
static KEYS_FILENAME: Lazy<String> = Lazy::new(|| {
    let keys_filename: String = LOCAL_CONF.get("keys_filename").unwrap();
    keys_filename
});
static RELAYER_PUBLIC_KEY: Lazy<String> = Lazy::new(|| {
    let relayer_public_key: String = LOCAL_CONF.get("relayer_public_key").unwrap();
    relayer_public_key
});

#[tokio::main]
async fn main() {
    // initialize tracing (aka logging)
    tracing_subscriber::registry().with(tracing_subscriber::fmt::layer()).init();

    // build our application with a route
    let app = Router::new()
        // `POST /relay` goes to `relay` handler function
        .route("/relay", post(relay));
        // See https://docs.rs/tower-http/0.1.1/tower_http/trace/index.html for more details.
        //.layer(TraceLayer::new_for_http()); // TODO LP: re-add when tower-http dependency conflict is resolved

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from((*IP_ADDRESS, *PORT));
    info!("listening on {}", addr);
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
            debug!("Deserialized SignedDelegateAction object: {:#?}", signed_delegate_action);

            // get the latest block hash
            let latest_final_block_hash = JSON_RPC_CLIENT
                .call(near_jsonrpc_client::methods::block::RpcBlockRequest{
                    block_reference: BlockReference::Finality(Finality::Final)
                }).await.unwrap().header.hash;

            // create Transaction from SignedDelegateAction
            let public_key = PublicKey::from_str(RELAYER_PUBLIC_KEY.as_str()).unwrap();
            // TODO is this the proper way to construct a nonce?
            let nonce = 369369369369369369_u64;
            // the receiver of the txn is the sender of the signed delegate action
            let receiver_id = signed_delegate_action.delegate_action.sender_id.clone();
            // TODO for batching add multiple signed delegate actions, then flush
            let actions = vec![Action::Delegate(signed_delegate_action)];
            let unsigned_transaction = Transaction{
                signer_id: RELAYER_ACCOUNT_ID.as_str().parse().unwrap(),
                public_key,
                nonce,
                receiver_id,
                block_hash: latest_final_block_hash,
                actions,
            };

            // sign transaction with locally stored key from json file
            let signed_transaction_result = sign_transaction(
                unsigned_transaction,
                KEYS_FILENAME.as_str(),
                JSON_RPC_CLIENT.clone(),
                ).await;
            match signed_transaction_result {
                Ok(_) => {
                    let signed_transaction = signed_transaction_result.unwrap().unwrap();

                    // send the SignedTransaction with retry logic
                    info!("Sending transaction ...");
                    let mut sleep_time_ms = 100;
                    let transaction_info = loop {
                        let transaction_info_result = JSON_RPC_CLIENT
                            .call(RpcBroadcastTxCommitRequest{signed_transaction: signed_transaction.clone()})
                            .await;
                        match transaction_info_result {
                            Ok(response) => {
                                break response;
                            }
                            Err(err) => match rpc_transaction_error(err) {
                                Ok(_) => {
                                    tokio::time::sleep(std::time::Duration::from_millis(sleep_time_ms)).await;
                                    sleep_time_ms *= 2;  // exponential backoff
                                }
                                Err(report) => {
                                    let err_msg = format!("{}: {:?}",
                                        "Error sending transaction to RPC",
                                        report.to_string()
                                    );
                                    info!("{}", err_msg);
                                    return (
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        err_msg,
                                    ).into_response()
                                },
                            },
                        };
                    };

                    // build response json
                    let success_msg_json = json!({
                        "message": "Successfully relayed and sent transaction.",
                        "status": transaction_info.status,
                        "Transaction Outcome Logs": transaction_info.transaction_outcome.outcome.logs.join("\n"),
                    });
                    info!("Success message: {:?}", success_msg_json);
                    let success_msg_str = serde_json::to_string(&success_msg_json).unwrap();
                    success_msg_str.into_response()
                }
                Err(_) => {
                    let err_msg = String::from("Error signing transaction");
                    info!("{}", err_msg);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        err_msg,
                    ).into_response()
                }
            }
        },
        Err(e) => {
            let err_msg = format!("{}: {:?}",
                "Error deserializing payload data object",
                e.to_string(),
            );
            info!("{}", err_msg);
            (
                StatusCode::BAD_REQUEST,
                err_msg,
            ).into_response()
        },
    }
}

/**
--------------------------- Testing below here ---------------------------
*/
#[cfg(test)]
fn create_signed_delegate_action(
    sender_id: String,
    receiver_id: String,
    actions: Vec<Action>,
    nonce: i32,
    max_block_height: i32
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

#[tokio::test]
async fn test_relay() {   // tests assume testnet in config
    // Test Transfer Action
    let actions = vec![
        Action::Transfer(TransferAction { deposit: 1 })
    ];
    let sender_id: String = String::from("relayer_test0.testnet");
    let receiver_id: String = String::from("relayer_test1.testnet");
    let nonce: i32 = 1;
    let max_block_height = 2000000000;

    // Call the `relay` function happy path
    let signed_delegate_action = create_signed_delegate_action(
        sender_id.clone(),
        receiver_id.clone(),
        actions.clone(),
        nonce,
        max_block_height,
    );
    let json_payload = signed_delegate_action.try_to_vec().unwrap();
    println!("SignedDelegateAction Json Serialized: {:?}", json_payload);
    let response = relay(Json(Vec::from(json_payload))).await.into_response();
    let response_status = response.status();
    assert_eq!(response_status, StatusCode::OK);

    // Call the `relay` function with a payload that can't be deserialized into a SignedDelegateAction
    let bad_json_payload = serde_json::to_string("arrrgh").unwrap();
    println!("Malformed Json Payload Serialized: {:?}", bad_json_payload);
    let err_response = relay(Json(Vec::from(bad_json_payload))).await.into_response();
    let err_response_status = err_response.status();
    assert_eq!(err_response_status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore]
async fn test_relay_with_load() {   // tests assume testnet in config
    // Test Transfer Action

    let actions = vec![
        Action::Transfer(TransferAction { deposit: 1 })
    ];
    let account_id0: String = "relayer_test0.testnet".to_string();
    let account_id1: String = "relayer_test1.testnet".to_string();
    let mut sender_id: String = String::new();
    let mut receiver_id: String = String::new();
    let mut nonce: i32 = 1;
    let max_block_height = 2000000000;

    let num_tests = 100;
    let mut response_statuses = vec![];

    // fire off all post requests in rapid succession and save the response status codes
    for i in 0..num_tests {
        if i % 2 == 0 {
            sender_id.push_str(&*account_id0.clone());
            receiver_id.push_str(&*account_id1.clone());
        } else {
            sender_id.push_str(&*account_id1.clone());
            receiver_id.push_str(&*account_id0.clone());
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
        let response = relay(Json(Vec::from(json_payload))).await.into_response();
        response_statuses.push(response.status());

        // increment nonce & reset sender, receiver strs
        nonce += 1;
        sender_id.clear();
        receiver_id.clear();
    }

    // all responses should be successful
    for i in 0..response_statuses.len() {
        assert_eq!(response_statuses[i], StatusCode::OK);
    }
}
