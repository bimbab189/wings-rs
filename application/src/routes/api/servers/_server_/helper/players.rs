use super::super::State;
use utoipa_axum::router::OpenApiRouter;

pub(crate) mod _player_ {
    use super::State;
    use utoipa_axum::{router::OpenApiRouter, routes};

    pub(crate) mod snapshot {
        use crate::{
            response::{ApiResponse, ApiResponseResult},
            routes::api::servers::_server_::GetServer,
        };
        use axum::extract::Path;
        use serde::Deserialize;

        #[derive(Deserialize)]
        pub struct SnapshotPath {
            server: String,
            player: String,
        }

        #[utoipa::path(get, path = "/snapshot", responses(
            (status = OK, body = serde_json::Value),
        ))]
        pub async fn route(Path(path): Path<SnapshotPath>, server: GetServer) -> ApiResponseResult {
            let _ = &path.server;
            let snapshot =
                crate::server::helper::get_player_snapshot(&server, &path.player).await?;

            ApiResponse::new_serialized(snapshot).ok()
        }
    }

    pub fn router(state: &State) -> OpenApiRouter<State> {
        OpenApiRouter::new()
            .routes(routes!(snapshot::route))
            .with_state(state.clone())
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .nest("/{player}", _player_::router(state))
        .with_state(state.clone())
}
