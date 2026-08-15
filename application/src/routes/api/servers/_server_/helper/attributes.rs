use super::super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

pub(crate) mod get {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::api::servers::_server_::GetServer,
    };

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = serde_json::Value),
    ))]
    pub async fn route(server: GetServer) -> ApiResponseResult {
        let attributes = crate::server::helper::get_attribute_catalog(&server).await?;

        ApiResponse::new_serialized(attributes).ok()
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .with_state(state.clone())
}
