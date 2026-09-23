//! 基于 Docker Engine API 的账号槽位资源适配器。

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bollard::errors::Error as DockerError;
use bollard::models::{
    ContainerCreateBody, ContainerSummaryStateEnum, HostConfig, Ipam, IpamConfig,
    NetworkConnectRequest, NetworkCreateRequest, NetworkDisconnectRequest, RestartPolicy,
    RestartPolicyNameEnum, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, DownloadFromContainerOptionsBuilder,
    ListContainersOptionsBuilder, RemoveContainerOptionsBuilder, StopContainerOptionsBuilder,
    UploadToContainerOptionsBuilder,
};
use bollard::{API_DEFAULT_VERSION, Docker};
use futures::StreamExt;
use gateway_core::account::{
    AccountSlotBearerToken, AccountSlotGeneration, AccountSlotInstanceId, AccountSlotRoute,
};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use tar::{Archive, Builder, EntryType, Header};
use uuid::Uuid;

use crate::config::OpenAiSlotsConfig;

use super::egress::{CommandIptables, ManagedSlotEgress, SlotEgressLifecycle, net};
use super::{
    AccountSlotEngine, AccountSlotEngineError, AccountSlotEngineErrorKind, AccountSlotHealth,
    AccountSlotRuntimeState, ConvergedAccountSlot, DesiredAccountSlot,
};

const OWNER_LABEL: &str = "io.codex-proxy-rs.owner";
const OWNER_VALUE: &str = "openai-account-slot";
const INSTANCE_LABEL: &str = "io.codex-proxy-rs.slot.instance";
const GENERATION_LABEL: &str = "io.codex-proxy-rs.slot.generation";
const IMAGE_LABEL: &str = "io.codex-proxy-rs.slot.image";
const SLOT_ROOT: &str = "/var/lib/cpr-slot";
const AUTH_PATH: &str = "/var/lib/cpr-slot/secrets/auth";
const PROXY_PATH: &str = "/var/lib/cpr-slot/secrets/proxy";
const SIDECAR_PORT: u16 = 8090;

/// 只持有非敏感配置；代理 URL 和 bearer token 不进入结构化日志。
pub struct BollardAccountSlotEngine {
    docker: Docker,
    config: OpenAiSlotsConfig,
    gateway_container: String,
    health_client: reqwest::Client,
    egress: Arc<dyn SlotEgressLifecycle>,
}

impl std::fmt::Debug for BollardAccountSlotEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BollardAccountSlotEngine")
            .field("image", &self.config.image)
            .field("docker_endpoint", &self.config.docker_endpoint)
            .field("gateway_container", &self.gateway_container)
            .finish_non_exhaustive()
    }
}

impl BollardAccountSlotEngine {
    /// 连接受控 Docker socket。gateway container ID 通常来自容器内的 `HOSTNAME`。
    ///
    /// # Errors
    ///
    /// endpoint、gateway container ID 或健康检查客户端无效时返回错误。
    pub fn connect(
        config: OpenAiSlotsConfig,
        gateway_container: impl Into<String>,
    ) -> Result<Self, AccountSlotEngineError> {
        let egress = Arc::new(ManagedSlotEgress::new(Arc::new(CommandIptables)));
        Self::connect_with_egress(config, gateway_container, egress)
    }

    /// 使用调用方提供的出网控制器连接 Docker；用于没有 Linux 网桥的集成测试。
    pub fn connect_with_egress(
        config: OpenAiSlotsConfig,
        gateway_container: impl Into<String>,
        egress: Arc<dyn SlotEgressLifecycle>,
    ) -> Result<Self, AccountSlotEngineError> {
        let gateway_container = gateway_container.into();
        if gateway_container.trim().is_empty() {
            return Err(engine_error(AccountSlotEngineErrorKind::InvalidState));
        }
        let docker = Docker::connect_with_socket(
            &config.docker_endpoint,
            config.start_timeout_seconds,
            API_DEFAULT_VERSION,
        )
        .map_err(|_| engine_error(AccountSlotEngineErrorKind::Unavailable))?;
        let health_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
        Ok(Self {
            docker,
            config,
            gateway_container,
            health_client,
            egress,
        })
    }

