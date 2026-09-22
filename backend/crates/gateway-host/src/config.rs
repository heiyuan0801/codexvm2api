//! 配置文件发现、反序列化与 Host-owned 配置校验。

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing_subscriber::EnvFilter;

use crate::system_update::SystemUpdateConfig;

const CONFIG_RELATIVE_PATH: &str = "deploy/config.yaml";
const SERVER_HOST_ENV: &str = "CPR_SERVER_HOST";
const SERVER_PORT_ENV: &str = "CPR_SERVER_PORT";
const SLOTS_ENABLED_ENV: &str = "CPR_OPENAI_SLOTS_ENABLED";
const SLOTS_IMAGE_ENV: &str = "CPR_OPENAI_SLOTS_IMAGE";

/// 由组装根实现的顶层配置契约。
///
/// Host 只负责找到和解析文件；每个包的字段解释与相对路径解析
/// 由顶层配置委托给对应的包完成。
pub trait LoadableConfig: DeserializeOwned {
    /// 同一配置文件中由其他工具消费的顶层配置段。
    const EXTERNAL_SECTIONS: &'static [&'static str] = &[];

    fn resolve_and_validate(&mut self, source_dir: &Path) -> Result<(), ConfigError>;
}

/// 从当前目录或父目录中的 `deploy/config.yaml` 加载顶层配置。
pub fn load_config<T: LoadableConfig>() -> Result<T, ConfigError> {
    let current = env::current_dir().map_err(|_| ConfigError::CurrentDirectory)?;
    let path = discover_config_path(&current)?;
    let source_dir = path.parent().ok_or(ConfigError::InvalidConfigPath)?;
    let document = config::Config::builder()
        .add_source(config::File::from(path.as_path()).required(true))
        .build()
        .map_err(|_| ConfigError::InvalidDocument { path: path.clone() })?;
    let mut unused = std::collections::BTreeSet::new();
    let mut value: T = serde_ignored::deserialize(document, |field| {
        unused.insert(field.to_string());
    })
    .map_err(|error| match missing_config_field(&error) {
        Some(field) => ConfigError::MissingField {
            path: path.clone(),
            field,
        },
        None => ConfigError::InvalidDocument { path: path.clone() },
    })?;
    // 此时日志尚未初始化；只报告字段路径，不回显可能包含凭据的配置值。
    for field in unused {
        if !T::EXTERNAL_SECTIONS.contains(&field.as_str()) {
            eprintln!("[警告] 配置字段 {field:?} 未使用，已忽略");
        }
    }
    value.resolve_and_validate(source_dir)?;
    Ok(value)
}

fn missing_config_field(error: &config::ConfigError) -> Option<String> {
    match error {
        config::ConfigError::NotFound(field) => Some(field.clone()),
        config::ConfigError::At { error, key, .. } => missing_config_field(error).map(|field| {
            key.as_ref()
                .map_or_else(|| field.clone(), |key| format!("{key}.{field}"))
        }),
        _ => None,
    }
}

/// Host 唯一拥有的进程配置。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HostConfig {
    pub listen: ListenConfig,
    pub runtime_data_dir: PathBuf,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub openai_slots: OpenAiSlotsConfig,
    #[serde(default)]
    pub system_update: SystemUpdateConfig,
    #[serde(default = "default_drain_timeout_seconds")]
    pub drain_timeout_seconds: u64,
    #[serde(default = "default_worker_shutdown_timeout_seconds")]
    pub worker_shutdown_timeout_seconds: u64,
}

