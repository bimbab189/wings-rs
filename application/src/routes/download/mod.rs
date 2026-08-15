use super::State;
use utoipa_axum::router::OpenApiRouter;

pub(crate) mod backup;
pub(crate) mod directory;
pub(crate) mod file;
pub(crate) mod files;

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .nest("/file", file::router(state))
        .nest("/files", files::router(state))
        .nest("/directory", directory::router(state))
        .nest("/backup", backup::router(state))
        .with_state(state.clone())
}
