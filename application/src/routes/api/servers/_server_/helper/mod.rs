use super::State;
use utoipa_axum::router::OpenApiRouter;

pub(crate) mod attributes;
pub(crate) mod install;
pub(crate) mod players;
pub(crate) mod status;

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .nest("/install", install::router(state))
        .nest("/attributes", attributes::router(state))
        .nest("/status", status::router(state))
        .nest("/players", players::router(state))
        .with_state(state.clone())
}
