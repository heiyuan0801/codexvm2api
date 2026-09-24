use super::*;
use gateway_admin::model::{AdminErrorKind, MutationActor, account_slots::*};
use gateway_admin::ports::account_slots::{AccountSlotAdminStore, AccountSlotControl};
use gateway_admin::{AccountSlotsService, DefaultAccountSlotsService};
use gateway_core::account::{
    AccountSlotGeneration, AccountSlotIdentity, AccountSlotInstanceId, AccountSlotRoute,
    AccountSlotRuntime, AccountSlotState, ProviderAccountSlot,
};

struct SlotStore {
    provider: &'static str,
    proxy: bool,
    account_enabled: bool,
}
impl SlotStore {
    fn account(&self) -> SlotAccount {
        SlotAccount {
            account_id: ProviderAccountId::new("acct_slot").unwrap(),
            provider: ProviderKind::new(self.provider).unwrap(),
            has_proxy: self.proxy,
            account_enabled: self.account_enabled,
            slot: Some(ProviderAccountSlot::new(
                ProviderAccountId::new("acct_slot").unwrap(),
                true,
                AccountSlotInstanceId::generate(),
                AccountSlotIdentity::new(
                    "cpr-slot-test".to_owned(),
                    "0123456789abcdef0123456789abcdef".to_owned(),
                    "0199f4c8-52a8-7aa0-a6d7-f75219e82e41".to_owned(),
                    "UTC".to_owned(),
                )
                .unwrap(),
                AccountSlotGeneration::new(1).unwrap(),
            )),
        }
    }
}
#[async_trait]
impl AccountSlotAdminStore for SlotStore {
    async fn read(&self, _: &[ProviderAccountId]) -> AdminStoreResult<Vec<SlotAccount>> {
        Ok(vec![self.account()])
    }
    async fn set(
        &self,
        _: SetAccountSlot,
        _: &MutationContext,
    ) -> AdminStoreResult<AccountSlotMutation> {
        Ok(AccountSlotMutation {
            config_revision: Revision::new(1).unwrap(),
            account: self.account(),
        })
    }
}
struct Runtime {
    enabled: bool,
    invalidated: Mutex<bool>,
}
impl AccountSlotRuntime for Runtime {
    fn route(&self, _: &ProviderAccountId) -> Option<AccountSlotRoute> {
        None
    }
}
impl AccountSlotControl for Runtime {
    fn globally_enabled(&self) -> bool {
        self.enabled
    }
    fn invalidate(&self, _: &ProviderAccountId) {
        *self.invalidated.lock().unwrap() = true;
    }
}

#[tokio::test]
async fn slot_view_distinguishes_global_disable_and_missing_runtime_prerequisites() {
    for (global, proxy, account_enabled, expected) in [
        (false, true, true, AccountSlotState::GlobalDisabled),
        (true, false, true, AccountSlotState::Degraded),
        (true, true, false, AccountSlotState::Degraded),
        (true, true, true, AccountSlotState::Starting),
    ] {
        let service = DefaultAccountSlotsService::new(
            Arc::new(SlotStore {
                provider: "openai",
                proxy,
                account_enabled,
            }),
            Arc::new(Runtime {
                enabled: global,
                invalidated: Mutex::new(false),
            }),
            Arc::new(NoopSnapshot),
        );
        let view = service
            .read(vec![ProviderAccountId::new("acct_slot").unwrap()])
            .await
            .unwrap();
        assert_eq!(view.global_enabled, global);
        assert_eq!(view.items[0].state, expected);
        assert_eq!(
            service.read(vec![]).await.err().unwrap().kind(),
            AdminErrorKind::Invalid
        );
    }
}

#[tokio::test]
async fn slot_mutation_checks_provider_and_proxy_and_invalidates_ready_route() {
    for (provider, proxy, valid) in [
        ("xai", true, false),
        ("openai", false, false),
        ("openai", true, true),
    ] {
        let runtime = Arc::new(Runtime {
            enabled: true,
            invalidated: Mutex::new(false),
        });
        let service = DefaultAccountSlotsService::new(
            Arc::new(SlotStore {
                provider,
                proxy,
                account_enabled: true,
            }),
            runtime.clone(),
            Arc::new(NoopSnapshot),
        );
        let result = service
            .set(
                SetAccountSlot {
                    account_id: ProviderAccountId::new("acct_slot").unwrap(),
                    enabled: true,
                    expected_generation: Some(1),
                },
                &MutationContext {
                    actor: MutationActor::System,
                    request_id: "slot-test".into(),
                },
            )
            .await;
        assert_eq!(result.is_ok(), valid);
        assert_eq!(*runtime.invalidated.lock().unwrap(), valid);
    }
}

#[tokio::test]
async fn container_slot_start_requires_global_docker_capability() {
    let service = DefaultAccountSlotsService::new(
        Arc::new(SlotStore {
            provider: "openai",
            proxy: true,
            account_enabled: true,
        }),
        Arc::new(Runtime {
            enabled: false,
            invalidated: Mutex::new(false),
        }),
        Arc::new(NoopSnapshot),
    );
    let context = MutationContext {
        actor: MutationActor::System,
        request_id: "container-start".into(),
    };
    for command in [
        ContainerSlotCommand {
            id: Some(AccountSlotInstanceId::generate()),
            expected_generation: Some(1),
            action: ContainerSlotAction::Start,
        },
        ContainerSlotCommand {
            id: None,
            expected_generation: None,
            action: ContainerSlotAction::Create { name: "  ".into() },
        },
    ] {
        assert_eq!(
            service
                .mutate_container(command, &context)
                .await
                .err()
                .unwrap()
                .kind(),
            AdminErrorKind::Invalid
        );
    }
}
