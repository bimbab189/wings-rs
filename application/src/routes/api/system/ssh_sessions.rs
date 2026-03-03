use super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

mod get {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::GetState,
        ssh::registry::SshSessionEntry,
    };
    use serde::Serialize;
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    pub struct Response {
        sessions: Vec<SshSessionEntry>,
        count: usize,
    }

    /// List all currently active SSH sessions across all servers.
    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = inline(Response)),
    ))]
    pub async fn route(state: GetState) -> ApiResponseResult {
        let sessions = state.ssh_sessions.all().await;
        let count = sessions.len();

        ApiResponse::new_serialized(Response { sessions, count }).ok()
    }
}

mod server_get {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::GetState,
        ssh::registry::SshSessionEntry,
    };
    use axum::extract::Path;
    use serde::Serialize;
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    pub struct Response {
        sessions: Vec<SshSessionEntry>,
        count: usize,
    }

    /// List active SSH sessions for a specific server.
    #[utoipa::path(get, path = "/{server}", responses(
        (status = OK, body = inline(Response)),
    ))]
    pub async fn route(state: GetState, Path(server): Path<uuid::Uuid>) -> ApiResponseResult {
        let sessions = state.ssh_sessions.for_server(server).await;
        let count = sessions.len();

        ApiResponse::new_serialized(Response { sessions, count }).ok()
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .routes(routes!(server_get::route))
        .with_state(state.clone())
}
