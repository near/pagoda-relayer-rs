use axum::{http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use config::{Config, File};
use once_cell::sync::Lazy;
use rpc_conf::rpc_transaction_error;
use serde_json::json;
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;
use tracing::{debug, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(test)]
use near_crypto::Signature;
use near_crypto::{KeyType, PublicKey};
use near_jsonrpc_client::methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest;
use near_primitives::delegate_action::SignedDelegateAction;
#[cfg(test)]
use near_primitives::delegate_action::{DelegateAction, NonDelegateAction};
use near_primitives::hash::CryptoHash;
#[cfg(test)]
use near_primitives::transaction::TransferAction;
use near_primitives::transaction::{Action, Transaction};
use near_primitives::types::AccountId;
#[cfg(test)]
use near_primitives::types::{BlockHeight, Nonce};

mod rpc_conf;
mod schemas;
mod signing;

// load config from toml and setup json rpc client
static LOCAL_CONF: Lazy<Config> = Lazy::new(|| {
    Config::builder()
        .add_source(File::with_name("config.toml"))
        .build()
        .unwrap()
});
static JSON_RPC_CLIENT: Lazy<near_jsonrpc_client::JsonRpcClient> = Lazy::new(|| {
    let network_name: String = LOCAL_CONF.get("network").unwrap();
    let rpc_config = crate::rpc_conf::RPCConfig::default();

    // optional overrides
    if LOCAL_CONF.get::<bool>("override_rpc_conf").unwrap() {
        let network_config = crate::rpc_conf::NetworkConfig {
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
static IP_ADDRESS: Lazy<[u8; 4]> = Lazy::new(|| LOCAL_CONF.get("ip_address").unwrap());
static PORT: Lazy<u16> = Lazy::new(|| LOCAL_CONF.get("port").unwrap());
static RELAYER_ACCOUNT_ID: Lazy<AccountId> =
    Lazy::new(|| LOCAL_CONF.get("relayer_account_id").unwrap());
static KEYS_FILENAME: Lazy<String> = Lazy::new(|| LOCAL_CONF.get("keys_filename").unwrap());

#[tokio::main]
async fn main() {
    // initialize tracing (aka logging)
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .init();

    // build our application with a route
    let app = Router::new()
        // `POST /relay` goes to `relay` handler function
        .route("/relay", post(relay))
        // See https://docs.rs/tower-http/0.1.1/tower_http/trace/index.html for more details.
        .layer(TraceLayer::new_for_http());

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from((*IP_ADDRESS, *PORT));
    info!("listening on {addr}");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn relay(Json(data): Json<crate::schemas::RelayInputArgs>) -> impl IntoResponse {
    let signed_delegate_action: SignedDelegateAction = data.signed_delegate_action.into();
    debug!("Deserialized SignedDelegateAction object: {signed_delegate_action:#?}");

    let unsigned_transaction = Transaction {
        // create Transaction from SignedDelegateAction
        signer_id: RELAYER_ACCOUNT_ID.clone(),
        // the receiver of the txn is the sender of the signed delegate action
        receiver_id: signed_delegate_action.delegate_action.sender_id.clone(),
        // gets replaced when signing txn
        public_key: PublicKey::empty(KeyType::ED25519),
        // gets replaced when signing txn
        nonce: 0,
        // gets replaced when signing txn
        block_hash: CryptoHash::default(),
        // TODO for batching add multiple signed delegate actions, then flush
        actions: vec![Action::Delegate(signed_delegate_action)],
    };
    debug!("unsigned_transaction {unsigned_transaction:?}");

    // sign transaction with locally stored key from json file
    let signed_transaction_result =
        crate::signing::sign_transaction(unsigned_transaction, &KEYS_FILENAME, &JSON_RPC_CLIENT)
            .await;
    match signed_transaction_result {
        Ok(signed_transaction) => {
            // send the SignedTransaction with retry logic
            info!("Sending transaction ...");
            let mut sleep_time_ms = 100;
            let transaction_info = loop {
                let transaction_info_result = JSON_RPC_CLIENT
                    .call(RpcBroadcastTxCommitRequest {
                        signed_transaction: signed_transaction.clone(),
                    })
                    .await;
                match transaction_info_result {
                    Ok(response) => {
                        break response;
                    }
                    Err(err) => match rpc_transaction_error(err) {
                        Ok(_) => {
                            tokio::time::sleep(std::time::Duration::from_millis(sleep_time_ms))
                                .await;
                            sleep_time_ms *= 2; // exponential backoff
                        }
                        Err(report) => {
                            let err_msg = format!("Error sending transaction to RPC: {report}");
                            info!("{err_msg}");
                            return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
                        }
                    },
                };
            };

            // build response json
            let success_msg_json = json!({
                "message": "Successfully relayed and sent transaction.",
                "status": transaction_info.status,
                "transaction_hash": transaction_info.transaction.hash,
                "transaction_outcome_logs": transaction_info.transaction_outcome.outcome.logs.join("\n"),
            });
            info!("Success message: {success_msg_json:?}");
            serde_json::to_string(&success_msg_json)
                .unwrap()
                .into_response()
        }
        Err(_) => {
            let err_msg = String::from("Error signing transaction");
            info!("{err_msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response()
        }
    }
}

/**
--------------------------- Testing below here ---------------------------
*/
#[cfg(test)]
fn create_signed_delegate_action(
    sender_id: near_primitives::types::AccountId,
    receiver_id: near_primitives::types::AccountId,
    actions: Vec<Action>,
    nonce: Nonce,
    max_block_height: BlockHeight,
) -> SignedDelegateAction {
    SignedDelegateAction {
        delegate_action: DelegateAction {
            sender_id,
            receiver_id,
            actions: actions
                .into_iter()
                .map(|a| NonDelegateAction::try_from(a).unwrap())
                .collect(),
            nonce,
            max_block_height,
            public_key: PublicKey::empty(KeyType::ED25519),
        },
        signature: Signature::empty(KeyType::ED25519),
    }
}

#[tokio::test]
async fn test_relay() {
    // tests assume testnet in config
    // Test Transfer Action
    let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
    let sender_id: AccountId = "relayer_test0.testnet".parse().unwrap();
    let receiver_id: AccountId = "relayer_test1.testnet".parse().unwrap();
    let nonce = 1;
    let max_block_height = 2000000000;

    // Call the `relay` function happy path
    let signed_delegate_action =
        create_signed_delegate_action(sender_id, receiver_id, actions, nonce, max_block_height);
    let json_payload = crate::schemas::RelayInputArgs {
        signed_delegate_action: signed_delegate_action.into(),
    };
    println!("SignedDelegateAction Json Serialized: {json_payload:?}");
    let response = relay(Json(json_payload)).await.into_response();
    let response_status = response.status();
    assert_eq!(response_status, StatusCode::OK);
}

#[tokio::test]
#[ignore]
async fn test_relay_with_load() {
    // tests assume testnet in config
    // Test Transfer Action
    let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
    let mut sender_id: AccountId = "relayer_test0.testnet".parse().unwrap();
    let mut receiver_id: AccountId = "relayer_test1.testnet".parse().unwrap();
    let mut nonce = 1;
    let max_block_height = 2000000000;

    let num_tests = 100;
    let mut response_statuses = vec![];

    // fire off all post requests in rapid succession and save the response status codes
    for _ in 0..num_tests {
        // Call the `relay` function happy path
        let signed_delegate_action = create_signed_delegate_action(
            sender_id.clone(),
            receiver_id.clone(),
            actions.clone(),
            nonce,
            max_block_height,
        );
        let json_payload = crate::schemas::RelayInputArgs {
            signed_delegate_action: signed_delegate_action.into(),
        };
        let response = relay(Json(json_payload)).await.into_response();
        response_statuses.push(response.status());

        nonce += 1;
        std::mem::swap(&mut sender_id, &mut receiver_id);
    }

    // all responses should be successful
    assert!(response_statuses.into_iter().all(|s| s == StatusCode::OK));
}
