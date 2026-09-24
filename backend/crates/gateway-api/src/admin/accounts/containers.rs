//! 独立容器槽位的管理员 HTTP 边界。
use super::*;
use crate::auth::SessionState;
use gateway_admin::model::account_slots::{
    ContainerSlotAction, ContainerSlotCommand, ContainerSlotView,
};
use gateway_core::account::AccountSlotInstanceId;

pub(super) fn router<S>() -> Router<S>
where
    S: SessionState + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/api/admin/containers", get(list::<S>))
        .route("/api/admin/containers/update", post(update::<S>))
}

#[derive(Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Update {
    Create {
        name: String,
    },
    ConfigureProxy {
        id: String,
        expected_generation: u64,
        proxy_id: Option<String>,
    },
    Bind {
        id: String,
        expected_generation: u64,
        account_id: Option<String>,
    },
    Start {
        id: String,
        expected_generation: u64,
    },
    Stop {
        id: String,
        expected_generation: u64,
    },
    Delete {
        id: String,
        expected_generation: u64,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct View {
    id: String,
    name: String,
    account_id: Option<String>,
    account_name: Option<String>,
    account_enabled: bool,
    proxy_id: Option<String>,
    proxy_name: Option<String>,
    running: bool,
    generation: u64,
    state: &'static str,
    reason: Option<&'static str>,
}
impl From<ContainerSlotView> for View {
    fn from(value: ContainerSlotView) -> Self {
        let s = value.slot;
        Self {
            id: s.id.to_string(),
            name: s.name,
            account_id: s.account_id.map(|id| id.as_str().to_owned()),
            account_name: s.account_name,
            account_enabled: s.account_enabled,
            proxy_id: s.proxy_id,
            proxy_name: s.proxy_name,
            running: s.running,
            generation: s.generation.get(),
            state: value.state,
            reason: value.reason,
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct List {
    global_enabled: bool,
    items: Vec<View>,
}

async fn list<S>(_auth: AdminAuth, State(state): State<S>) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let services = state.admin_services();
    let result = services
        .account_slots()
        .map_err(map_service_error)?
        .containers()
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(
        StatusCode::OK,
        AdminEnvelope::ok(List {
            global_enabled: result.global_enabled,
            items: result.items.into_iter().map(View::from).collect(),
        }),
    ))
}
async fn update<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(body): AdminJson<Update>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let (id, generation, action) = match body {
        Update::Create { name } => (None, None, ContainerSlotAction::Create { name }),
        Update::ConfigureProxy {
            id,
            expected_generation,
            proxy_id,
        } => (
            Some(id),
            Some(expected_generation),
            ContainerSlotAction::ConfigureProxy { proxy_id },
        ),
        Update::Bind {
            id,
            expected_generation,
            account_id,
        } => (
            Some(id),
            Some(expected_generation),
            ContainerSlotAction::Bind {
                account_id: account_id
                    .map(ProviderAccountId::new)
                    .transpose()
                    .map_err(|_| map_service_error(AdminServiceError::invalid("账号 ID 不合法")))?,
            },
        ),
        Update::Start {
            id,
            expected_generation,
        } => (
            Some(id),
            Some(expected_generation),
            ContainerSlotAction::Start,
        ),
        Update::Stop {
            id,
            expected_generation,
        } => (
            Some(id),
            Some(expected_generation),
            ContainerSlotAction::Stop,
        ),
        Update::Delete {
            id,
            expected_generation,
        } => (
            Some(id),
            Some(expected_generation),
            ContainerSlotAction::Delete,
        ),
    };
    let id = id
        .map(|id| AccountSlotInstanceId::parse(&id))
        .transpose()
        .map_err(|_| map_service_error(AdminServiceError::invalid("槽位 ID 不合法")))?;
    let services = state.admin_services();
    services
        .account_slots()
        .map_err(map_service_error)?
        .mutate_container(
            ContainerSlotCommand {
                id,
                expected_generation: generation,
                action,
            },
            &auth.context().mutation_context(),
        )
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(
        StatusCode::OK,
        AdminEnvelope::ok(serde_json::json!({ "saved": true })),
    ))
}
