use super::*;
use gateway_host::{config::OpenAiSlotsConfig, slots::BollardAccountSlotEngine};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    image: String,
    instances: [String; 2],
}

#[tokio::test]
#[ignore = "requires the isolated Docker fixture from deploy/tests/verify-account-slot-host.py"]
async fn real_docker_slot_lifecycle() {
    let fixture: Fixture =
        serde_json::from_slice(&std::fs::read("/fixture.json").unwrap()).unwrap();
    let engine = BollardAccountSlotEngine::connect(
        OpenAiSlotsConfig {
            enabled: true,
            image: fixture.image,
            start_timeout_seconds: 15,
            ..OpenAiSlotsConfig::default()
        },
        std::env::var("HOSTNAME").unwrap(),
    )
    .unwrap();
    let mut first = desired("acct_smoke_a", &fixture.instances[0]);
    let mut second = desired("acct_smoke_b", &fixture.instances[1]);
    first.hostname = format!("cpr-smoke-{}", &fixture.instances[0][..8]);
    second.hostname = format!("cpr-smoke-{}", &fixture.instances[1][..8]);
    second.machine_id = "abcdef0123456789abcdef0123456789".to_owned();
    second.installation_id = uuid::Uuid::new_v4().to_string();
    first.outbound_proxy =
        OutboundProxy::parse("http://synthetic-a:synthetic-pass-a@proxy-a.invalid:3128").unwrap();
    second.outbound_proxy =
        OutboundProxy::parse("http://synthetic-b:synthetic-pass-b@proxy-b.invalid:3128").unwrap();
    let ready_a = engine.converge(&first).await.expect("first converge");
    let ready_b = engine.converge(&second).await.expect("second converge");
    assert_eq!(ready_a.health.state, AccountSlotRuntimeState::Ready);
    assert_eq!(ready_b.health.state, AccountSlotRuntimeState::Ready);
    assert_ne!(
        ready_a.route.as_ref().unwrap().endpoint(),
        ready_b.route.as_ref().unwrap().endpoint()
    );
    let repeated = engine.converge(&first).await.expect("idempotent converge");
    assert_eq!(ready_a.route, repeated.route);
    first.generation = AccountSlotGeneration::new(2).unwrap();
    first.outbound_proxy =
        OutboundProxy::parse("http://synthetic-c:synthetic-pass-c@proxy-c.invalid:3128").unwrap();
    let rebuilt = engine
        .converge(&first)
        .await
        .expect("changed proxy converge");
    assert_eq!(rebuilt.health.state, AccountSlotRuntimeState::Ready);
    assert_eq!(rebuilt.health.generation.get(), 2);
    assert_ne!(
        rebuilt.route.as_ref().unwrap().bearer_token(),
        ready_a.route.as_ref().unwrap().bearer_token()
    );
    assert_eq!(engine.converge(&second).await.unwrap().route, ready_b.route);
    engine.stop(first.instance_id).await.expect("stop first");
    assert_eq!(
        engine
            .inspect(first.instance_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        AccountSlotRuntimeState::Stopped
    );
    assert_eq!(
        engine.converge(&second).await.unwrap().health.state,
        AccountSlotRuntimeState::Ready
    );
    let docker = bollard::Docker::connect_with_local_defaults().unwrap();
    let stem = format!("cpr-slot-{}", first.instance_id.uuid().simple());
    let stopped = docker.inspect_container(&stem, None).await.unwrap();
    assert_eq!(stopped.state.unwrap().exit_code, Some(0));
    engine
        .delete(first.instance_id)
        .await
        .expect("delete first");
    engine
        .delete(first.instance_id)
        .await
        .expect("idempotent delete");
    assert!(engine.inspect(first.instance_id).await.unwrap().is_none());
    assert!(matches!(
        docker.inspect_network(&format!("{stem}-net"), None).await,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            ..
        })
    ));
    assert!(matches!(
        docker.inspect_volume(&format!("{stem}-data")).await,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            ..
        })
    ));
    assert_eq!(
        engine.converge(&second).await.unwrap().health.state,
        AccountSlotRuntimeState::Ready
    );
    engine.stop(second.instance_id).await.expect("stop second");
}
