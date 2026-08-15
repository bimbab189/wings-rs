use super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

pub(crate) mod get {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::api::servers::_server_::GetServer,
        routes::{ApiError, GetState},
    };
    use serde::Serialize;
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    struct Response {
        enabled: bool,
        unix_socket_proxy: bool,
        terminal: bool,
        console: bool,
        collaboration: bool,
    }

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = inline(Response)),
        (status = BAD_REQUEST, body = inline(ApiError)),
    ))]
    pub async fn route(state: GetState, _server: GetServer) -> ApiResponseResult {
        // Egg-specific eligibility is checked by the panel and repeated in the
        // session-start payload. Keep this read-only capability endpoint free
        // of game-container configuration locks.
        let eligible = state.web_ide.enabled();

        ApiResponse::new_serialized(Response {
            enabled: eligible,
            unix_socket_proxy: true,
            terminal: eligible,
            console: eligible,
            collaboration: eligible,
        })
        .ok()
    }
}

pub(crate) mod post_session {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::api::servers::_server_::GetServer,
        routes::{ApiError, GetState},
        server::web_ide::StartWebIdeSession,
    };
    use serde::{Deserialize, Serialize};
    use std::{path::PathBuf, time::Duration};
    use utoipa::ToSchema;

    #[derive(ToSchema, Deserialize)]
    pub struct Payload {
        session_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
        #[serde(default)]
        user_session_limit: Option<usize>,
        #[serde(default)]
        user_server_session_limit: Option<usize>,
        display_name: String,
        can_use_console: bool,
        #[serde(default)]
        copilot_addon_tools_enabled: bool,
        #[serde(default)]
        permissions: crate::server::permissions::Permissions,
        #[serde(default)]
        has_file_denylist: Option<bool>,
        #[serde(default = "default_presence_timeout_seconds")]
        presence_timeout_seconds: u64,
        idle_timeout_seconds: u64,
        maximum_lifetime_seconds: u64,
    }

    fn default_presence_timeout_seconds() -> u64 {
        90
    }

    #[derive(ToSchema, Serialize)]
    struct Response {
        session_uuid: uuid::Uuid,
    }

    #[utoipa::path(post, path = "/sessions", responses(
        (status = OK, body = inline(Response)),
        (status = BAD_REQUEST, body = inline(ApiError)),
    ), request_body = inline(Payload))]
    pub async fn route(
        state: GetState,
        server: GetServer,
        crate::Payload(data): crate::Payload<Payload>,
    ) -> ApiResponseResult {
        if data.display_name.is_empty()
            || data.display_name.len() > 191
            || !(300..=86_400).contains(&data.idle_timeout_seconds)
            || !(30..=600).contains(&data.presence_timeout_seconds)
            || !(900..=86_400).contains(&data.maximum_lifetime_seconds)
            || data.user_session_limit == Some(0)
            || data.user_server_session_limit == Some(0)
        {
            return ApiResponse::error("invalid Web IDE session parameters")
                .with_status(axum::http::StatusCode::BAD_REQUEST)
                .ok();
        }

        let created = state
            .web_ide
            .start(
                &server,
                StartWebIdeSession {
                    session_uuid: data.session_uuid,
                    user_uuid: data.user_uuid,
                    user_session_limit: data.user_session_limit,
                    user_server_session_limit: data.user_server_session_limit,
                    display_name: data.display_name,
                    can_use_console: data.can_use_console,
                    copilot_addon_tools_enabled: data.copilot_addon_tools_enabled,
                    permissions: data.permissions,
                    has_file_denylist: data.has_file_denylist,
                    presence_timeout: Duration::from_secs(data.presence_timeout_seconds),
                    idle_timeout: Duration::from_secs(data.idle_timeout_seconds),
                    maximum_lifetime: Duration::from_secs(data.maximum_lifetime_seconds),
                },
            )
            .await?;

        if created {
            let terminal_socket = PathBuf::from(&state.config.web_ide.runtime_directory)
                .join(data.session_uuid.to_string())
                .join("terminal.sock");
            if let Err(error) = crate::routes::api::web_ide::start_local_terminal_listener(
                state.clone(),
                server.0.clone(),
                server.uuid,
                data.session_uuid,
                terminal_socket,
            )
            .await
            {
                tracing::error!(
                    server = %server.uuid,
                    session = %data.session_uuid,
                    error = %error,
                    "failed to start the isolated Web IDE agent terminal"
                );
                state
                    .web_ide
                    .stop(data.session_uuid, "terminal_socket_failed")
                    .await;
                return ApiResponse::error("failed to create the isolated Web IDE terminal")
                    .with_status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                    .ok();
            }

            let addon_tools_socket = PathBuf::from(&state.config.web_ide.runtime_directory)
                .join(data.session_uuid.to_string())
                .join("addon-tools.sock");
            if let Err(error) = crate::routes::api::web_ide::start_local_addon_tools_listener(
                state.clone(),
                server.0.clone(),
                server.uuid,
                data.session_uuid,
                addon_tools_socket,
            )
            .await
            {
                tracing::error!(
                    server = %server.uuid,
                    session = %data.session_uuid,
                    error = %error,
                    "failed to start the isolated Web IDE addon-tools listener"
                );
                state
                    .web_ide
                    .stop(data.session_uuid, "addon_tools_socket_failed")
                    .await;
                return ApiResponse::error(
                    "failed to create the isolated Web IDE addon-tools listener",
                )
                .with_status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                .ok();
            }
        }

        ApiResponse::new_serialized(Response {
            session_uuid: data.session_uuid,
        })
        .ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rolling_upgrade_payload_without_permissions_is_accepted_fail_closed() {
            let payload: Payload = serde_json::from_value(serde_json::json!({
                "session_uuid": uuid::Uuid::new_v4(),
                "user_uuid": uuid::Uuid::new_v4(),
                "display_name": "owner",
                "can_use_console": true,
                "presence_timeout_seconds": 90,
                "idle_timeout_seconds": 1800,
                "maximum_lifetime_seconds": 3600
            }))
            .unwrap();

            assert!(payload.permissions.is_empty());
            assert_eq!(payload.has_file_denylist, None);
            assert_eq!(payload.user_session_limit, None);
            assert_eq!(payload.user_server_session_limit, None);
            assert_eq!(payload.presence_timeout_seconds, 90);
        }
    }
}

