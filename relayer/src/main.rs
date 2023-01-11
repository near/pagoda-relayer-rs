use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

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
    // TODO get payload and create Transaction from SignedDelegateAction and send to RPC
    let relay = Relay {
        signed_delegate_action: payload.signed_delegate_action,
    };

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    (StatusCode::CREATED, Json(relay))
}

// the input to our `create_relay` handler
#[derive(Deserialize)]
struct CreateRelay {
    // TODO get SignedDelegateAction from https://github.com/binary-star-near/nearcore/tree/NEP-366 instead of near_primatives
    // signed_delegate_action: near_primitives::transaction::SignedDelegateAction,
    signed_delegate_action: String,
}

// the output to our `create_relay` handler
#[derive(Serialize)]
struct Relay {
    // TODO get SignedDelegateAction from https://github.com/binary-star-near/nearcore/tree/NEP-366 instead of near_primatives
    // signed_delegate_action: near_primitives::transaction::SignedDelegateAction,
    signed_delegate_action: String,
}
