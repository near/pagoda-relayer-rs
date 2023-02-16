mod config;

use axum::{response::IntoResponse, routing::post, Router, extract};
use std::net::SocketAddr;
use near_primitives::borsh::BorshDeserialize;
use near_primitives::delegate_action::{DelegateAction, NonDelegateAction, SignedDelegateAction};
use near_primitives::transaction::Action;

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

            // filter out transfer Action types (FT transfers or NFT OK)
            let filtered_actions: Vec<NonDelegateAction> = signed_delegate_action.delegate_action.actions
                .into_iter()
                .map(|a| Action::try_from(a.clone()).unwrap())
                .filter(|action| {
                    // TODO error[E0308]: mismatched types expected `NonDelegateAction`, found `Action`
                    if let Action::Transfer(_) = action {
                        false // exclude TransferAction types
                    } else {
                        true // include all other Action variants
                    }
                })
                .collect();
            // SignedDelegateAction is immutable so need to create new instance post action filter
            let signed_delegate_action_filtered = SignedDelegateAction {
                delegate_action: DelegateAction {
                    sender_id: signed_delegate_action.delegate_action.sender_id,
                    receiver_id: signed_delegate_action.delegate_action.receiver_id,
                    actions: filtered_actions,
                    nonce: signed_delegate_action.delegate_action.nonce,
                    max_block_height: signed_delegate_action.delegate_action.max_block_height,
                    public_key: signed_delegate_action.delegate_action.public_key,
                },
                signature: signed_delegate_action.signature,
            };

            // TODO create near_primitives::transaction::Transaction from SignedDelegateAction

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