pub(crate) mod delete_session {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::api::servers::_server_::GetServer,
        routes::{ApiError, GetState},
    };
    use axum::extract::Path;
    use serde::{Deserialize, Serialize};
    use utoipa::ToSchema;

    #[derive(ToSchema, Deserialize)]
    pub struct Payload {
        reason: Option<String>,
    }

    #[derive(ToSchema, Serialize)]
    struct Response {}

    #[utoipa::path(delete, path = "/sessions/{session}", responses(
        (status = OK, body = inline(Response)),
        (status = BAD_REQUEST, body = inline(ApiError)),
    ), request_body = inline(Payload))]
    pub async fn route(
        state: GetState,
        server: GetServer,
        Path((_server, session)): Path<(uuid::Uuid, uuid::Uuid)>,
        crate::Payload(data): crate::Payload<Payload>,
    ) -> ApiResponseResult {
        let reason = data.reason.as_deref().unwrap_or("panel_revoked");
        state
            .web_ide
            .stop_for_server(session, server.uuid, reason)
            .await;
        ApiResponse::new_serialized(Response {}).ok()
    }
}

pub(crate) mod heartbeat_session {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::GetState,
        routes::api::servers::_server_::GetServer,
    };
    use axum::{extract::Path, http::StatusCode};
    use serde::Serialize;
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    struct Response {}

    /// Authenticated panel-to-Wings lease renewal. The panel's node bearer
    /// token is applied by the parent `/api/servers` route layer; no browser
    /// session cookie or extension token is accepted here.
    #[utoipa::path(post, path = "/sessions/{session}/heartbeat", responses(
        (status = NO_CONTENT, body = inline(Response)),
        (status = NOT_FOUND, body = crate::routes::ApiError),
    ))]
    pub async fn route(
        state: GetState,
        server: GetServer,
        Path((_server, session)): Path<(uuid::Uuid, uuid::Uuid)>,
    ) -> ApiResponseResult {
        match state.web_ide.touch_presence(session, server.uuid).await {
            Ok(()) => ApiResponse::new_serialized(Response {})
                .with_status(StatusCode::NO_CONTENT)
                .ok(),
            Err(_) => ApiResponse::error("Web IDE session is no longer active")
                .with_status(StatusCode::NOT_FOUND)
                .ok(),
        }
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .routes(routes!(post_session::route))
        .routes(routes!(heartbeat_session::route))
        .routes(routes!(delete_session::route))
        .with_state(state.clone())
}