    async fn ensure_volume(
        &self,
        names: &SlotResourceNames,
        instance_id: AccountSlotInstanceId,
    ) -> Result<(), AccountSlotEngineError> {
        match self.docker.inspect_volume(&names.volume).await {
            Ok(volume) => verify_resource_labels(&volume.labels, instance_id),
            Err(error) if is_not_found(&error) => {
                self.docker
                    .create_volume(VolumeCreateRequest {
                        name: Some(names.volume.clone()),
                        labels: Some(base_labels(instance_id)),
                        ..Default::default()
                    })
                    .await
                    .map_err(map_docker_error)?;
                Ok(())
            }
            Err(error) => Err(map_docker_error(error)),
        }
    }

    async fn ensure_network(
        &self,
        names: &SlotResourceNames,
        instance_id: AccountSlotInstanceId,
    ) -> Result<(), AccountSlotEngineError> {
        let network = match self.docker.inspect_network(&names.network, None).await {
            Ok(network) => network,
            Err(error) if is_not_found(&error) => {
                self.docker
                    .create_network(NetworkCreateRequest {
                        name: names.network.clone(),
                        driver: Some("bridge".to_owned()),
                        internal: Some(true),
                        attachable: Some(false),
                        options: Some(HashMap::from([(
                            "com.docker.network.bridge.name".to_owned(),
                            net::bridge_name(instance_id),
                        )])),
                        ipam: Some(Ipam {
                            config: Some(vec![IpamConfig {
                                subnet: Some(net::subnet_for(instance_id)),
                                ..Default::default()
                            }]),
                            ..Default::default()
                        }),
                        labels: Some(base_labels(instance_id)),
                        ..Default::default()
                    })
                    .await
                    .map_err(map_docker_error)?;
                self.docker
                    .inspect_network(&names.network, None)
                    .await
                    .map_err(map_docker_error)?
            }
            Err(error) => return Err(map_docker_error(error)),
        };
        verify_resource_labels(
            network.labels.as_ref().unwrap_or(&HashMap::new()),
            instance_id,
        )?;
        verify_network_config(&network, instance_id)?;

        let gateway_attached = network.containers.as_ref().is_some_and(|containers| {
            let gateway = self.gateway_container.trim_start_matches('/');
            // HOSTNAME 默认是短容器 ID；Docker 网络返回的 key 是完整 ID。
            let short_id = gateway.len() >= 12
                && gateway.len() <= 64
                && gateway.bytes().all(|byte| byte.is_ascii_hexdigit());
            containers.iter().any(|(id, endpoint)| {
                id == gateway
                    || (short_id && id.starts_with(gateway))
                    || endpoint.name.as_deref() == Some(gateway)
            })
        });
        if !gateway_attached {
            self.docker
                .connect_network(
                    &names.network,
                    NetworkConnectRequest {
                        container: self.gateway_container.clone(),
                        endpoint_config: None,
                    },
                )
                .await
                .map_err(map_docker_error)?;
        }
        Ok(())
    }

