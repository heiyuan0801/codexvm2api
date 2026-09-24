//! 独立槽位的配置和绑定事务。代理仍由现有代理库持有。
use super::*;
use gateway_admin::model::account_slots::{
    ContainerSlot, ContainerSlotAction, ContainerSlotCommand, ContainerSlotMutation,
};

const SELECT: &str = "select s.account_id, s.enabled, s.instance_id::text as instance_id,
 s.identity_json, s.desired_generation, s.name, s.proxy_id, s.start_requested_at is not null as start_requested,
 s.delete_requested_at is not null as delete_requested, a.name as account_name, coalesce(a.enabled, false) as account_enabled,
 p.name as proxy_name, p.location_country, p.location_region, p.location_city, p.location_timezone,
 p.last_test_ip, p.last_test_at
 from openai_account_slots s left join provider_accounts a on a.id = s.account_id
 left join outbound_proxies p on p.id = s.proxy_id order by s.created_at, s.instance_id";

pub(super) async fn list(
    repo: &PgProviderAccountRepository,
) -> AdminStoreResult<Vec<ContainerSlot>> {
    let rows = sqlx::query(SELECT)
        .fetch_all(&repo.pool)
        .await
        .map_err(db_error)?;
    rows.into_iter()
        .map(|row| {
            let document: serde_json::Value = row.try_get("identity_json").map_err(db_error)?;
            let identity = super::slots::identity_from_document(document)
                .map_err(|_| invalid("槽位身份数据无效"))?;
            let id: String = row.try_get("instance_id").map_err(db_error)?;
            let generation: i64 = row.try_get("desired_generation").map_err(db_error)?;
            let proxy_location = crate::postgres::proxies::location_from_row(&row)
                .map_err(|_| invalid("代理位置数据无效"))?;
            let proxy_exit_ip = row
                .try_get::<Option<String>, _>("last_test_ip")
                .map_err(db_error)?
                .map(|value| value.parse().map_err(|_| invalid("代理出口 IP 无效")))
                .transpose()?;
            Ok(ContainerSlot {
                id: AccountSlotInstanceId::parse(&format!("slot_{id}"))
                    .map_err(|_| invalid("槽位 ID 无效"))?,
                name: row.try_get("name").map_err(db_error)?,
                account_id: row
                    .try_get::<Option<String>, _>("account_id")
                    .map_err(db_error)?
                    .map(CoreProviderAccountId::new)
                    .transpose()
                    .map_err(|_| invalid("账号 ID 无效"))?,
                account_name: row.try_get("account_name").map_err(db_error)?,
                account_enabled: row.try_get("account_enabled").map_err(db_error)?,
                proxy_id: row.try_get("proxy_id").map_err(db_error)?,
                proxy_name: row.try_get("proxy_name").map_err(db_error)?,
                proxy_location,
                proxy_exit_ip,
                proxy_tested_at: row.try_get("last_test_at").map_err(db_error)?,
                running: row.try_get("enabled").map_err(db_error)?,
                start_requested: row.try_get("start_requested").map_err(db_error)?,
                delete_requested: row.try_get("delete_requested").map_err(db_error)?,
                identity,
                generation: u64::try_from(generation)
                    .ok()
                    .and_then(AccountSlotGeneration::new)
                    .ok_or_else(|| invalid("槽位代次无效"))?,
            })
        })
        .collect()
}

