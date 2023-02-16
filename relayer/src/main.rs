mod config;

use axum::{response::IntoResponse, routing::post, Router, extract};
use std::net::SocketAddr;
use near_primitives::borsh::BorshDeserialize;
use near_primitives::delegate_action::SignedDelegateAction;

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
            // TODO create near_primitives::transaction::Transaction from SignedDelegateAction

            // TODO filter out transfer Action types (FT transfers or NFT OK)
            // let transfer_action = near_primitives::transaction::TransferAction;

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

            "Successfully relayed SignedDelegateAction".into_response()
        },
        Err(e) => {
            println!("Error deserializing MyData object: {:?}", e);
            "Error deserializing MyData object".into_response()
        },
    }

}
