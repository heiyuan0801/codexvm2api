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
    async fn pending_slot_deletions(&self) -> Result<Vec<AccountSlotInstanceId>, CoreStoreError> {
        let ids: Vec<String> = sqlx::query_scalar("select instance_id::text from openai_account_slots where delete_requested_at is not null order by delete_requested_at")
            .fetch_all(&self.pool).await.map_err(|_| CoreStoreError::new(CoreStoreErrorKind::Unavailable))?;
        ids.into_iter()
            .map(|id| {
                AccountSlotInstanceId::parse(&format!("slot_{id}"))
                    .map_err(|_| CoreStoreError::new(CoreStoreErrorKind::InvalidData))
            })
            .collect()
    }

    async fn complete_slot_deletion(
        &self,
        id: AccountSlotInstanceId,
    ) -> Result<(), CoreStoreError> {
        sqlx::query("delete from openai_account_slots where instance_id = $1::uuid and delete_requested_at is not null and not enabled and account_id is null")
            .bind(id.uuid().to_string()).execute(&self.pool).await
            .map_err(|_| CoreStoreError::new(CoreStoreErrorKind::Unavailable))?;
        Ok(())
    }

    async fn get_account_slot(
        &self,
        account_id: &CoreProviderAccountId,
    ) -> Result<Option<ProviderAccountSlot>, CoreStoreError> {
        let row = sqlx::query(
            "select s.account_id, s.enabled, s.instance_id::text as instance_id, s.identity_json, s.desired_generation, p.proxy_url
             from openai_account_slots s left join outbound_proxies p on p.id = s.proxy_id where s.account_id = $1",
        )
        .bind(account_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| CoreStoreError::new(CoreStoreErrorKind::Unavailable))?;
        row.map(slot_from_row).transpose()
    }

    async fn list_account_slots(&self) -> Result<Vec<ProviderAccountSlot>, CoreStoreError> {
        sqlx::query(
            "select s.account_id, s.enabled, s.instance_id::text as instance_id, s.identity_json, s.desired_generation, p.proxy_url
             from openai_account_slots s left join outbound_proxies p on p.id = s.proxy_id where s.account_id is not null order by s.account_id",
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
    let identity = identity_from_document(row.try_get("identity_json").map_err(|_| invalid())?)?;
    let proxy = row
        .try_get::<Option<String>, _>("proxy_url")
        .ok()
        .flatten()
        .map(|url| gateway_core::account::OutboundProxy::parse(&url).map_err(|_| invalid()))
        .transpose()?;
    let generation = row
        .try_get::<i64, _>("desired_generation")
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .and_then(AccountSlotGeneration::new)
        .ok_or_else(invalid)?;
    Ok(
        ProviderAccountSlot::new(account_id, enabled, instance_id, identity, generation)
            .with_outbound_proxy(proxy),
    )
}

pub(super) fn identity_from_document(
    value: serde_json::Value,
) -> Result<AccountSlotIdentity, CoreStoreError> {
    let invalid = || CoreStoreError::new(CoreStoreErrorKind::InvalidData);
    let doc: SlotIdentityDocument = serde_json::from_value(value).map_err(|_| invalid())?;
    AccountSlotIdentity::new(
        doc.hostname,
        doc.machine_id,
        doc.installation_id,
        doc.timezone,
    )
    .map_err(|_| invalid())
}

#[async_trait]
impl gateway_admin::ports::account_slots::AccountSlotAdminStore for PgProviderAccountRepository {
    async fn containers(
        &self,
    ) -> AdminStoreResult<Vec<gateway_admin::model::account_slots::ContainerSlot>> {
        super::containers::list(self).await
    }
    async fn mutate_container(
        &self,
        command: gateway_admin::model::account_slots::ContainerSlotCommand,
        context: &MutationContext,
    ) -> AdminStoreResult<gateway_admin::model::account_slots::ContainerSlotMutation> {
        super::containers::mutate(self, command, context).await
    }

    async fn read(
        &self,
        accounts: &[CoreProviderAccountId],
    ) -> AdminStoreResult<Vec<gateway_admin::model::account_slots::SlotAccount>> {
        let ids = accounts
            .iter()
            .map(CoreProviderAccountId::as_str)
            .collect::<Vec<_>>();
        let rows = sqlx::query("select a.id as account_id, a.provider_kind, a.enabled as account_enabled, p.proxy_url as outbound_proxy_url,
            s.enabled, s.instance_id::text as instance_id, s.identity_json, s.desired_generation, p.proxy_url
            from provider_accounts a left join openai_account_slots s on s.account_id = a.id left join outbound_proxies p on p.id = s.proxy_id
            where a.id = any($1) order by a.id")
            .bind(ids).fetch_all(&self.pool).await.map_err(|_| slot_admin_error(AdminStoreErrorKind::Unavailable))?;
        rows.into_iter()
            .map(|row| {
                let invalid = |_| slot_admin_error(AdminStoreErrorKind::Unavailable);
                let account_id = CoreProviderAccountId::new(
                    row.try_get::<String, _>("account_id").map_err(invalid)?,
                )
                .map_err(|_| slot_admin_error(AdminStoreErrorKind::Unavailable))?;
                let provider =
                    ProviderKind::new(row.try_get::<String, _>("provider_kind").map_err(invalid)?)
                        .map_err(|_| slot_admin_error(AdminStoreErrorKind::Unavailable))?;
                let account_enabled = row.try_get::<bool, _>("account_enabled").map_err(invalid)?;
                let has_proxy = row
                    .try_get::<Option<String>, _>("outbound_proxy_url")
                    .map_err(invalid)?
                    .as_deref()
                    .is_some_and(|value| {
                        gateway_core::account::OutboundProxy::parse(value).is_ok()
                    });
                let present = row
                    .try_get::<Option<String>, _>("instance_id")
                    .map_err(invalid)?
                    .is_some();
                let slot = if present {
                    Some(
                        slot_from_row(row)
                            .map_err(|_| slot_admin_error(AdminStoreErrorKind::Unavailable))?,
                    )
                } else {
                    None
                };
                Ok(gateway_admin::model::account_slots::SlotAccount {
                    account_id,
                    provider,
                    has_proxy,
                    account_enabled,
                    slot,
                })
            })
            .collect()
    }

    async fn set(
        &self,
        command: gateway_admin::model::account_slots::SetAccountSlot,
        context: &MutationContext,
    ) -> AdminStoreResult<gateway_admin::model::account_slots::AccountSlotMutation> {
        let _ = (command, context);
        Err(AdminStoreError::new(
            AdminStoreErrorKind::Invalid,
            "account slots",
            "请在容器管理中创建和启停槽位",
        ))
    }
}

fn slot_admin_error(kind: AdminStoreErrorKind) -> AdminStoreError {
    AdminStoreError::new(kind, "account slots", "account slot operation failed")
}
