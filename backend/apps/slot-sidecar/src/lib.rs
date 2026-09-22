//! OpenAI 账号槽位内的受限上游转发 sidecar。

mod config;
mod forward;
mod server;

pub use config::{SidecarConfig, SidecarConfigError};
pub use server::{SidecarBuildError, build_router, serve};
