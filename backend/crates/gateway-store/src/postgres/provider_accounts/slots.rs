//! OpenAI 账号槽位期望状态的 PostgreSQL adapter。

use serde::Deserialize;

use super::*;

#[derive(Deserialize)]
struct SlotIdentityDocument {
    hostname: String,
    machine_id: String,
    installation_id: String,
    timezone: String,
}

#[async_trait]
impl ProviderAccountSlotStore for PgProviderAccountRepository {
    async fn get_account_slot(
        &self,
        account_id: &CoreProviderAccountId,
    ) -> Result<Option<ProviderAccountSlot>, CoreStoreError> {
        let row = sqlx::query(
            "select account_id, enabled, instance_id::text as instance_id, identity_json, desired_generation
             from openai_account_slots where account_id = $1",
        )
        .bind(account_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| CoreStoreError::new(CoreStoreErrorKind::Unavailable))?;
        row.map(slot_from_row).transpose()
    }

    async fn list_account_slots(&self) -> Result<Vec<ProviderAccountSlot>, CoreStoreError> {
        sqlx::query(
            "select account_id, enabled, instance_id::text as instance_id, identity_json, desired_generation
             from openai_account_slots order by account_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| CoreStoreError::new(CoreStoreErrorKind::Unavailable))?
        .into_iter()
        .map(slot_from_row)
        .collect()
    }
}

fn slot_from_row(row: sqlx::postgres::PgRow) -> Result<ProviderAccountSlot, CoreStoreError> {
    let invalid = || CoreStoreError::new(CoreStoreErrorKind::InvalidData);
    let account_id = row
        .try_get::<String, _>("account_id")
        .map_err(|_| invalid())
        .and_then(|value| CoreProviderAccountId::new(value).map_err(|_| invalid()))?;
    let enabled = row.try_get::<bool, _>("enabled").map_err(|_| invalid())?;
    let instance_id = row
        .try_get::<String, _>("instance_id")
        .map_err(|_| invalid())
        .and_then(|value| {
            AccountSlotInstanceId::parse(&format!("slot_{value}")).map_err(|_| invalid())
        })?;
    let identity = row
        .try_get::<serde_json::Value, _>("identity_json")
        .map_err(|_| invalid())
        .and_then(|value| {
            serde_json::from_value::<SlotIdentityDocument>(value).map_err(|_| invalid())
        })
        .and_then(|document| {
            AccountSlotIdentity::new(
                document.hostname,
                document.machine_id,
                document.installation_id,
                document.timezone,
            )
            .map_err(|_| invalid())
        })?;
    let generation = row
        .try_get::<i64, _>("desired_generation")
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .and_then(AccountSlotGeneration::new)
        .ok_or_else(invalid)?;
    Ok(ProviderAccountSlot::new(
        account_id,
        enabled,
        instance_id,
        identity,
        generation,
    ))
}
