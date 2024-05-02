use anyhow::{anyhow, Context};
use near_crypto::InMemorySigner;
use near_fetch::Client;
use near_primitives::transaction::{Action, FunctionCallAction};
use near_primitives::types::{AccountId, Balance, Gas, StorageUsage};
use serde::Deserialize;
use std::sync::Arc;
use tracing::debug;

/// Gas for transactions to shared storage pool.
const MAX_GAS: Gas = 300_000_000_000_000;

/// Constant set at 1E19 yoctoNEAR per byte on chain.
// TODO: This is set for reference on chain, and can change with future protocol changes,
// so best to retrieve chain config later.
const STORAGE_PRICE_PER_BYTE: Balance = 10_000_000_000_000_000_000;

/// Minimum deposit required to use the shared storage pool. This will also be the amount
/// to reup the pool every time. This is set to the minimum that social DB wants of 100N.
const STORAGE_UP_DEPOSIT: Balance = 100 * 10u128.pow(24);

/// Default amount of bytes allocated per account. This will not change and is set to 50KB.
const BYTES_ALLOCATED_PER_ACCOUNT: u64 = bytes_per_amount(near_units::parse_near!("0.5 N"));

const fn bytes_per_amount(amount: Balance) -> u64 {
    (amount / STORAGE_PRICE_PER_BYTE) as u64
}

/// Shared storage pool manager is used to manage the storage pools in a contract
/// that implements the shared storage pool interface. This includes social DB for
/// near social.
pub struct SharedStoragePoolManager {
    signer: InMemorySigner,
    rpc_client: Arc<Client>,
    pool_contract_id: AccountId,
    pool_owner_id: AccountId,
}
impl std::fmt::Debug for SharedStoragePoolManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedStoragePoolManager")
            .field("rpc_client", &self.rpc_client)
            .field("pool_contract_id", &self.pool_contract_id)
            .field("pool_owner_id", &self.pool_owner_id)
            .finish()
    }
}

impl SharedStoragePoolManager {
    pub fn new(
        signer: InMemorySigner,
        rpc_client: Arc<Client>,
        pool_contract_id: AccountId,
        pool_owner_id: AccountId,
    ) -> Self {
        Self {
            signer,
            rpc_client,
            pool_contract_id,
            pool_owner_id,
        }
    }

    /// Spawn the pool if it hasn't been yet.
    pub async fn check_and_spawn_pool(&self) -> anyhow::Result<()> {
        // let pool = self.get_shared_storage_pool().await?;
        // debug!(">> POOL: {pool:?}");
        if let Some(pool) = self.get_shared_storage_pool().await? {
            debug!("Shared storage pool already exists: {pool:?}");
        } else {
            debug!("Spawning shared storage pool");
            self.allocate_deposit_to_pool().await?;
        }

        Ok(())
    }

    /// Allocate default number of bytes per account in the shared pool.
    pub async fn allocate_default(&self, id: AccountId) -> anyhow::Result<()> {
        // NOTE: Allocating default amount of bytes is idempotent since calling into
        // share_storage will only set the max bytes and not increase it. Thus calling
        // with the same amount of bytes will not increase the amount of bytes allocated.
        self.allocate(id, BYTES_ALLOCATED_PER_ACCOUNT).await
    }

    /// Allocate a set number of bytes for an account in the shared pool.
    pub async fn allocate(&self, id: AccountId, max_bytes: u64) -> anyhow::Result<()> {
        // TODO: figure out why get_account_storage doesn't work for an account like:
        // `social-storage.pagodaplatform.near`
        self.share_storage(id, max_bytes).await?;
        Ok(())
    }

    async fn allocate_deposit_to_pool(&self) -> anyhow::Result<()> {
        let actions = vec![Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "shared_storage_pool_deposit".into(),
            args: serde_json::json!({
                "owner_id": self.pool_owner_id.clone(),
            })
            .to_string()
            .into_bytes(),
            gas: MAX_GAS,
            deposit: STORAGE_UP_DEPOSIT,
        }))];
        self.rpc_client
            .send_tx(&self.signer, &self.pool_contract_id, actions, None)
            .await
            .context("failed to send transaction for shared_storage_pool_deposit")?;
        Ok(())
    }

    async fn share_storage(&self, id: AccountId, max_bytes: StorageUsage) -> anyhow::Result<()> {
        let actions = vec![Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "share_storage".into(),
            args: serde_json::json!({
                "account_id": id,
                "max_bytes": max_bytes,
            })
            .to_string()
            .into_bytes(),
            gas: MAX_GAS,
            deposit: 0,
        }))];
        self.rpc_client
            .send_tx(&self.signer, &self.pool_contract_id, actions, None)
            .await
            .context("failed to send transaction for share_storage")?;
        Ok(())
    }

    async fn get_shared_storage_pool(&self) -> anyhow::Result<Option<SharedStoragePool>> {
        let res = self
            .rpc_client
            .view(&self.pool_contract_id, "get_shared_storage_pool")
            .args_json(serde_json::json!({
                "owner_id": self.pool_owner_id.clone(),
            }))
            .await;
        match res {
            Ok(res) => serde_json::from_slice(&res.result).map_err(|e| anyhow!(e)),
            Err(e) => Err(anyhow!(e)),
        }
    }
}

/// Taken directly from near.social contract to deserialize into when calling
/// get_account_storage.
#[derive(Debug, Deserialize)]
pub struct StorageView {
    pub used_bytes: StorageUsage,
    pub available_bytes: StorageUsage,
}

/// Taken directly from near.social contract to deserialize into when calling
/// get_shared_storage_pool
// JSON deserialization trick. no need to understand what actual structure is.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SharedStoragePool(serde_json::Map<String, serde_json::Value>);