impl HostConfig {
    /// 解析 Host 配置；在线更新默认沿用已解析的 API 静态资源目录。
    pub fn resolve_and_validate(
        &mut self,
        source_dir: &Path,
        asset_directory: &Path,
    ) -> Result<(), ConfigError> {
        if let Some(enabled) = optional_environment_value(SLOTS_ENABLED_ENV)? {
            self.openai_slots.enabled = enabled
                .parse()
                .map_err(|_| ConfigError::InvalidEnvironment(SLOTS_ENABLED_ENV))?;
        }
        if let Some(image) = optional_environment_value(SLOTS_IMAGE_ENV)? {
            self.openai_slots.image = image;
        }
        if let Some(host) = optional_environment_value(SERVER_HOST_ENV)? {
            self.listen.host = host;
        }
        if let Some(port) = optional_environment_value(SERVER_PORT_ENV)? {
            self.listen.port = port
                .parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or(ConfigError::InvalidEnvironment(SERVER_PORT_ENV))?;
        }
        if self.listen.host.trim().is_empty() {
            return Err(ConfigError::InvalidField("host.listen.host"));
        }
        if self.listen.port == 0 {
            return Err(ConfigError::InvalidField("host.listen.port"));
        }
        if self.drain_timeout_seconds == 0 {
            return Err(ConfigError::InvalidField("host.drain_timeout_seconds"));
        }
        if self.worker_shutdown_timeout_seconds == 0 {
            return Err(ConfigError::InvalidField(
                "host.worker_shutdown_timeout_seconds",
            ));
        }
        if self.runtime_data_dir.as_os_str().is_empty() {
            return Err(ConfigError::InvalidField("host.runtime_data_dir"));
        }
        resolve_relative_path(source_dir, &mut self.runtime_data_dir);
        self.logging.resolve_and_validate(source_dir)?;
        self.openai_slots.validate()?;
        self.system_update.resolve_and_validate(
            source_dir,
            &self.runtime_data_dir,
            asset_directory,
        )?;
        Ok(())
    }

    #[must_use]
    pub fn runtime_data_dir(&self) -> &Path {
        &self.runtime_data_dir
    }

    #[must_use]
    pub const fn drain_timeout(&self) -> Duration {
        Duration::from_secs(self.drain_timeout_seconds)
    }

    #[must_use]
    pub const fn worker_shutdown_timeout(&self) -> Duration {
        Duration::from_secs(self.worker_shutdown_timeout_seconds)
    }
}

/// OpenAI 账号独立容器槽位的 Host 配置。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OpenAiSlotsConfig {
    /// 全局开关；关闭时不连接 Docker，已启用槽位的账号保持不可调度。
    pub enabled: bool,
    /// sidecar 镜像引用，生产环境建议固定 digest。
    pub image: String,
    /// Docker Engine endpoint，仅支持 Host 可访问的 unix/npipe endpoint。
    pub docker_endpoint: String,
    /// 网关容器 ID 或名称；缺省读取容器内 HOSTNAME。
    pub gateway_container: Option<String>,
    pub reconcile_interval_seconds: u64,
    pub start_timeout_seconds: u64,
    pub memory_limit_mb: u64,
    pub cpu_limit_millis: u64,
    pub pids_limit: u64,
}

impl Default for OpenAiSlotsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            image: "codex-slot-sidecar:local".to_owned(),
            docker_endpoint: "unix:///var/run/docker.sock".to_owned(),
            gateway_container: None,
            reconcile_interval_seconds: 10,
            start_timeout_seconds: 30,
            memory_limit_mb: 512,
            cpu_limit_millis: 1_000,
            pids_limit: 128,
        }
    }
}

impl OpenAiSlotsConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }
        if self
            .gateway_container
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(ConfigError::InvalidField(
                "host.openai_slots.gateway_container",
            ));
        }
        if self.image.trim().is_empty() {
            return Err(ConfigError::InvalidField("host.openai_slots.image"));
        }
        if !self.docker_endpoint.starts_with("unix://")
            && !self.docker_endpoint.starts_with("npipe://")
        {
            return Err(ConfigError::InvalidField(
                "host.openai_slots.docker_endpoint",
            ));
        }
        for (value, field) in [
            (
                self.reconcile_interval_seconds,
                "host.openai_slots.reconcile_interval_seconds",
            ),
            (
                self.start_timeout_seconds,
                "host.openai_slots.start_timeout_seconds",
            ),
            (self.memory_limit_mb, "host.openai_slots.memory_limit_mb"),
            (self.cpu_limit_millis, "host.openai_slots.cpu_limit_millis"),
            (self.pids_limit, "host.openai_slots.pids_limit"),
        ] {
            if value == 0 {
                return Err(ConfigError::InvalidField(field));
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn reconcile_interval(&self) -> Duration {
        Duration::from_secs(self.reconcile_interval_seconds)
    }

    #[must_use]
    pub const fn start_timeout(&self) -> Duration {
        Duration::from_secs(self.start_timeout_seconds)
    }
}

/// HTTP 监听地址。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ListenConfig {
    pub host: String,
    pub port: u16,
}

