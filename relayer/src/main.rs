mod config;

use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use near_primitives::transaction::TransferAction;

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
    // this argument tells axum to parse the request body
    // as JSON into a `CreateRelay` type
    Json(payload): Json<CreateRelay>,
) -> impl IntoResponse {
    // TODO get payload and create near_primitives::transaction::Transaction from SignedDelegateAction
    let relay = Relay {
        signed_delegate_action: payload.signed_delegate_action,
    };

    // TODO filter out transfer Action types (FT transfers or NFT OK)
    //let transfer_action = near_primitives::transaction::TransferAction;

    // create json_rpc_client, TODO send the Transaction
    println!("Sending transaction ...");
    // let transaction_info = loop {
    //     let transaction_info_result = network_config.json_rpc_client()
    //         .call(near_jsonrpc_client::methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest{signed_transaction: signed_transaction.clone()})
    //         .await;
    //     match transaction_info_result {
    //         Ok(response) => {
    //             break response;
    //         }
    //         Err(err) => match crate::common::rpc_transaction_error(err) {
    //             Ok(_) => {
    //                 tokio::time::sleep(std::time::Duration::from_millis(100)).await
    //             }
    //             Err(report) => return color_eyre::eyre::Result::Err(report),
    //         },
    //     };
    // };
    // TODO return transaction_info

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    (StatusCode::CREATED, Json(relay))
}

// the input to our `create_relay` handler
#[derive(Deserialize)]
struct CreateRelay {
    // TODO get SignedDelegateAction from nearcore instead of near_primatives
    // signed_delegate_action: near_primitives::transaction::SignedDelegateAction,
    signed_delegate_action: String,  // mocking out signed_delegate_action as String to compile - rm when typing and deserialization is done
}

// the output to our `create_relay` handler
#[derive(Serialize)]
struct Relay {
    // TODO get SignedDelegateAction from nearcore instead of near_primatives
    // signed_delegate_action: near_primitives::transaction::SignedDelegateAction,
    signed_delegate_action: String,  // mocking out signed_delegate_action as String to compile - rm when typing and deserialization is done
}