    async fn create_container(
        &self,
        names: &SlotResourceNames,
        desired: &DesiredAccountSlot,
        token: &[u8],
    ) -> Result<(), AccountSlotEngineError> {
        let labels = desired_labels(desired, &self.config.image);
        let memory = i64::try_from(self.config.memory_limit_mb.saturating_mul(1024 * 1024))
            .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
        let nano_cpus = i64::try_from(self.config.cpu_limit_millis.saturating_mul(1_000_000))
            .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
        let pids_limit = i64::try_from(self.config.pids_limit)
            .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
        let body = ContainerCreateBody {
            hostname: Some(desired.hostname.clone()),
            image: Some(self.config.image.clone()),
            env: Some(vec![
                format!("HOME={SLOT_ROOT}/home"),
                format!("TZ={}", desired.timezone),
                "CPR_SLOT_LISTEN=0.0.0.0:8090".to_owned(),
                format!("CPR_SLOT_AUTH_FILE={AUTH_PATH}"),
                format!("CPR_SLOT_PROXY_FILE={PROXY_PATH}"),
                format!("CPR_INSTALLATION_ID={}", desired.installation_id),
            ]),
            labels: Some(labels),
            exposed_ports: Some(vec![format!("{SIDECAR_PORT}/tcp")]),
            host_config: Some(HostConfig {
                memory: Some(memory),
                nano_cpus: Some(nano_cpus),
                pids_limit: Some(pids_limit),
                binds: Some(vec![format!("{}:{SLOT_ROOT}:rw", names.volume)]),
                network_mode: Some(names.network.clone()),
                restart_policy: Some(RestartPolicy {
                    name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                }),
                cap_drop: Some(vec!["ALL".to_owned()]),
                security_opt: Some(vec!["no-new-privileges:true".to_owned()]),
                privileged: Some(false),
                publish_all_ports: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(&names.container)
                        .build(),
                ),
                body,
            )
            .await
            .map_err(map_docker_error)?;
        let archive = build_slot_archive(desired, token)?;
        self.docker
            .upload_to_container(
                &names.container,
                Some(UploadToContainerOptionsBuilder::default().path("/").build()),
                bollard::body_full(archive.into()),
            )
            .await
            .map_err(map_docker_error)?;
        Ok(())
    }

    async fn remove_owned_container(
        &self,
        names: &SlotResourceNames,
        instance_id: AccountSlotInstanceId,
    ) -> Result<(), AccountSlotEngineError> {
        let Some(inspect) = self.inspect_owned_container(names, instance_id).await? else {
            return Ok(());
        };
        if inspect.state.as_ref().and_then(|state| state.running) == Some(true) {
            self.docker
                .stop_container(
                    &names.container,
                    Some(StopContainerOptionsBuilder::default().t(10).build()),
                )
                .await
                .map_err(map_docker_error)?;
        }
        self.docker
            .remove_container(
                &names.container,
                Some(
                    RemoveContainerOptionsBuilder::default()
                        .force(false)
                        .v(false)
                        .build(),
                ),
            )
            .await
            .map_err(map_docker_error)
    }

    async fn inspect_owned_container(
        &self,
        names: &SlotResourceNames,
        instance_id: AccountSlotInstanceId,
    ) -> Result<Option<bollard::models::ContainerInspectResponse>, AccountSlotEngineError> {
        match self.docker.inspect_container(&names.container, None).await {
            Ok(inspect) => {
                let labels = inspect
                    .config
                    .as_ref()
                    .and_then(|config| config.labels.as_ref())
                    .ok_or_else(|| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
                verify_resource_labels(labels, instance_id)?;
                Ok(Some(inspect))
            }
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(map_docker_error(error)),
        }
    }

    async fn read_token(&self, container: &str) -> Result<Vec<u8>, AccountSlotEngineError> {
        let options = DownloadFromContainerOptionsBuilder::default()
            .path(AUTH_PATH)
            .build();
        let mut stream = self
            .docker
            .download_from_container(container, Some(options));
        let mut archive_bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_docker_error)?;
            if archive_bytes.len().saturating_add(chunk.len()) > 65_536 {
                return Err(engine_error(AccountSlotEngineErrorKind::InvalidState));
            }
            archive_bytes.extend_from_slice(&chunk);
        }
        read_only_file_from_archive(&archive_bytes)
    }

