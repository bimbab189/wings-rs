use super::State;
use utoipa_axum::router::OpenApiRouter;

pub(crate) mod broadcast;
pub(crate) mod deny;
pub(crate) mod permissions;

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .nest("/deny", deny::router(state))
        .nest("/broadcast", broadcast::router(state))
        .nest("/permissions", permissions::router(state))
        .with_state(state.clone())
}
