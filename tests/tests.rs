use std::sync::Arc;

use axum::extract::State;

use relayer::rpc_conf::NetworkConfig;
use relayer::{AppState, RelayerConfiguration};

/**
--------------------------- Testing below here ---------------------------
 */

#[cfg(test)]
fn test_configuration() -> RelayerConfiguration {
    RelayerConfiguration {
        burn_address: "burn_address.near".parse().unwrap(),
        social_db_contract_id: "social_db.near".parse().unwrap(),
        network_config: NetworkConfig {
            rpc_url: "https://rpc.testnet.near.org".parse().unwrap(),
            rpc_api_key: None,
        },
        flametrace_performance: false,
        network_env: "testnet".to_string(),
        redis_url: "redis://127.0.0.1:6379".to_string(),
        relayer_account_id: "relayer.testnet".to_string(),
        shared_storage_account_id: "shared_storage.testnet".to_string(),
        ip_address: [127, 0, 0, 1],
        port: 3030,
        keys_filename: "./account_keys/nomnomnom.testnet.json".parse().unwrap(),
        // shared_storage_keys_filename: "relayer-test.testnet.json".parse().unwrap(),
        shared_storage_keys_filename: "./account_keys/nomnomnom.testnet.json".parse().unwrap(),
        use_fastauth_features: true,
        use_pay_with_ft: true,
        use_redis: true,
        use_shared_storage: false,
        use_whitelisted_senders: true,
        whitelisted_contracts: vec!["whitelisted_contract_1.near".parse().unwrap()],
        whitelisted_senders: vec!["arrr_me.testnet".parse().unwrap()],
        use_whitelisted_contracts: false,
        use_exchange: false,
    }
}

#[cfg(test)]
fn test_state() -> State<Arc<AppState>> {
    let config = test_configuration();
    let rpc_client = Arc::new(config.network_config.rpc_client());
    let rpc_client_nofetch = Arc::new(config.network_config.raw_rpc_client());
    let shared_storage_pool = config
        .use_shared_storage
        .then(|| relayer::create_shared_storage_pool(&config, Arc::clone(&rpc_client)));
    let redis_pool = relayer::create_redis_pool(&config);
    State(Arc::new(AppState {
        config,
        shared_storage_pool,
        rpc_client,
        rpc_client_nofetch,
        redis_pool: Some(redis_pool),
    }))
}

#[cfg(test)]
mod test {
    use axum::{
        extract::{Json, State},
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
        Router,
    };
    use near_crypto::{InMemorySigner, PublicKey};
    use near_fetch::signer::{ExposeAccountId, KeyRotatingSigner, SignerExt};
    use near_primitives::delegate_action::SignedDelegateAction;
    use near_primitives::transaction::{Action, FunctionCallAction, Transaction};
    use near_primitives::types::AccountId;
    use near_primitives::views::{
        ExecutionOutcomeWithIdView, FinalExecutionOutcomeView, FinalExecutionStatus,
    };
    use near_primitives::{borsh::BorshDeserialize, transaction::CreateAccountAction};
    use relayer::rpc_conf::NetworkConfig;
    use relayer::{AppState, RelayerConfiguration};
    use serde_json::{json, Value};

