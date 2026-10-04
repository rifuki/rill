//! Owner-authenticated HTTP dispatch for setup previews and transactions.
use super::*;

pub async fn prepare(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle(state, headers, body, SetupAction::Prepare).await
}
pub async fn attach(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle(state, headers, body, SetupAction::Attach).await
}
pub async fn preview_setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(state, headers, body, SetupAction::Preview).await
}
enum SetupAction {
    Preview,
    Prepare,
    Attach,
}
async fn handle(state: AppState, headers: HeaderMap, body: Bytes, action: SetupAction) -> Response {
    let body = match studio_api::parse_body(&body) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let owner = match studio_api::owner(&state, &headers) {
        Ok(Some(o)) => o,
        Ok(None) => {
            return api_err_typed(
                StatusCode::UNAUTHORIZED,
                "Sign in with the wallet that owns this skill",
                "Unauthorized",
            )
        }
        Err(e) => return *e,
    };
    let Some(skill) = body["skillId"].as_str().and_then(|id| state.skills.get(id)) else {
        return api_err_typed(StatusCode::NOT_FOUND, "Skill not found", "NotFound");
    };
    let context = match setup_context(&state, &skill).await {
        Ok(context) => context,
        Err(e) => return *e,
    };
    let result = match action {
        SetupAction::Preview => {
            preview::plan(&body, &skill, &owner, &context, state.chain.as_ref())
                .await
                .map(|p| json!({"swapPreview":p}))
        }
        SetupAction::Attach => {
            attach_plan(&body, &skill, &owner, &context, state.chain.as_ref()).await
        }
        SetupAction::Prepare => {
            prepare_plan(&body, &skill, &owner, &context, state.chain.as_ref()).await
        }
    };
    match result {
        Ok(value) => api_ok(value),
        Err(e) => studio_api::invalid(e),
    }
}
