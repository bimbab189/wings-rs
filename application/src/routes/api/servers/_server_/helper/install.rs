use super::super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

pub(crate) mod post {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::{ApiError, api::servers::_server_::GetServer},
    };
    use axum::http::StatusCode;
    use serde::Deserialize;
    use utoipa::ToSchema;

    #[derive(ToSchema, Deserialize)]
    pub struct Payload {
        #[schema(example = "legacyfabric")]
        variant: String,
    }

    #[utoipa::path(post, path = "/", responses(
        (status = OK, body = crate::server::helper::HelperInstallResponse),
        (status = EXPECTATION_FAILED, body = ApiError),
    ), params(
        (
            "server" = uuid::Uuid,
            description = "The server uuid",
            example = "123e4567-e89b-12d3-a456-426614174000",
        ),
    ), request_body = inline(Payload))]
    pub async fn route(
        server: GetServer,
        crate::Payload(data): crate::Payload<Payload>,
    ) -> ApiResponseResult {
        match crate::server::helper::install_host_artifact(&server, &data.variant).await {
            Ok(response) => ApiResponse::new_serialized(response).ok(),
            Err(error) => ApiResponse::error(&error.to_string())
                .with_status(StatusCode::EXPECTATION_FAILED)
                .ok(),
        }
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(post::route))
        .with_state(state.clone())
}