    async fn wait_until_ready(
        &self,
        names: &SlotResourceNames,
        token: &AccountSlotBearerToken,
    ) -> Result<bool, AccountSlotEngineError> {
        let endpoint = slot_endpoint(names)?;
        let ready_url = endpoint
            .join("/readyz")
            .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
        let mut authorization = Vec::with_capacity(7 + token.expose_to_provider().len());
        authorization.extend_from_slice(b"Bearer ");
        authorization.extend_from_slice(token.expose_to_provider());
        let authorization = HeaderValue::from_bytes(&authorization)
            .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
        let deadline = Instant::now() + self.config.start_timeout();
        while Instant::now() < deadline {
            let result = self
                .health_client
                .get(ready_url.clone())
                .header(AUTHORIZATION, authorization.clone())
                .send()
                .await;
            if result.is_ok_and(|response| response.status().is_success()) {
                return Ok(true);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(false)
    }
}

#[async_trait]
impl AccountSlotEngine for BollardAccountSlotEngine {
    async fn delete(
        &self,
        instance_id: AccountSlotInstanceId,
    ) -> Result<(), AccountSlotEngineError> {
        let names = SlotResourceNames::new(instance_id);
        // 先检查全部资源和网络端点，再执行破坏性操作；同名外部资源不能被删除。
        let container = self.inspect_owned_container(&names, instance_id).await?;
        let volume = match self.docker.inspect_volume(&names.volume).await {
            Ok(volume) => {
                verify_resource_labels(&volume.labels, instance_id)?;
                Some(volume)
            }
            Err(error) if is_not_found(&error) => None,
            Err(error) => return Err(map_docker_error(error)),
        };
        let network = match self.docker.inspect_network(&names.network, None).await {
            Ok(network) => {
                verify_resource_labels(
                    network.labels.as_ref().unwrap_or(&HashMap::new()),
                    instance_id,
                )?;
                verify_network_config(&network, instance_id)?;
                Some(network)
            }
            Err(error) if is_not_found(&error) => None,
            Err(error) => return Err(map_docker_error(error)),
        };
        let mut gateway_id = None;
        if let Some(network) = &network {
            let gateway = self
                .docker
                .inspect_container(&self.gateway_container, None)
                .await
                .map_err(map_docker_error)?;
            let id = gateway
                .id
                .ok_or_else(|| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
            for endpoint in network
                .containers
                .iter()
                .flat_map(|endpoints| endpoints.keys())
            {
                if endpoint == &id {
                    gateway_id = Some(id.clone());
                } else if container
                    .as_ref()
                    .and_then(|container| container.id.as_ref())
                    != Some(endpoint)
                {
                    return Err(engine_error(AccountSlotEngineErrorKind::InvalidState));
                }
            }
        }
        // 所有资源归属和端点检查都通过后，先拆除出网规则，再移除容器与网桥。
        self.egress
            .remove(instance_id)
            .await
            .map_err(map_egress_error)?;
        self.remove_owned_container(&names, instance_id).await?;
        if network.is_some() {
            if let Some(gateway) = gateway_id {
                self.docker
                    .disconnect_network(
                        &names.network,
                        NetworkDisconnectRequest {
                            container: gateway,
                            force: Some(false),
                        },
                    )
                    .await
                    .or_else(ignore_not_found)
                    .map_err(map_docker_error)?;
            }
            self.docker
                .remove_network(&names.network)
                .await
                .or_else(ignore_not_found)
                .map_err(map_docker_error)?;
        }
        if volume.is_some() {
            // 不强删占用中的卷，清理失败保留删除意图以便重试。
            self.docker
                .remove_volume(
                    &names.volume,
                    Some(
                        bollard::query_parameters::RemoveVolumeOptionsBuilder::default()
                            .force(false)
                            .build(),
                    ),
                )
                .await
                .or_else(ignore_not_found)
                .map_err(map_docker_error)?;
        }
        Ok(())
    }

    async fn list_owned(&self) -> Result<Vec<AccountSlotHealth>, AccountSlotEngineError> {
        // 首轮对账即使没有 desired slot，也要先清理上一进程留下的内核规则。
        self.egress
            .cleanup_stale()
            .await
            .map_err(map_egress_error)?;
        let filters = HashMap::from([(
            "label".to_owned(),
            vec![format!("{OWNER_LABEL}={OWNER_VALUE}")],
        )]);
        let containers = self
            .docker
            .list_containers(Some(
                ListContainersOptionsBuilder::default()
                    .all(true)
                    .filters(&filters)
                    .build(),
            ))
            .await
            .map_err(map_docker_error)?;
        containers
            .into_iter()
            .map(|container| {
                let labels = container
                    .labels
                    .ok_or_else(|| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
                health_from_labels(
                    &labels,
                    container.state == Some(ContainerSummaryStateEnum::RUNNING),
                )
            })
            .collect()
    }

    async fn inspect(
        &self,
        instance_id: AccountSlotInstanceId,
    ) -> Result<Option<AccountSlotHealth>, AccountSlotEngineError> {
        let names = SlotResourceNames::new(instance_id);
        let Some(inspect) = self.inspect_owned_container(&names, instance_id).await? else {
            return Ok(None);
        };
        let labels = inspect
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .ok_or_else(|| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
        let running = inspect.state.as_ref().and_then(|state| state.running) == Some(true);
        health_from_labels(labels, running).map(Some)
    }

    async fn converge(
        &self,
        desired: &DesiredAccountSlot,
    ) -> Result<ConvergedAccountSlot, AccountSlotEngineError> {
        let names = SlotResourceNames::new(desired.instance_id);
        self.ensure_volume(&names, desired.instance_id).await?;
        self.ensure_network(&names, desired.instance_id).await?;
        self.egress
            .ensure(desired.instance_id, &desired.outbound_proxy)
            .await
            .map_err(map_egress_error)?;
        self.egress
            .apply_rules(desired.instance_id)
            .await
            .map_err(map_egress_error)?;

        let existing = self
            .inspect_owned_container(&names, desired.instance_id)
            .await?;
        let matches_desired = existing.as_ref().is_some_and(|inspect| {
            inspect.config.as_ref().is_some_and(|config| {
                config.labels.as_ref().is_some_and(|labels| {
                    labels.get(GENERATION_LABEL) == Some(&desired.generation.get().to_string())
                        && labels.get(IMAGE_LABEL) == Some(&self.config.image)
                })
            })
        });
        if existing.is_some() && !matches_desired {
            self.remove_owned_container(&names, desired.instance_id)
                .await?;
        }

        let token_bytes = if matches_desired {
            match self.read_token(&names.container).await {
                Ok(token) => token,
                Err(_) => {
                    self.remove_owned_container(&names, desired.instance_id)
                        .await?;
                    let token = generate_token();
                    self.create_container(&names, desired, &token).await?;
                    token
                }
            }
        } else {
            let token = generate_token();
            self.create_container(&names, desired, &token).await?;
            token
        };
        let token = AccountSlotBearerToken::new(token_bytes)
            .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;

        let inspect = self
            .inspect_owned_container(&names, desired.instance_id)
            .await?
            .ok_or_else(|| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
        if inspect.state.as_ref().and_then(|state| state.running) != Some(true) {
            self.docker
                .start_container(&names.container, None)
                .await
                .map_err(map_docker_error)?;
        }
        let ready = self.wait_until_ready(&names, &token).await?;
        let state = if ready {
            AccountSlotRuntimeState::Ready
        } else {
            AccountSlotRuntimeState::Degraded
        };
        Ok(ConvergedAccountSlot {
            health: AccountSlotHealth {
                instance_id: desired.instance_id,
                generation: desired.generation,
                state,
                reason: (!ready).then_some("sidecar readiness check failed"),
            },
            route: if ready {
                Some(AccountSlotRoute::new(slot_endpoint(&names)?, token))
            } else {
                None
            },
        })
    }

    async fn stop(&self, instance_id: AccountSlotInstanceId) -> Result<(), AccountSlotEngineError> {
        let names = SlotResourceNames::new(instance_id);
        let Some(inspect) = self.inspect_owned_container(&names, instance_id).await? else {
            return Ok(());
        };
        if inspect.state.as_ref().and_then(|state| state.running) == Some(true) {
            self.docker
                .stop_container(
                    &names.container,
                    Some(StopContainerOptionsBuilder::default().t(10).build()),
                )
                .await
                .map_err(map_docker_error)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlotResourceNames {
    container: String,
    volume: String,
    network: String,
}

impl SlotResourceNames {
    fn new(instance_id: AccountSlotInstanceId) -> Self {
        let suffix = instance_id.uuid().simple();
        Self {
            container: format!("cpr-slot-{suffix}"),
            volume: format!("cpr-slot-{suffix}-data"),
            network: format!("cpr-slot-{suffix}-net"),
        }
    }
}

fn base_labels(instance_id: AccountSlotInstanceId) -> HashMap<String, String> {
    HashMap::from([
        (OWNER_LABEL.to_owned(), OWNER_VALUE.to_owned()),
        (INSTANCE_LABEL.to_owned(), instance_id.to_string()),
    ])
}

fn desired_labels(desired: &DesiredAccountSlot, image: &str) -> HashMap<String, String> {
    let mut labels = base_labels(desired.instance_id);
    labels.insert(
        GENERATION_LABEL.to_owned(),
        desired.generation.get().to_string(),
    );
    labels.insert(IMAGE_LABEL.to_owned(), image.to_owned());
    labels
}

fn verify_resource_labels(
    labels: &HashMap<String, String>,
    instance_id: AccountSlotInstanceId,
) -> Result<(), AccountSlotEngineError> {
    if labels.get(OWNER_LABEL).map(String::as_str) != Some(OWNER_VALUE)
        || labels.get(INSTANCE_LABEL) != Some(&instance_id.to_string())
    {
        return Err(engine_error(AccountSlotEngineErrorKind::Unauthorized));
    }
    Ok(())
}

fn verify_network_config(
    network: &bollard::models::NetworkInspect,
    instance_id: AccountSlotInstanceId,
) -> Result<(), AccountSlotEngineError> {
    let expected_bridge = net::bridge_name(instance_id);
    let expected_subnet = net::subnet_for(instance_id);
    let bridge_matches = network
        .options
        .as_ref()
        .and_then(|options| options.get("com.docker.network.bridge.name"))
        .map(String::as_str)
        == Some(expected_bridge.as_str());
    let subnet_matches = network
        .ipam
        .as_ref()
        .and_then(|ipam| ipam.config.as_ref())
        .is_some_and(|configs| {
            configs
                .iter()
                .any(|config| config.subnet.as_deref() == Some(expected_subnet.as_str()))
        });
    if network.driver.as_deref() != Some("bridge")
        || network.internal != Some(true)
        || !bridge_matches
        || !subnet_matches
    {
        return Err(engine_error(AccountSlotEngineErrorKind::InvalidState));
    }
    Ok(())
}

fn health_from_labels(
    labels: &HashMap<String, String>,
    running: bool,
) -> Result<AccountSlotHealth, AccountSlotEngineError> {
    if labels.get(OWNER_LABEL).map(String::as_str) != Some(OWNER_VALUE) {
        return Err(engine_error(AccountSlotEngineErrorKind::Unauthorized));
    }
    let instance_label = labels
        .get(INSTANCE_LABEL)
        .ok_or_else(|| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
    let instance_id = AccountSlotInstanceId::parse(instance_label)
        .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
    let generation = labels
        .get(GENERATION_LABEL)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(AccountSlotGeneration::new)
        .ok_or_else(|| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
    Ok(AccountSlotHealth {
        instance_id,
        generation,
        state: if running {
            AccountSlotRuntimeState::Starting
        } else {
            AccountSlotRuntimeState::Stopped
        },
        reason: None,
    })
}

fn slot_endpoint(names: &SlotResourceNames) -> Result<reqwest::Url, AccountSlotEngineError> {
    reqwest::Url::parse(&format!(
        "http://{}:{SIDECAR_PORT}/internal/v1/forward",
        names.container
    ))
    .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))
}

fn generate_token() -> Vec<u8> {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()).into_bytes()
}

fn build_slot_archive(
    desired: &DesiredAccountSlot,
    token: &[u8],
) -> Result<Vec<u8>, AccountSlotEngineError> {
    let mut builder = Builder::new(Vec::new());
    append_directory(&mut builder, "var/lib/cpr-slot/secrets")?;
    append_directory(&mut builder, "var/lib/cpr-slot/home")?;
    append_directory(&mut builder, "var/lib/cpr-slot/identity")?;
    append_file(&mut builder, "var/lib/cpr-slot/secrets/auth", token, 0o600)?;
    append_file(
        &mut builder,
        "var/lib/cpr-slot/secrets/proxy",
        desired.outbound_proxy.expose_url().as_bytes(),
        0o600,
    )?;
    append_file(
        &mut builder,
        "var/lib/cpr-slot/identity/installation-id",
        desired.installation_id.as_bytes(),
        0o600,
    )?;
    let machine_id = format!("{}\n", desired.machine_id);
    append_file(&mut builder, "etc/machine-id", machine_id.as_bytes(), 0o444)?;
    builder
        .into_inner()
        .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))
}

fn append_directory(
    builder: &mut Builder<Vec<u8>>,
    path: &str,
) -> Result<(), AccountSlotEngineError> {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Directory);
    header.set_mode(0o700);
    header.set_uid(0);
    header.set_gid(0);
    header.set_size(0);
    header.set_cksum();
    builder
        .append_data(&mut header, path, std::io::empty())
        .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))
}

fn append_file(
    builder: &mut Builder<Vec<u8>>,
    path: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<(), AccountSlotEngineError> {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    builder
        .append_data(&mut header, path, bytes)
        .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))
}

fn read_only_file_from_archive(bytes: &[u8]) -> Result<Vec<u8>, AccountSlotEngineError> {
    let mut archive = Archive::new(Cursor::new(bytes));
    let mut entries = archive
        .entries()
        .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
    let mut entry = entries
        .next()
        .ok_or_else(|| engine_error(AccountSlotEngineErrorKind::InvalidState))?
        .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
    if entry.header().entry_type() != EntryType::Regular || entry.size() > 4096 {
        return Err(engine_error(AccountSlotEngineErrorKind::InvalidState));
    }
    let mut value = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut value)
        .map_err(|_| engine_error(AccountSlotEngineErrorKind::InvalidState))?;
    Ok(value)
}

fn ignore_not_found(error: DockerError) -> Result<(), DockerError> {
    if is_not_found(&error) {
        Ok(())
    } else {
        Err(error)
    }
}

fn is_not_found(error: &DockerError) -> bool {
    matches!(
        error,
        DockerError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

fn map_docker_error(error: DockerError) -> AccountSlotEngineError {
    match error {
        DockerError::DockerResponseServerError {
            status_code: 401 | 403,
            ..
        } => engine_error(AccountSlotEngineErrorKind::Unauthorized),
        DockerError::DockerResponseServerError {
            status_code: 400 | 404 | 409,
            ..
        } => engine_error(AccountSlotEngineErrorKind::InvalidState),
        _ => engine_error(AccountSlotEngineErrorKind::Unavailable),
    }
}

fn map_egress_error(_: super::egress::EgressError) -> AccountSlotEngineError {
    engine_error(AccountSlotEngineErrorKind::Unavailable)
}

const fn engine_error(kind: AccountSlotEngineErrorKind) -> AccountSlotEngineError {
    AccountSlotEngineError { kind }
}