/// Host 结构化日志配置。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LoggingConfig {
    pub level: String,
    pub stdout: bool,
    pub file: FileLoggingConfig,
    /// 将 OAuth 原始 AT/RT 写入独立恢复日志；默认关闭，沿用 file 的目录与保留期。
    #[serde(default)]
    pub oauth_recovery: bool,
    /// 将完整请求原文写入独立诊断日志；默认关闭，因为内容包含凭据。
    /// 请求报文的留存窗口由 request_dump_retention_days 独立控制。
    #[serde(default)]
    pub request_dump: bool,
    /// 完整报文的最少保留天数；按 UTC 日期整组清理，默认 1 天。
    #[serde(default = "default_request_dump_retention_days")]
    pub request_dump_retention_days: usize,
}

fn default_file_retention_days() -> usize {
    7
}

fn default_request_dump_retention_days() -> usize {
    1
}

impl LoggingConfig {
    fn resolve_and_validate(&mut self, source_dir: &Path) -> Result<(), ConfigError> {
        EnvFilter::try_new(&self.level)
            .map_err(|_| ConfigError::InvalidField("host.logging.level"))?;
        if !self.stdout && !self.file.enabled && !self.oauth_recovery && !self.request_dump {
            return Err(ConfigError::InvalidField("host.logging"));
        }
        if self.file.directory.as_os_str().is_empty() {
            return Err(ConfigError::InvalidField("host.logging.file.directory"));
        }
        if self.file.retention_days == 0 {
            return Err(ConfigError::InvalidField(
                "host.logging.file.retention_days",
            ));
        }
        if self.file.max_file_size_mb == 0 {
            return Err(ConfigError::InvalidField(
                "host.logging.file.max_file_size_mb",
            ));
        }
        if self.request_dump_retention_days == 0 {
            return Err(ConfigError::InvalidField(
                "host.logging.request_dump_retention_days",
            ));
        }
        resolve_relative_path(source_dir, &mut self.file.directory);
        Ok(())
    }
}

/// Host 文件日志配置。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FileLoggingConfig {
    pub enabled: bool,
    pub directory: PathBuf,
    /// 至少保留的完整天数，默认 7 天；文件数量不参与清理。
    #[serde(default = "default_file_retention_days")]
    pub retention_days: usize,
    pub max_file_size_mb: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("current directory is unavailable")]
    CurrentDirectory,
    #[error("deploy/config.yaml was not found")]
    ConfigFileNotFound,
    #[error("configuration path is invalid")]
    InvalidConfigPath,
    #[error("configuration document is invalid: {path}")]
    InvalidDocument { path: PathBuf },
    #[error("配置文件 {path} 缺少必填字段 {field:?}")]
    MissingField { path: PathBuf, field: String },
    #[error("configuration field is invalid: {0}")]
    InvalidField(&'static str),
    #[error("environment variable is invalid: {0}")]
    InvalidEnvironment(&'static str),
}

fn optional_environment_value(name: &'static str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Err(ConfigError::InvalidEnvironment(name)),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError::InvalidEnvironment(name)),
    }
}

fn discover_config_path(start: &Path) -> Result<PathBuf, ConfigError> {
    start
        .ancestors()
        .map(|directory| directory.join(CONFIG_RELATIVE_PATH))
        .find(|candidate| candidate.is_file())
        .ok_or(ConfigError::ConfigFileNotFound)
}

fn resolve_relative_path(base: &Path, path: &mut PathBuf) {
    if path.is_relative() {
        *path = base.join(&*path);
    }
}

const fn default_drain_timeout_seconds() -> u64 {
    30
}

const fn default_worker_shutdown_timeout_seconds() -> u64 {
    30
}
