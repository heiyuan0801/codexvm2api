use async_trait::async_trait;
use gateway_core::account::*;
use gateway_core::error::{StoreError, StoreErrorKind};
use gateway_core::routing::ProviderKind;
use gateway_host::config::OpenAiSlotsConfig;
use gateway_host::slots::{
    AccountSlotDesiredStateSource, StoredAccountSlotDesiredStateSource, initialize_account_slots,
};
use std::sync::Arc;

struct MemoryStores {
    slots: Vec<ProviderAccountSlot>,
    accounts: Vec<ProviderAccount>,
    fail_slots: bool,
}

#[async_trait]
impl ProviderAccountSlotStore for MemoryStores {
    async fn pending_slot_deletions(&self) -> Result<Vec<AccountSlotInstanceId>, StoreError> {
        Ok(vec![])
    }
    async fn complete_slot_deletion(&self, _id: AccountSlotInstanceId) -> Result<(), StoreError> {
        unreachable!("no pending deletions")
    }

    async fn get_account_slot(
        &self,
        id: &ProviderAccountId,
    ) -> Result<Option<ProviderAccountSlot>, StoreError> {
        Ok(self
            .slots
            .iter()
            .find(|slot| slot.account_id() == id)
            .cloned())
    }
    async fn list_account_slots(&self) -> Result<Vec<ProviderAccountSlot>, StoreError> {
        if self.fail_slots {
            return Err(StoreError::new(StoreErrorKind::Unavailable));
        }
        Ok(self.slots.clone())
    }
}

#[async_trait]
impl ProviderAccountStore for MemoryStores {
    async fn create_account(&self, _account: NewProviderAccount) -> Result<(), StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn get_account(
        &self,
        _account: &ProviderAccountId,
    ) -> Result<Option<ProviderAccount>, StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn list_accounts(&self) -> Result<Vec<ProviderAccount>, StoreError> {
        Ok(self.accounts.clone())
    }
    async fn list_for_provider(
        &self,
        _provider: &ProviderKind,
    ) -> Result<Vec<ProviderAccount>, StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn list_refresh_candidates(
        &self,
        _query: ProviderRefreshQuery,
    ) -> Result<Vec<LoadedCredential>, StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn load_credential(
        &self,
        _account: &ProviderAccountId,
        _expected_revision: CredentialRevision,
    ) -> Result<LoadedCredential, StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn load_current_credential(
        &self,
        _account: &ProviderAccountId,
    ) -> Result<LoadedCredential, StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn compare_and_swap_credential(
        &self,
        _update: CredentialCasUpdate,
    ) -> Result<CredentialCasOutcome, StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn get_quotas(
        &self,
        _accounts: &[ProviderAccountId],
    ) -> Result<Vec<QuotaObservation>, StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn compare_and_swap_quota(
        &self,
        _observation: QuotaObservation,
    ) -> Result<QuotaWriteOutcome, StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn touch_quota_observation(
        &self,
        _touch: QuotaObservationTouch,
    ) -> Result<QuotaWriteOutcome, StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn apply_quota_access(
        &self,
        _change: QuotaAccessChange,
    ) -> Result<QuotaWriteOutcome, StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn apply_state_change(&self, _change: AccountStateChange) -> Result<(), StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn update_account(&self, _update: ProviderAccountUpdate) -> Result<(), StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn set_enabled(
        &self,
        _account: &ProviderAccountId,
        _enabled: bool,
    ) -> Result<(), StoreError> {
        unreachable!("unexpected account store method")
    }
    async fn delete_account(&self, _account: &ProviderAccountId) -> Result<(), StoreError> {
        unreachable!("unexpected account store method")
    }
}

