use super::*;
use gateway_admin::model::account_slots::{ContainerSlotAction as Action, ContainerSlotCommand};
use gateway_admin::ports::account_slots::AccountSlotAdminStore;
use gateway_admin::ports::store::AdminStoreErrorKind;

#[tokio::test]
async fn independent_slots_create_configure_bind_start_stop_and_preserve_identity() {
    let Some(db) = TestDatabase::create("independent_slots").await else {
        return;
    };
    let repo = PgProviderAccountRepository::new(db.pool.clone());
    repo.insert_provider_account(account("acct_container", "slot-user"))
        .await
        .unwrap();
    let context = MutationContext {
        actor: MutationActor::System,
        request_id: "container-test".into(),
    };
    repo.mutate_container(
        ContainerSlotCommand {
            id: None,
            expected_generation: None,
            action: Action::Create {
                name: "First container".into(),
            },
        },
        &context,
    )
    .await
    .unwrap();
    let slot = repo.containers().await.unwrap().remove(0);
    assert!(slot.account_id.is_none());
    assert!(!slot.running);
    assert!(repo.list_account_slots().await.unwrap().is_empty());
    let id = slot.id;
    let identity = slot.identity.clone();
    repo.complete_slot_deletion(id).await.unwrap();
    assert_eq!(repo.containers().await.unwrap().len(), 1);
    let command = |generation, action| ContainerSlotCommand {
        id: Some(id),
        expected_generation: Some(generation),
        action,
    };
    assert!(matches!(
        repo.mutate_container(command(1, Action::Start), &context)
            .await
            .err()
            .unwrap()
            .kind(),
        AdminStoreErrorKind::Invalid
    ));
    assert!(
        repo.mutate_container(
            command(
                1,
                Action::Bind {
                    account_id: Some(ProviderAccountId::new("acct_container").unwrap())
                }
            ),
            &context
        )
        .await
        .is_err()
    );
    sqlx::query("insert into outbound_proxies (id, name, proxy_url) values ('proxy_container', 'Synthetic proxy', 'http://proxy-a:3128')").execute(&db.pool).await.unwrap();
    repo.mutate_container(
        command(
            1,
            Action::ConfigureProxy {
                proxy_id: Some("proxy_container".into()),
            },
        ),
        &context,
    )
    .await
    .unwrap();
    repo.mutate_container(
        command(
            2,
            Action::Bind {
                account_id: Some(ProviderAccountId::new("acct_container").unwrap()),
            },
        ),
        &context,
    )
    .await
    .unwrap();
    // 账号本身没有代理，槽位仍可独立配置和启动。
    repo.mutate_container(command(3, Action::Start), &context)
        .await
        .unwrap();
    let ready_intent = repo
        .get_account_slot(&ProviderAccountId::new("acct_container").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(ready_intent.enabled());
    assert_eq!(ready_intent.identity(), &identity);
    assert!(ready_intent.outbound_proxy().is_some());
    assert!(
        repo.mutate_container(command(4, Action::Bind { account_id: None }), &context)
            .await
            .is_err()
    );
    assert!(matches!(
        repo.mutate_container(command(2, Action::Stop), &context)
            .await
            .err()
            .unwrap()
            .kind(),
        AdminStoreErrorKind::StaleRevision
    ));
    // 代理库更新推动槽位代次，账号侧的不同出口不覆盖槽位代理。
    sqlx::query("update outbound_proxies set proxy_url = 'socks5://proxy-b:1080' where id = 'proxy_container'").execute(&db.pool).await.unwrap();
    sqlx::query("update provider_accounts set outbound_proxy_url = 'http://account-proxy:8080' where id = 'acct_container'").execute(&db.pool).await.unwrap();
    let current = repo
        .get_account_slot(&ProviderAccountId::new("acct_container").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.generation().get(), 5);
    assert_eq!(
        current.outbound_proxy(),
        Some(&gateway_core::account::OutboundProxy::parse("socks5://proxy-b:1080").unwrap())
    );
    repo.mutate_container(command(5, Action::Stop), &context)
        .await
        .unwrap();
    let stopped = repo
        .get_account_slot(&ProviderAccountId::new("acct_container").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(!stopped.enabled());
    assert_eq!(stopped.instance_id(), id);
    assert_eq!(stopped.identity(), &identity);
    // 停机保留绑定供 Provider fail closed；解绑才移除账号路由归属。
    repo.mutate_container(command(6, Action::Bind { account_id: None }), &context)
        .await
        .unwrap();
    assert!(
        repo.get_account_slot(&ProviderAccountId::new("acct_container").unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(repo.containers().await.unwrap()[0].identity, identity);
    repo.mutate_container(command(7, Action::Delete), &context)
        .await
        .unwrap();
    assert!(repo.containers().await.unwrap()[0].delete_requested);
    assert_eq!(repo.pending_slot_deletions().await.unwrap(), vec![id]);
    assert!(
        repo.mutate_container(command(8, Action::Start), &context)
            .await
            .is_err()
    );
    assert!(
        repo.mutate_container(
            command(
                8,
                Action::Bind {
                    account_id: Some(ProviderAccountId::new("acct_container").unwrap())
                }
            ),
            &context
        )
        .await
        .is_err()
    );
    // 清理成功前不能移除记录；完成回执和重试均幂等。
    repo.complete_slot_deletion(id).await.unwrap();
    repo.complete_slot_deletion(id).await.unwrap();
    assert!(repo.containers().await.unwrap().is_empty());
    assert!(repo.pending_slot_deletions().await.unwrap().is_empty());
    db.close().await;
}

#[tokio::test]
async fn independent_slots_enforce_unique_bindings_and_optimistic_concurrency() {
    let Some(db) = TestDatabase::create("slot_binding_race").await else {
        return;
    };
    let repo = PgProviderAccountRepository::new(db.pool.clone());
    repo.insert_provider_account(account("acct_container_race", "slot-race"))
        .await
        .unwrap();
    let context = MutationContext {
        actor: MutationActor::System,
        request_id: "container-race".into(),
    };
    sqlx::query("insert into outbound_proxies (id, name, proxy_url) values ('proxy_race', 'Synthetic proxy', 'http://proxy:3128')").execute(&db.pool).await.unwrap();
    for name in ["A", "B"] {
        repo.mutate_container(
            ContainerSlotCommand {
                id: None,
                expected_generation: None,
                action: Action::Create { name: name.into() },
            },
            &context,
        )
        .await
        .unwrap();
    }
    let slots = repo.containers().await.unwrap();
    for slot in &slots {
        repo.mutate_container(
            ContainerSlotCommand {
                id: Some(slot.id),
                expected_generation: Some(1),
                action: Action::ConfigureProxy {
                    proxy_id: Some("proxy_race".into()),
                },
            },
            &context,
        )
        .await
        .unwrap();
    }
    let binding = |id| ContainerSlotCommand {
        id: Some(id),
        expected_generation: Some(2),
        action: Action::Bind {
            account_id: Some(ProviderAccountId::new("acct_container_race").unwrap()),
        },
    };
    let (first, second) = tokio::join!(
        repo.mutate_container(binding(slots[0].id), &context),
        repo.mutate_container(binding(slots[1].id), &context)
    );
    assert_ne!(first.is_ok(), second.is_ok());
    assert!(matches!(
        first.err().or(second.err()).unwrap().kind(),
        AdminStoreErrorKind::Conflict
    ));
    assert_eq!(repo.list_account_slots().await.unwrap().len(), 1);
    db.close().await;
}
