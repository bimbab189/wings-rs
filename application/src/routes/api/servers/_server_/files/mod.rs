use super::State;
use utoipa_axum::router::OpenApiRouter;

pub(crate) mod chmod;
pub(crate) mod compress;
pub(crate) mod contents;
pub(crate) mod copy;
pub(crate) mod copy_many;
pub(crate) mod copy_remote;
pub(crate) mod create_directory;
pub(crate) mod decompress;
pub(crate) mod delete;
pub(crate) mod fingerprints;
pub(crate) mod largest_directories;
pub(crate) mod list;
pub(crate) mod list_directory;
pub(crate) mod operations;
pub(crate) mod pull;
pub(crate) mod rename;
pub(crate) mod revisions;
pub(crate) mod search;
pub(crate) mod write;

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .nest("/contents", contents::router(state))
        .nest("/list-directory", list_directory::router(state))
        .nest("/list", list::router(state))
        .nest("/rename", rename::router(state))
        .nest("/copy", copy::router(state))
        .nest("/copy-many", copy_many::router(state))
        .nest("/copy-remote", copy_remote::router(state))
        .nest("/write", write::router(state))
        .nest("/create-directory", create_directory::router(state))
        .nest("/largest-directories", largest_directories::router(state))
        .nest("/delete", delete::router(state))
        .nest("/chmod", chmod::router(state))
        .nest("/search", search::router(state))
        .nest("/fingerprints", fingerprints::router(state))
        .nest("/pull", pull::router(state))
        .nest("/compress", compress::router(state))
        .nest("/decompress", decompress::router(state))
        .nest("/operations", operations::router(state))
        .nest("/revisions", revisions::router(state))
        .with_state(state.clone())
}