pub(super) async fn mutate(
    repo: &PgProviderAccountRepository,
    command: ContainerSlotCommand,
    context: &MutationContext,
) -> AdminStoreResult<ContainerSlotMutation> {
    let mut tx = repo.pool.begin().await.map_err(db_error)?;
    let mut affected_accounts = Vec::new();
    let id = command.id.unwrap_or_else(AccountSlotInstanceId::generate);
    let id_text = id.uuid().to_string();
    let action_name;
    if let ContainerSlotAction::Create { name } = command.action {
        if name.trim().is_empty() || name.chars().count() > 100 {
            return Err(invalid("容器名称无效"));
        }
        let identity = serde_json::json!({
            "hostname": format!("cpr-slot-{}", id.uuid().simple()),
            "machine_id": uuid::Uuid::new_v4().simple().to_string(),
            "installation_id": uuid::Uuid::new_v4().to_string(), "timezone": "UTC"
        });
        sqlx::query("insert into openai_account_slots (instance_id, name, identity_json) values ($1::uuid, $2, $3)")
            .bind(&id_text).bind(name.trim()).bind(identity).execute(&mut *tx).await.map_err(db_error)?;
        action_name = "create_container";
    } else {
        let row = sqlx::query("select account_id, enabled, proxy_id, identity_json, desired_generation, delete_requested_at is not null as deleting from openai_account_slots where instance_id = $1::uuid for update")
            .bind(&id_text).fetch_optional(&mut *tx).await.map_err(db_error)?
            .ok_or_else(|| AdminStoreError::new(AdminStoreErrorKind::NotFound, "containers", "槽位不存在"))?;
        let generation: i64 = row.try_get("desired_generation").map_err(db_error)?;
        if u64::try_from(generation).ok() != command.expected_generation {
            return Err(AdminStoreError::new(
                AdminStoreErrorKind::StaleRevision,
                "containers",
                "槽位已更新，请刷新",
            ));
        }
        if row.try_get::<bool, _>("deleting").map_err(db_error)? {
            return Err(invalid("容器正在删除，请等待清理完成"));
        }
        let account: Option<String> = row.try_get("account_id").map_err(db_error)?;
        let running: bool = row.try_get("enabled").map_err(db_error)?;
        let proxy: Option<String> = row.try_get("proxy_id").map_err(db_error)?;
        let identity: serde_json::Value = row.try_get("identity_json").map_err(db_error)?;
        if let Some(account) = &account {
            affected_accounts.push(
                CoreProviderAccountId::new(account.clone()).map_err(|_| invalid("账号 ID 无效"))?,
            );
        }
        match command.action {
            ContainerSlotAction::ConfigureProxy { proxy_id } => {
                if running {
                    return Err(invalid("请先停止容器，再修改代理"));
                }
                let mut updated_identity = identity;
                let proxy_timezone = if let Some(proxy_id) = &proxy_id {
                    let record: Option<(String, Option<String>)> = sqlx::query_as(
                        "select proxy_url, location_timezone from outbound_proxies where id = $1 for share",
                    )
                    .bind(proxy_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_error)?;
                    let (url, timezone) = record.ok_or_else(|| invalid("请选择有效代理"))?;
                    if gateway_core::account::OutboundProxy::parse(&url).is_err() {
                        return Err(invalid("请选择有效代理"));
                    }
                    timezone
                } else {
                    None
                };
                let mut identity_changed = false;
                if let Some(timezone) = proxy_timezone {
                    if updated_identity
                        .get("timezone")
                        .and_then(serde_json::Value::as_str)
                        != Some(timezone.as_str())
                    {
                        updated_identity["timezone"] = serde_json::Value::String(timezone);
                        identity_changed = true;
                    }
                }
                let proxy_changed = proxy != proxy_id;
                let generation_bump = i64::from(identity_changed && !proxy_changed);
                sqlx::query(
                    "update openai_account_slots set proxy_id = $2, identity_json = $3, desired_generation = desired_generation + $4 where instance_id = $1::uuid",
                )
                .bind(&id_text)
                .bind(proxy_id)
                .bind(updated_identity)
                .bind(generation_bump)
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
                action_name = "configure_container_proxy";
            }
            ContainerSlotAction::Bind { account_id } => {
                if running {
                    return Err(invalid("请先停止容器，再更改账号绑定"));
                }
                if let Some(account_id) = &account_id {
                    if proxy.is_none() {
                        return Err(invalid("请先配置槽位代理"));
                    }
                    let eligible: Option<bool> = sqlx::query_scalar("select provider_kind = 'openai' and authentication_kind = 'oauth' from provider_accounts where id = $1 for update")
                        .bind(account_id.as_str()).fetch_optional(&mut *tx).await.map_err(db_error)?;
                    if eligible != Some(true) {
                        return Err(invalid("仅支持绑定 OpenAI OAuth 账号"));
                    }
                    affected_accounts.push(account_id.clone());
                }
                sqlx::query(
                    "update openai_account_slots set account_id = $2 where instance_id = $1::uuid",
                )
                .bind(&id_text)
                .bind(account_id.as_ref().map(CoreProviderAccountId::as_str))
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
                action_name = "bind_container_account";
            }
            ContainerSlotAction::Start => {
                if proxy.is_none() || account.is_none() {
                    return Err(invalid("请先配置代理并绑定账号"));
                }
                let enabled: Option<bool> = sqlx::query_scalar("select enabled and provider_kind = 'openai' and authentication_kind = 'oauth' from provider_accounts where id = $1 for share")
                    .bind(&account).fetch_optional(&mut *tx).await.map_err(db_error)?;
                if enabled != Some(true) {
                    return Err(invalid("请先启用绑定的 OpenAI OAuth 账号"));
                }
                sqlx::query("update openai_account_slots set enabled = true, start_requested_at = now() where instance_id = $1::uuid")
                    .bind(&id_text).execute(&mut *tx).await.map_err(db_error)?;
                action_name = "start_container";
            }
            ContainerSlotAction::Stop => {
                sqlx::query(
                    "update openai_account_slots set enabled = false where instance_id = $1::uuid",
                )
                .bind(&id_text)
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
                action_name = "stop_container";
            }
            ContainerSlotAction::Delete => {
                if running || account.is_some() {
                    return Err(invalid("请先停止容器并解绑账号"));
                }
                // 空槽也核对遗留资源，避免历史迁移缺少启动时间时漏清理。
                sqlx::query("update openai_account_slots set delete_requested_at = now(), desired_generation = desired_generation + 1 where instance_id = $1::uuid")
                    .bind(&id_text).execute(&mut *tx).await.map_err(db_error)?;
                action_name = "delete_container";
            }
            ContainerSlotAction::Create { .. } => return Err(invalid("操作无效")),
        }
    }
    let revision = bump_config_revision_in_transaction(&mut tx)
        .await
        .map_err(|_| invalid("配置版本更新失败"))?;
    append_admin_audit_event_in_transaction(
        &mut tx,
        mutation_audit(
            context,
            action_name,
            "container_slot",
            &id.to_string(),
            vec!["slot_configuration".to_owned()],
        ),
        revision,
    )
    .await
    .map_err(|_| invalid("审计写入失败"))?;
    tx.commit().await.map_err(db_error)?;
    Ok(ContainerSlotMutation {
        config_revision: admin_revision(revision)?,
        affected_accounts,
    })
}

fn invalid(message: &'static str) -> AdminStoreError {
    AdminStoreError::new(AdminStoreErrorKind::Invalid, "containers", message)
}
fn db_error(error: sqlx::Error) -> AdminStoreError {
    if error
        .as_database_error()
        .is_some_and(|e| e.is_unique_violation())
    {
        return AdminStoreError::new(
            AdminStoreErrorKind::Conflict,
            "containers",
            "该账号已绑定其他槽位",
        );
    }
    if error
        .as_database_error()
        .is_some_and(|e| e.is_foreign_key_violation())
    {
        return invalid("账号或代理已变更，请刷新");
    }
    AdminStoreError::new(
        AdminStoreErrorKind::Unavailable,
        "containers",
        "容器配置暂不可用",
    )
}