fn account(id: &str, provider: &str, proxy: bool) -> ProviderAccount {
    ProviderAccount::new(
        ProviderAccountId::new(id).unwrap(),
        ProviderKind::new(provider).unwrap(),
        "test".to_owned(),
        None,
        "oauth".to_owned(),
        CredentialRevision::new(1).unwrap(),
        None,
    )
    .with_outbound_proxy(
        proxy.then(|| OutboundProxy::parse("socks5://user:pass@127.0.0.1:1080").unwrap()),
    )
}

fn slot(id: &str, enabled: bool) -> ProviderAccountSlot {
    ProviderAccountSlot::new(
        ProviderAccountId::new(id).unwrap(),
        enabled,
        AccountSlotInstanceId::generate(),
        AccountSlotIdentity::new(
            "slot-test".to_owned(),
            "0123456789abcdef0123456789abcdef".to_owned(),
            "00000000-0000-4000-8000-000000000001".to_owned(),
            "UTC".to_owned(),
        )
        .unwrap(),
        AccountSlotGeneration::new(3).unwrap(),
    )
    .with_outbound_proxy(
        (id != "acct_no_proxy").then(|| OutboundProxy::parse("http://slot-proxy:3128").unwrap()),
    )
}

#[tokio::test]
async fn desired_state_requires_openai_intent_and_current_proxy() {
    let expected = slot("acct_ready", true);
    let stores = Arc::new(MemoryStores {
        slots: vec![
            expected.clone(),
            slot("acct_off", false),
            slot("acct_no_proxy", true),
            slot("acct_xai", true),
            slot("acct_deleted", true),
            slot("acct_disabled", true),
        ],
        accounts: vec![
            account("acct_ready", "openai", true),
            account("acct_off", "openai", true),
            account("acct_no_proxy", "openai", false),
            account("acct_xai", "xai", true),
            account("acct_disabled", "openai", true).with_account_facts(
                false,
                CredentialState::Ready,
                QuotaState::unknown(),
                None,
                None,
            ),
        ],
        fail_slots: false,
    });
    let source = StoredAccountSlotDesiredStateSource::new(stores.clone(), stores);
    let desired = source.list_desired_slots().await.unwrap();
    assert_eq!(desired.len(), 1);
    assert_eq!(
        desired[0].outbound_proxy,
        OutboundProxy::parse("http://slot-proxy:3128").unwrap()
    );
    assert_eq!(&desired[0].account_id, expected.account_id());
    assert_eq!(desired[0].generation, expected.generation());
    assert_eq!(desired[0].instance_id, expected.instance_id());
    assert_eq!(
        desired[0].installation_id,
        expected.identity().installation_id()
    );
    let debug = format!("{:?}", desired[0]);
    assert!(!debug.contains("user:pass"));
    assert!(!debug.contains("acct_ready"));
}

#[tokio::test]
async fn desired_state_store_failure_is_not_an_empty_success() {
    let stores = Arc::new(MemoryStores {
        slots: vec![],
        accounts: vec![],
        fail_slots: true,
    });
    let source = StoredAccountSlotDesiredStateSource::new(stores.clone(), stores);
    assert!(source.list_desired_slots().await.is_err());
}

#[test]
fn disabled_bundle_does_not_connect_docker_or_read_stores() {
    let stores = Arc::new(MemoryStores {
        slots: vec![],
        accounts: vec![],
        fail_slots: true,
    });
    let config = OpenAiSlotsConfig {
        enabled: false,
        docker_endpoint: "not-a-docker-endpoint".to_owned(),
        gateway_container: Some(String::new()),
        ..OpenAiSlotsConfig::default()
    };
    let bundle = initialize_account_slots(config, stores.clone(), stores).unwrap();
    assert!(
        bundle
            .registry
            .route(&ProviderAccountId::new("acct_ready").unwrap())
            .is_none()
    );
    assert!(matches!(
        bundle.worker,
        gateway_core::task::WorkerContribution::Disabled {
            kind: gateway_core::task::WorkerKind::AccountSlotReconciliation,
            ..
        }
    ));
}