    use axum::body::{BoxBody, HttpBody};
    use axum::response::Response;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as BASE64_ENGINE, Engine};
    use bytes::BytesMut;
    use near_crypto::{KeyType, Signature};
    use near_primitives::{
        borsh::BorshSerialize,
        delegate_action::{DelegateAction, NonDelegateAction},
        transaction::TransferAction,
        types::{BlockHeight, Nonce},
    };

    fn create_signed_delegate_action(
        sender_id: String,
        receiver_id: String,
        actions: Vec<Action>,
        nonce: i32,
        max_block_height: i32,
    ) -> SignedDelegateAction {
        let max_block_height: i32 = max_block_height;
        let public_key: PublicKey = PublicKey::empty(KeyType::ED25519);
        let signature: Signature = Signature::empty(KeyType::ED25519);
        SignedDelegateAction {
            delegate_action: DelegateAction {
                sender_id: sender_id.parse().unwrap(),
                receiver_id: receiver_id.parse().unwrap(),
                actions: actions
                    .into_iter()
                    .map(|a| NonDelegateAction::try_from(a).unwrap())
                    .collect(),
                nonce: nonce as Nonce,
                max_block_height: max_block_height as BlockHeight,
                public_key,
            },
            signature,
        }
    }

    #[cfg(test)]
    async fn read_body_to_string(
        mut body: BoxBody,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // helper fn to convert the awful BoxBody dtype into a String so I can view the darn msg
        let mut bytes = BytesMut::new();
        while let Some(chunk) = body.data().await {
            bytes.extend_from_slice(&chunk?);
        }
        Ok(String::from_utf8(bytes.to_vec())?)
    }

    #[tokio::test]
    async fn test_base64_encode_args() {
        // NOTE this is how you encode the "args" in a function call
        // replace with your own "args" to base64 encode
        let ft_transfer_args_json = json!(
            {"receiver_id":"guest-book.testnet","amount":"10"}
        );
        let add_message_args_json = json!(
            {"text":"funny_joke"}
        );
        let ft_transfer_args_b64 = BASE64_ENGINE.encode(ft_transfer_args_json.to_string());
        let add_message_args_b64 = BASE64_ENGINE.encode(add_message_args_json.to_string());
        println!("ft_transfer_args_b64: {ft_transfer_args_b64}");
        println!("add_message_args_b64: {add_message_args_b64}");
    }

    #[tokio::test]
    // NOTE: uncomment ignore locally to run test bc redis doesn't work in github action build env
    #[ignore]
    async fn test_send_meta_tx() {
        // tests assume testnet in config
        // Test Transfer Action
        let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
        let sender_id: String = String::from("relayer_test0.testnet");
        let receiver_id: String = String::from("relayer_test1.testnet");
        let nonce: i32 = 1;
        let max_block_height = 2_000_000_000;

        // simulate calling the '/update_allowance' function with sender_id & allowance
        let allowance_in_gas: u64 = u64::MAX;
        set_account_and_allowance_in_redis(&sender_id, &allowance_in_gas)
            .await
            .expect("Failed to update account and allowance in redis");

        // Call the `/send_meta_tx` function happy path
        let signed_delegate_action = create_signed_delegate_action(
            sender_id.clone(),
            receiver_id.clone(),
            actions.clone(),
            nonce,
            max_block_height,
        );
        let json_payload = Json(signed_delegate_action);
        println!("SignedDelegateAction Json Serialized (no borsh): {json_payload:?}");
        let response: Response = send_meta_tx(json_payload).await.into_response();
        let response_status: StatusCode = response.status();
        let body: BoxBody = response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        println!("Response body: {body_str:?}");
        assert_eq!(response_status, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_send_meta_tx_no_gas_allowance() {
        let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
        let sender_id: String = String::from("relayer_test0.testnet");
        let receiver_id: String = String::from("arrr_me_not_in_whitelist");
        let nonce: i32 = 54321;
        let max_block_height = 2_000_000_123;

        // Call the `send_meta_tx` function with a sender that has no gas allowance
        // (and a receiver_id that isn't in whitelist)
        let sda2 = create_signed_delegate_action(
            sender_id.clone(),
            receiver_id.clone(),
            actions.clone(),
            nonce,
            max_block_height,
        );
        let non_whitelist_json_payload = Json(sda2);
        println!(
            "SignedDelegateAction Json Serialized (no borsh) receiver_id not in whitelist: {non_whitelist_json_payload:?}"
        );
        let err_response = send_meta_tx(non_whitelist_json_payload)
            .await
            .into_response();
        let err_response_status = err_response.status();
        let body: BoxBody = err_response.into_body();
        let body_str: String = read_body_to_string(body).await.unwrap();
        println!("Response body: {body_str:?}");
        assert!(
            err_response_status == StatusCode::BAD_REQUEST
                || err_response_status == StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_relay_with_load() {
        // tests assume testnet in config
        // Test Transfer Action

        let actions = vec![Action::Transfer(TransferAction { deposit: 1 })];
        let account_id0: String = "nomnomnom.testnet".to_string();
        let account_id1: String = "relayer_test0.testnet".to_string();
        let mut sender_id: String = String::new();
        let mut receiver_id: String = String::new();
        let mut nonce: i32 = 1;
        let max_block_height = 2_000_000_000;

        let num_tests = 100;
        let mut response_statuses = vec![];
        let mut response_bodies = vec![];

        // fire off all post requests in rapid succession and save the response status codes
        for i in 0..num_tests {
            if i % 2 == 0 {
                sender_id.push_str(&account_id0);
                receiver_id.push_str(&account_id1);
            } else {
                sender_id.push_str(&account_id1);
                receiver_id.push_str(&account_id0);
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
            let response = relay(Json(json_payload)).await.into_response();
            response_statuses.push(response.status());
            let body: BoxBody = response.into_body();
            let body_str: String = read_body_to_string(body).await.unwrap();
            response_bodies.push(body_str);

            // increment nonce & reset sender, receiver strs
            nonce += 1;
            sender_id.clear();
            receiver_id.clear();
        }

        // all responses should be successful
        for i in 0..response_statuses.len() {
            let response_status = response_statuses[i];
            println!("{response_status}");
            println!("{}", response_bodies[i]);
            assert_eq!(response_status, StatusCode::OK);
        }
    }
}
