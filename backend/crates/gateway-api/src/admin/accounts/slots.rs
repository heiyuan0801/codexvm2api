//! 管理员槽位状态查询与带代次校验的意图更新。

use super::*;
use crate::auth::SessionState;
use gateway_admin::model::account_slots::{AccountSlotView, SetAccountSlot};

pub(super) fn router<S>() -> Router<S>
where
    S: SessionState + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/api/admin/accounts/slots/query", post(query::<S>))
        .route("/api/admin/accounts/slots/update", post(update::<S>))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Query {
    account_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Update {
    account_id: String,
    enabled: bool,
    expected_generation: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SlotView {
    account_id: String,
    enabled: bool,
    generation: Option<u64>,
    state: &'static str,
    reason: Option<&'static str>,
}

impl From<AccountSlotView> for SlotView {
    fn from(value: AccountSlotView) -> Self {
        Self {
            account_id: value.account_id.as_str().to_owned(),
            enabled: value.enabled,
            generation: value.generation,
            state: value.state.as_str(),
            reason: value.reason,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SlotsView {
    global_enabled: bool,
    items: Vec<SlotView>,
}

async fn query<S>(
    _auth: AdminAuth,
    State(state): State<S>,
    AdminJson(body): AdminJson<Query>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    if body.account_ids.is_empty() || body.account_ids.len() > 200 {
        return Err(map_service_error(AdminServiceError::invalid(
            "每次查询 1 至 200 个账号",
        )));
    }
    let ids = body
        .account_ids
        .into_iter()
        .map(|id| {
            ProviderAccountId::new(id)
                .map_err(|_| map_service_error(AdminServiceError::invalid("账号 ID 不合法")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let services = state.admin_services();
    let result = services
        .account_slots()
        .map_err(map_service_error)?
        .read(ids)
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(
        StatusCode::OK,
        AdminEnvelope::ok(SlotsView {
            global_enabled: result.global_enabled,
            items: result.items.into_iter().map(SlotView::from).collect(),
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
    let account_id = ProviderAccountId::new(body.account_id)
        .map_err(|_| map_service_error(AdminServiceError::invalid("账号 ID 不合法")))?;
    let services = state.admin_services();
    let result = services
        .account_slots()
        .map_err(map_service_error)?
        .set(
            SetAccountSlot {
                account_id,
                enabled: body.enabled,
                expected_generation: body.expected_generation,
            },
            &auth.context().mutation_context(),
        )
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(
        StatusCode::OK,
        AdminEnvelope::ok(SlotView::from(result)),
    ))
}
