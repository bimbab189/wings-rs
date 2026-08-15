use anyhow::Context;
use axum::{
    body::{self, Body},
    extract::{
        ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, Request,
        ws::{Message as ClientMessage, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, header},
    response::{Html, IntoResponse},
    routing::{any, get, post},
};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, mpsc},
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use utoipa_axum::router::OpenApiRouter;
use yrs_axum::ws::{AxumSink, AxumStream};

use crate::{
    remote::jwt::BasePayload,
    routes::{GetState, State},
    server::{
        activity::{Activity, ActivityEvent},
        permissions::{Permission, Permissions},
        web_ide::{BrowserStateOperation, BrowserStateResult},
    },
};

#[derive(Deserialize)]
struct LaunchRequest {
    token: String,
}

#[derive(Deserialize)]
struct LaunchClaims {
    #[serde(flatten)]
    base: BasePayload,
    kind: String,
    server_uuid: uuid::Uuid,
    session_uuid: uuid::Uuid,
    user_uuid: uuid::Uuid,
    display_name: String,
    permissions: Permissions,
    can_use_console: bool,
}

#[derive(Deserialize)]
struct ProxyPath {
    server: uuid::Uuid,
    session: uuid::Uuid,
    // Present on the wildcard route and absent on the exact `/proxy/` route.
    // The URI itself remains authoritative for forwarding after authentication.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Serialize)]
struct LaunchResponse {
    redirect: String,
}

#[derive(Deserialize)]
struct AgentChatRequest {
    provider: String,
    api_key: String,
    model: String,
    messages: serde_json::Value,
    #[serde(default)]
    tools: Option<serde_json::Value>,
    #[serde(default)]
    tool_choice: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct AddonToolRequest {
    operation: String,
    input: serde_json::Value,
}

#[derive(Deserialize)]
struct UserThemeRequest {
    theme: String,
}

#[derive(Serialize)]
struct UserThemeResponse {
    theme: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum BrowserStateRequest {
    SecretGet {
        key: String,
    },
    SecretSet {
        key: String,
        value: String,
    },
    SecretDelete {
        key: String,
    },
    SecretKeys,
    StorageSnapshot {
        database: String,
    },
    StorageUpdate {
        database: String,
        #[serde(default)]
        insert: Vec<(String, String)>,
        #[serde(default)]
        delete: Vec<String>,
    },
    StorageClear {
        database: String,
    },
}

#[derive(Default, Serialize)]
struct BrowserStateResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keys: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<(String, String)>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    present: Option<bool>,
}

#[derive(Clone)]
enum SessionCredential {
    Cookie(String),
    Extension(String),
}

#[derive(Clone)]
struct AuthenticatedCollaborationProtocol {
    display_name: String,
    connection_uuid: uuid::Uuid,
    claimed_client: Arc<StdMutex<Option<u64>>>,
    awareness_clients: Arc<StdMutex<HashMap<u64, uuid::Uuid>>>,
}

impl yrs::sync::Protocol for AuthenticatedCollaborationProtocol {
    fn handle_awareness_update(
        &self,
        awareness: &mut yrs::sync::Awareness,
        mut update: yrs::sync::AwarenessUpdate,
    ) -> Result<Option<yrs::sync::Message>, yrs::sync::Error> {
        let invalid_update = || {
            yrs::sync::Error::IO(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "awareness client identity does not belong to this connection",
            ))
        };
        let mut claimed_client = self.claimed_client.lock().map_err(|_| invalid_update())?;
        let mut awareness_clients = self
            .awareness_clients
            .lock()
            .map_err(|_| invalid_update())?;
        let client_id = match *claimed_client {
            Some(client_id) => client_id,
            None => {
                if update.clients.len() != 1 {
                    return Err(invalid_update());
                }
                let (client_id, entry) = update.clients.iter().next().ok_or_else(invalid_update)?;
                if entry.json == "null" {
                    return Err(invalid_update());
                }
                match awareness_clients.get(client_id) {
                    Some(owner) if *owner != self.connection_uuid => return Err(invalid_update()),
                    Some(_) => {}
                    None => {
                        awareness_clients.insert(*client_id, self.connection_uuid);
                    }
                }
                *claimed_client = Some(*client_id);
                *client_id
            }
        };
        if awareness_clients.get(&client_id) != Some(&self.connection_uuid) {
            return Err(invalid_update());
        }
        // y-websocket re-broadcasts awareness changes it receives for remote
        // clients. The room already has the canonical copy of those entries;
        // silently discard them so this connection can only mutate its bound ID.
        update
            .clients
            .retain(|candidate, _| *candidate == client_id);
        for entry in update.clients.values_mut() {
            if entry.json == "null" {
                continue;
            }
            let mut value: serde_json::Value = serde_json::from_str(&entry.json)
                .map_err(|error| yrs::sync::Error::Other(Box::new(error)))?;
            let object = value.as_object_mut().ok_or_else(|| {
                yrs::sync::Error::IO(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "awareness state must be an object",
                ))
            })?;
            object.insert(
                "user".to_string(),
                serde_json::json!({ "name": self.display_name }),
            );
            entry.json = serde_json::to_string(&value)
                .map_err(|error| yrs::sync::Error::Other(Box::new(error)))?;
        }
        awareness.apply_update(update)?;
        Ok(None)
    }
}

impl SessionCredential {
    async fn authenticate(
        &self,
        state: &GetState,
        session: uuid::Uuid,
        server: uuid::Uuid,
        interaction: bool,
    ) -> Option<crate::server::web_ide::WebIdeSession> {
        match self {
            Self::Cookie(secret) => {
                state
                    .web_ide
                    .authenticate_cookie(session, server, secret, interaction)
                    .await
            }
            Self::Extension(secret) => {
                state
                    .web_ide
                    .authenticate_extension(session, server, secret, interaction)
                    .await
            }
        }
    }
}

async fn bootstrap(state: GetState, Path(server): Path<uuid::Uuid>) -> impl IntoResponse {
    if !state.web_ide.enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let nonce = random_nonce();
    let html = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="referrer" content="no-referrer"><meta name="viewport" content="width=device-width"><title>Opening Web IDE</title></head><body><p>Opening the secure Web IDE…</p><script nonce="{nonce}">(()=>{{const h=new URLSearchParams(location.hash.slice(1));const token=h.get('launch');history.replaceState(null,'',location.pathname);if(!token){{document.body.textContent='Missing launch credential.';return;}}fetch('./auth',{{method:'POST',credentials:'same-origin',headers:{{'content-type':'application/json'}},body:JSON.stringify({{token}})}}).then(async r=>{{if(!r.ok)throw new Error(await r.text());return r.json();}}).then(r=>location.replace(r.redirect)).catch(()=>{{document.body.textContent='This Web IDE launch is invalid, expired, or already used.';}});}})();</script></body></html>"#
    );
    let mut response = Html(html).into_response();
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&format!(
            "default-src 'none'; script-src 'nonce-{nonce}'; connect-src 'self'; style-src 'none'; img-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
        ))
        .unwrap(),
    );
    add_security_headers(response.headers_mut());
    let _ = server;
    response
}

async fn exchange(
    state: GetState,
    Path(server): Path<uuid::Uuid>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<LaunchRequest>,
) -> impl IntoResponse {
    if !same_origin(&state, &headers) || request.token.len() > 8192 {
        return StatusCode::FORBIDDEN.into_response();
    }

    let claims = match state.config.jwt.verify::<LaunchClaims>(&request.token) {
        Ok(claims) => claims,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let now = chrono::Utc::now().timestamp();
    let issuer = state.config.remote.trim_end_matches('/');
    let audience = state.config.web_ide.public_url.trim_end_matches('/');
    let valid = claims.base.validate(&state.config.jwt).await
        && claims.kind == "webide_launch"
        && claims.base.subject.as_deref() == Some("webide-launch")
        && claims.base.issuer.trim_end_matches('/') == issuer
        && claims
            .base
            .audience
            .iter()
            .any(|value| value.trim_end_matches('/') == audience)
        && claims.server_uuid == server
        && claims.permissions.has_permission(Permission::WebIdeAccess)
        && claims
            .base
            .issued_at
            .is_some_and(|issued| issued >= now - 120 && issued <= now)
        && claims
            .base
            .expiration_time
            .is_some_and(|expiry| expiry > now && expiry <= now + 120)
        && !claims.display_name.is_empty()
        && claims.display_name.len() <= 191
        && (!claims.can_use_console
            || (claims
                .permissions
                .has_permission(Permission::WebsocketConnect)
                && claims
                    .permissions
                    .has_permission(Permission::ControlConsole)));

    if !valid {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let expiry_seconds = claims.base.expiration_time.unwrap_or(now) - now;
    let cookie_secret = match state
        .web_ide
        .consume_launch(
            claims.session_uuid,
            server,
            claims.user_uuid,
            &claims.base.jwt_id,
            claims.permissions.clone(),
            claims.can_use_console,
            Instant::now() + Duration::from_secs(expiry_seconds.max(1) as u64),
        )
        .await
    {
        Ok(secret) => secret,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let cookie_name = cookie_name(claims.session_uuid);
    let cookie_path = format!("/api/servers/{server}/web-ide/s/{}/", claims.session_uuid);
    let cookie = format!(
        "{cookie_name}={cookie_secret}; Path={cookie_path}; Max-Age=86400; Secure; HttpOnly; SameSite=Strict"
    );
    let redirect = format!("{cookie_path}proxy/");
    let mut response = axum::Json(LaunchResponse { redirect }).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    add_security_headers(response.headers_mut());
    response
}

async fn heartbeat(
    state: GetState,
    Path((server, session)): Path<(uuid::Uuid, uuid::Uuid)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let (credential, _) = match session_credential(&headers, session) {
        Some(value) => value,
        None => return StatusCode::UNAUTHORIZED,
    };
    // A web extension worker is not guaranteed to have a browser Origin or
    // the HttpOnly launch cookie. Its bearer token is already scoped to this
    // exact server/session, so it is a safe CSRF-resistant alternative.
    if !credential_permits_request(&state, &headers, &credential) {
        return StatusCode::FORBIDDEN;
    }
    if credential
        .authenticate(&state, session, server, true)
        .await
        .is_some()
    {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::UNAUTHORIZED
    }
}

async fn browser_state(
    state: GetState,
    Path((server_uuid, session_uuid)): Path<(uuid::Uuid, uuid::Uuid)>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<BrowserStateRequest>,
) -> impl IntoResponse {
    browser_state_impl(state, server_uuid, session_uuid, headers, request, false).await
}

async fn browser_state_global(
    state: GetState,
    Path((server_uuid, session_uuid)): Path<(uuid::Uuid, uuid::Uuid)>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<BrowserStateRequest>,
) -> impl IntoResponse {
    browser_state_impl(state, server_uuid, session_uuid, headers, request, true).await
}

async fn browser_state_impl(
    state: GetState,
    server_uuid: uuid::Uuid,
    session_uuid: uuid::Uuid,
    headers: HeaderMap,
    request: BrowserStateRequest,
    user_scoped: bool,
) -> impl IntoResponse {
    if !same_origin(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let cookie = match cookie_secret(&headers, session_uuid) {
        Some(cookie) => cookie,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let session = match state
        .web_ide
        .authenticate_cookie(session_uuid, server_uuid, cookie, true)
        .await
    {
        Some(session) => session,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let supplied_token = headers
        .get("x-jexactyl-browser-storage")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 128);
    if supplied_token.is_none_or(|token| {
        !constant_time_eq::constant_time_eq(
            token.as_bytes(),
            session.browser_storage_token.as_bytes(),
        )
    }) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let operation = match request {
        BrowserStateRequest::SecretGet { key } => BrowserStateOperation::SecretGet { key },
        BrowserStateRequest::SecretSet { key, value } => {
            BrowserStateOperation::SecretSet { key, value }
        }
        BrowserStateRequest::SecretDelete { key } => BrowserStateOperation::SecretDelete { key },
        BrowserStateRequest::SecretKeys => BrowserStateOperation::SecretKeys,
        BrowserStateRequest::StorageSnapshot { database } => {
            BrowserStateOperation::StorageSnapshot { database }
        }
        BrowserStateRequest::StorageUpdate {
            database,
            insert,
            delete,
        } => BrowserStateOperation::StorageUpdate {
            database,
            insert,
            delete,
        },
        BrowserStateRequest::StorageClear { database } => {
            BrowserStateOperation::StorageClear { database }
        }
    };
    let mut response = match if user_scoped {
        state.web_ide.user_browser_state(&session, operation).await
    } else {
        state.web_ide.browser_state(&session, operation).await
    } {
        Ok(BrowserStateResult::Empty) => StatusCode::NO_CONTENT.into_response(),
        Ok(BrowserStateResult::Secret(value)) => axum::Json(BrowserStateResponse {
            value,
            ..Default::default()
        })
        .into_response(),
        Ok(BrowserStateResult::Keys(keys)) => axum::Json(BrowserStateResponse {
            keys: Some(keys),
            ..Default::default()
        })
        .into_response(),
        Ok(BrowserStateResult::Entries { entries, present }) => axum::Json(BrowserStateResponse {
            entries: Some(entries),
            present: Some(present),
            ..Default::default()
        })
        .into_response(),
        Err(error) => {
            tracing::warn!(
                server = %server_uuid,
                session = %session_uuid,
                user = %session.user_uuid,
                error = %error,
                "rejected Web IDE browser-state operation"
            );
            StatusCode::BAD_REQUEST.into_response()
        }
    };
    // SecretStorage and chat indexes must never be cached by a browser,
    // reverse proxy, or intermediary.  This also applies to empty/delete
    // responses so the endpoint has one consistent security policy.
    add_security_headers(response.headers_mut());
    response
}

/// Relays a BYOK OpenAI-compatible request without giving the networkless IDE
/// sidecar egress. Provider origins are fixed here so neither a user nor an
/// extension can turn Wings into an SSRF proxy. Keys and prompts are never
/// persisted or written to logs.
async fn agent_chat(
    state: GetState,
    Path((server_uuid, session_uuid)): Path<(uuid::Uuid, uuid::Uuid)>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<AgentChatRequest>,
) -> impl IntoResponse {
    let credential = match session_credential(&headers, session_uuid) {
        Some((credential, _)) => credential,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !credential_permits_request(&state, &headers, &credential) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let session = match credential
        .authenticate(&state, session_uuid, server_uuid, true)
        .await
    {
        Some(session) => session,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let _request_permit = match Arc::clone(&session.agent_requests).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "too many concurrent AI requests",
            )
                .into_response();
        }
    };

    let endpoint = match request.provider.as_str() {
        "openai" => "https://api.openai.com/v1/chat/completions",
        "openrouter" => "https://openrouter.ai/api/v1/chat/completions",
        _ => return (StatusCode::BAD_REQUEST, "unsupported AI provider").into_response(),
    };
    if request.api_key.is_empty()
        || request.api_key.len() > 512
        || request.api_key.contains(['\r', '\n'])
        || request.model.is_empty()
        || request.model.len() > 191
        || !request
            .model
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || ".:_-/".contains(value))
        || !request.messages.is_array()
        || request
            .messages
            .as_array()
            .is_some_and(|messages| messages.len() > 100)
        || request.tools.as_ref().is_some_and(|tools| {
            !tools.is_array() || tools.as_array().is_some_and(|tools| tools.len() > 64)
        })
    {
        return (StatusCode::BAD_REQUEST, "invalid AI request").into_response();
    }

    let mut payload = serde_json::json!({
        "model": request.model,
        "messages": request.messages,
        "stream": false,
    });
    if let Some(tools) = request
        .tools
        .filter(|tools| tools.as_array().is_some_and(|tools| !tools.is_empty()))
    {
        payload["tools"] = tools;
        payload["tool_choice"] = request
            .tool_choice
            .unwrap_or_else(|| serde_json::json!("auto"));
    }
    let encoded = match serde_json::to_vec(&payload) {
        Ok(encoded) if encoded.len() <= 2 * 1024 * 1024 => encoded,
        _ => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let mut upstream = client
        .post(endpoint)
        .header(header::AUTHORIZATION, format!("Bearer {}", request.api_key))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, "Jexactyl-WebIDE-Agent/1.0")
        .body(encoded);
    if request.provider == "openrouter" {
        upstream = upstream
            .header("HTTP-Referer", &state.config.web_ide.public_url)
            .header("X-Title", "Jexactyl Web IDE");
    }
    let upstream = match upstream.send().await {
        Ok(response) => response,
        Err(_) => return (StatusCode::BAD_GATEWAY, "AI provider request failed").into_response(),
    };
    let status = upstream.status();
    if upstream
        .content_length()
        .is_some_and(|length| length > 4 * 1024 * 1024)
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let mut bytes = Vec::new();
    let mut stream = upstream.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        };
        if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        bytes.extend_from_slice(&chunk);
    }
    tracing::info!(
        server = %server_uuid,
        session = %session_uuid,
        user = %session.user_uuid,
        provider = %request.provider,
        model = %request.model,
        status = %status,
        "completed Web IDE BYOK agent request"
    );

    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    add_security_headers(response.headers_mut());
    response.into_response()
}

/// Proxies the separate Jexactyl mod/plugin tool pack to the panel. A bearer
/// extension token is scoped to one session and is checked before this
/// handler can reach the node's panel client.
async fn addon_tools(
    state: GetState,
    Path((server_uuid, session_uuid)): Path<(uuid::Uuid, uuid::Uuid)>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<AddonToolRequest>,
) -> impl IntoResponse {
    let credential = match session_credential(&headers, session_uuid) {
        Some((credential, _)) => credential,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !credential_permits_request(&state, &headers, &credential) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !matches!(request.operation.as_str(), "status" | "search" | "install")
        || !request.input.is_object()
        || request.operation.len() > 16
    {
        return (StatusCode::BAD_REQUEST, "invalid addon tool request").into_response();
    }
    let session = match credential
        .authenticate(&state, session_uuid, server_uuid, true)
        .await
    {
        Some(session) => session,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let _request_permit = match Arc::clone(&session.agent_requests).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "too many concurrent addon requests",
            )
                .into_response();
        }
    };

    let (status, body) = match state
        .config
        .client
        .send_web_ide_addon_tool(
            server_uuid,
            session_uuid,
            &request.operation,
            &request.input,
        )
        .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                server = %server_uuid,
                session = %session_uuid,
                user = %session.user_uuid,
                error = %error,
                "failed to forward Web IDE addon tool request to panel"
            );
            let mut response = axum::Json(serde_json::json!({
                "success": false,
                "error_code": "PANEL_UNAVAILABLE",
                "message": "The panel could not complete the addon operation."
            }))
            .into_response();
            *response.status_mut() = StatusCode::BAD_GATEWAY;
            add_security_headers(response.headers_mut());
            return response;
        }
    };

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    add_security_headers(response.headers_mut());
    response.into_response()
}

async fn user_theme(
    state: GetState,
    Path((server_uuid, session_uuid)): Path<(uuid::Uuid, uuid::Uuid)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let credential = match session_credential(&headers, session_uuid) {
        Some((credential, _)) => credential,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !credential_permits_request(&state, &headers, &credential) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let session = match credential
        .authenticate(&state, session_uuid, server_uuid, true)
        .await
    {
        Some(session) => session,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let mut response = match state.web_ide.user_theme(&session).await {
        Ok(theme) => axum::Json(UserThemeResponse { theme }).into_response(),
        Err(error) => {
            tracing::warn!(
                server = %server_uuid,
                session = %session_uuid,
                user = %session.user_uuid,
                error = %error,
                "failed to read Web IDE user theme"
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    };
    add_security_headers(response.headers_mut());
    response
}

async fn update_user_theme(
    state: GetState,
    Path((server_uuid, session_uuid)): Path<(uuid::Uuid, uuid::Uuid)>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<UserThemeRequest>,
) -> impl IntoResponse {
    let credential = match session_credential(&headers, session_uuid) {
        Some((credential, _)) => credential,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !credential_permits_request(&state, &headers, &credential) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let session = match credential
        .authenticate(&state, session_uuid, server_uuid, true)
        .await
    {
        Some(session) => session,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let mut response = match state.web_ide.set_user_theme(&session, request.theme).await {
        Ok(()) => {
            tracing::info!(
                server = %server_uuid,
                session = %session_uuid,
                user = %session.user_uuid,
                "updated shared Web IDE user theme"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => {
            tracing::warn!(
                server = %server_uuid,
                session = %session_uuid,
                user = %session.user_uuid,
                error = %error,
                "rejected Web IDE user theme update"
            );
            StatusCode::BAD_REQUEST.into_response()
        }
    };
    add_security_headers(response.headers_mut());
    response
}

async fn user_theme_live(
    state: GetState,
    Path((server_uuid, session_uuid)): Path<(uuid::Uuid, uuid::Uuid)>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    let (credential, protocol) = match session_credential(&headers, session_uuid) {
        Some(value) => value,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !credential_permits_request(&state, &headers, &credential) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let session = match credential
        .authenticate(&state, session_uuid, server_uuid, true)
        .await
    {
        Some(session) => session,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let events = state.web_ide.subscribe_user_theme(session.user_uuid).await;
    let websocket = match protocol {
        Some(protocol) => websocket.protocols([protocol]),
        None => websocket,
    };
    websocket
        .max_message_size(1024)
        .max_frame_size(1024)
        .on_upgrade(move |socket| async move {
            let (mut client_tx, mut client_rx) = socket.split();
            let mut events = events;
            let mut authorization_tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tokio::select! {
                    event = events.recv() => {
                        let theme = match event {
                            Ok(theme) => theme,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        };
                        let payload = serde_json::json!({ "theme": theme }).to_string();
                        if client_tx.send(ClientMessage::Text(payload.into())).await.is_err() {
                            break;
                        }
                    }
                    message = client_rx.next() => {
                        match message {
                            Some(Ok(ClientMessage::Close(_))) | None => break,
                            _ => {}
                        }
                    }
                    _ = authorization_tick.tick() => {
                        if credential.authenticate(&state, session_uuid, server_uuid, false).await.is_none() {
                            break;
                        }
                    }
                }
            }
        })
        .into_response()
}

async fn terminal(
    state: GetState,
    Path((server_uuid, session_uuid)): Path<(uuid::Uuid, uuid::Uuid)>,
    headers: HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    let (credential, protocol) = match session_credential(&headers, session_uuid) {
        Some(value) => value,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !credential_permits_request(&state, &headers, &credential) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let session = match credential
        .authenticate(&state, session_uuid, server_uuid, true)
        .await
    {
        Some(session) if session.can_use_console => session,
        Some(_) => return StatusCode::FORBIDDEN.into_response(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let server = match state.server_manager.get_server(server_uuid).await {
        Some(server) => server,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let user_ip = state.config.find_ip(&headers, connect_info);

    let websocket = match protocol {
        Some(protocol) => websocket.protocols([protocol]),
        None => websocket,
    };
    websocket
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| async move {
            let user_uuid = session.user_uuid;
            tracing::info!(server = %server_uuid, session = %session_uuid, user = %session.user_uuid, "opened Web IDE terminal");
            bridge_wings_shell(
                state,
                socket,
                server,
                session,
                server_uuid,
                session_uuid,
                credential,
                user_ip,
            )
            .await;
            tracing::info!(server = %server_uuid, session = %session_uuid, user = %user_uuid, "closed Web IDE terminal");
        })
        .into_response()
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalInput {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

#[derive(Clone)]
enum TerminalAuthorization {
    Credential(SessionCredential),
    Local,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalTransport {
    Browser,
    NativeProcess,
}

impl TerminalAuthorization {
    async fn authenticate(
        &self,
        state: &GetState,
        session_uuid: uuid::Uuid,
        server_uuid: uuid::Uuid,
        interaction: bool,
    ) -> Option<crate::server::web_ide::WebIdeSession> {
        match self {
            Self::Credential(credential) => {
                credential
                    .authenticate(state, session_uuid, server_uuid, interaction)
                    .await
            }
            Self::Local => {
                state
                    .web_ide
                    .authenticate_local_terminal(session_uuid, server_uuid, interaction)
                    .await
            }
        }
    }
}

async fn bridge_wings_shell(
    state: GetState,
    socket: WebSocket,
    server: crate::server::Server,
    session: crate::server::web_ide::WebIdeSession,
    server_uuid: uuid::Uuid,
    session_uuid: uuid::Uuid,
    credential: SessionCredential,
    user_ip: std::net::IpAddr,
) {
    let (mut client_tx, mut client_rx) = socket.split();
    let (input_tx, input_rx) = mpsc::channel::<String>(64);
    let (output_tx, mut output_rx) = mpsc::channel::<Vec<u8>>(256);

    let receive = async move {
        while let Some(Ok(message)) = client_rx.next().await {
            let ClientMessage::Text(text) = message else {
                if matches!(message, ClientMessage::Close(_)) {
                    break;
                }
                continue;
            };
            match serde_json::from_str::<TerminalInput>(&text) {
                Ok(TerminalInput::Input { data }) if data.len() <= 64 * 1024 => {
                    if input_tx.send(data).await.is_err() {
                        break;
                    }
                }
                Ok(TerminalInput::Resize { cols, rows })
                    if (2..=500).contains(&cols) && (2..=300).contains(&rows) => {}
                _ => {}
            }
        }
    };
    let send = async move {
        while let Some(output) = output_rx.recv().await {
            if client_tx
                .send(ClientMessage::Binary(output.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    };
    let transport = async {
        tokio::select! {
            _ = receive => {},
            _ = send => {},
        }
    };
    let shell = run_wings_shell(
        state,
        server,
        session,
        server_uuid,
        session_uuid,
        TerminalAuthorization::Credential(credential),
        user_ip,
        input_rx,
        output_tx,
        TerminalTransport::Browser,
    );
    tokio::select! {
        _ = transport => {},
        _ = shell => {},
    }
}

/// Creates the private process-terminal endpoint used by native Copilot
/// Execute tools. The socket lives inside the exact session's 0700 runtime,
/// is owned by the non-root IDE uid, and never opens a node or container TCP
/// port.
pub(crate) async fn start_local_terminal_listener(
    state: GetState,
    server: crate::server::Server,
    server_uuid: uuid::Uuid,
    session_uuid: uuid::Uuid,
    socket_path: PathBuf,
) -> Result<(), anyhow::Error> {
    if tokio::fs::symlink_metadata(&socket_path).await.is_ok() {
        anyhow::bail!("Web IDE terminal socket path already exists");
    }
    let listener = tokio::net::UnixListener::bind(&socket_path)
        .context("failed to bind the Web IDE terminal socket")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let (uid, gid) = if state.config.system.user.rootless.enabled {
            (
                state.config.system.user.rootless.container_uid,
                state.config.system.user.rootless.container_gid,
            )
        } else {
            (state.config.system.user.uid, state.config.system.user.gid)
        };
        tokio::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).await?;
        std::os::unix::fs::chown(&socket_path, Some(uid), Some(gid))?;
    }

    let listener_state = state.clone();
    let listener_socket_path = socket_path.clone();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(
                        server = %server_uuid,
                        session = %session_uuid,
                        error = %error,
                        "Web IDE local terminal listener stopped"
                    );
                    break;
                }
            };
            let Some(session) = listener_state
                .web_ide
                .authenticate_local_terminal(session_uuid, server_uuid, true)
                .await
            else {
                break;
            };
            let connection_state = listener_state.clone();
            let connection_server = server.clone();
            tokio::spawn(async move {
                tracing::info!(
                    server = %server_uuid,
                    session = %session_uuid,
                    user = %session.user_uuid,
                    "opened Web IDE agent terminal"
                );
                bridge_local_wings_shell(
                    connection_state,
                    stream,
                    connection_server,
                    session.clone(),
                    server_uuid,
                    session_uuid,
                )
                .await;
                tracing::info!(
                    server = %server_uuid,
                    session = %session_uuid,
                    user = %session.user_uuid,
                    "closed Web IDE agent terminal"
                );
            });
        }
        let _ = tokio::fs::remove_file(listener_socket_path).await;
    });
    if !state
        .web_ide
        .attach_terminal_socket_task(session_uuid, task.abort_handle())
        .await
    {
        task.abort();
        anyhow::bail!("Web IDE session ended before its terminal socket was ready");
    }
    Ok(())
}

const LOCAL_ADDON_REQUEST_LIMIT: usize = 64 * 1024;
const LOCAL_ADDON_RESPONSE_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Serialize)]
struct LocalAddonToolResponse {
    status: u16,
    body: serde_json::Value,
}

/// Creates the private process-to-Wings endpoint used by the Jexactyl
/// mod/plugin Copilot tools. The code-server sidecar cannot hairpin to the
/// node's public HTTPS address; this socket keeps the same isolation while
/// avoiding any host-network or Docker-socket access.
pub(crate) async fn start_local_addon_tools_listener(
    state: GetState,
    server: crate::server::Server,
    server_uuid: uuid::Uuid,
    session_uuid: uuid::Uuid,
    socket_path: PathBuf,
) -> Result<(), anyhow::Error> {
    if tokio::fs::symlink_metadata(&socket_path).await.is_ok() {
        anyhow::bail!("Web IDE addon-tools socket path already exists");
    }
    let listener = tokio::net::UnixListener::bind(&socket_path)
        .context("failed to bind the Web IDE addon-tools socket")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let (uid, gid) = if state.config.system.user.rootless.enabled {
            (
                state.config.system.user.rootless.container_uid,
                state.config.system.user.rootless.container_gid,
            )
        } else {
            (state.config.system.user.uid, state.config.system.user.gid)
        };
        tokio::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).await?;
        std::os::unix::fs::chown(&socket_path, Some(uid), Some(gid))?;
    }

    let listener_state = state.clone();
    let listener_socket_path = socket_path.clone();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(
                        server = %server_uuid,
                        session = %session_uuid,
                        error = %error,
                        "Web IDE addon-tools listener stopped"
                    );
                    break;
                }
            };
            let connection_state = listener_state.clone();
            let connection_server = server.clone();
            tokio::spawn(async move {
                bridge_local_addon_tools(
                    connection_state,
                    stream,
                    connection_server,
                    server_uuid,
                    session_uuid,
                )
                .await;
            });
        }
        let _ = tokio::fs::remove_file(listener_socket_path).await;
    });
    if !state
        .web_ide
        .attach_addon_tools_socket_task(session_uuid, task.abort_handle())
        .await
    {
        task.abort();
        anyhow::bail!("Web IDE session ended before its addon-tools socket was ready");
    }
    Ok(())
}

async fn read_local_addon_request(
    reader: &mut tokio::net::unix::OwnedReadHalf,
) -> Result<Option<Vec<u8>>, anyhow::Error> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok((!bytes.is_empty()).then_some(bytes));
        }
        let newline = chunk[..read].iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(read);
        if bytes.len().saturating_add(count) > LOCAL_ADDON_REQUEST_LIMIT {
            anyhow::bail!("Web IDE addon-tools request exceeded the size limit");
        }
        bytes.extend_from_slice(&chunk[..count]);
        if newline.is_some() {
            return Ok(Some(bytes));
        }
    }
}

async fn send_local_addon_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    status: u16,
    body: serde_json::Value,
) -> Result<(), anyhow::Error> {
    let response = LocalAddonToolResponse { status, body };
    let mut encoded = serde_json::to_vec(&response)?;
    if encoded.len() > LOCAL_ADDON_RESPONSE_LIMIT {
        encoded = serde_json::to_vec(&LocalAddonToolResponse {
            status: StatusCode::BAD_GATEWAY.as_u16(),
            body: serde_json::json!({
                "success": false,
                "error_code": "RESPONSE_TOO_LARGE",
                "message": "The panel response exceeded the Web IDE tool limit."
            }),
        })?;
    }
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.shutdown().await?;
    Ok(())
}

async fn bridge_local_addon_tools(
    state: GetState,
    stream: tokio::net::UnixStream,
    server: crate::server::Server,
    server_uuid: uuid::Uuid,
    session_uuid: uuid::Uuid,
) {
    let (mut reader, mut writer) = stream.into_split();
    let request = match read_local_addon_request(&mut reader).await {
        Ok(Some(bytes)) => match serde_json::from_slice::<AddonToolRequest>(&bytes) {
            Ok(request) => request,
            Err(_) => {
                let _ = send_local_addon_response(
                    &mut writer,
                    StatusCode::BAD_REQUEST.as_u16(),
                    serde_json::json!({
                        "success": false,
                        "error_code": "INVALID_REQUEST",
                        "message": "The addon tool request was invalid."
                    }),
                )
                .await;
                return;
            }
        },
        Ok(None) | Err(_) => {
            let _ = send_local_addon_response(
                &mut writer,
                StatusCode::BAD_REQUEST.as_u16(),
                serde_json::json!({
                    "success": false,
                    "error_code": "INVALID_REQUEST",
                    "message": "The addon tool request was invalid."
                }),
            )
            .await;
            return;
        }
    };

    if !matches!(request.operation.as_str(), "status" | "search" | "install")
        || request.operation.len() > 16
        || !request.input.is_object()
    {
        let _ = send_local_addon_response(
            &mut writer,
            StatusCode::BAD_REQUEST.as_u16(),
            serde_json::json!({
                "success": false,
                "error_code": "INVALID_REQUEST",
                "message": "The addon tool operation or input was invalid."
            }),
        )
        .await;
        return;
    }

    let Some(session) = state
        .web_ide
        .authenticate_local_addon_tools(session_uuid, server_uuid, true)
        .await
    else {
        let _ = send_local_addon_response(
            &mut writer,
            StatusCode::UNAUTHORIZED.as_u16(),
            serde_json::json!({
                "success": false,
                "error_code": "SESSION_UNAVAILABLE",
                "message": "The Web IDE session is no longer authorized."
            }),
        )
        .await;
        return;
    };
    let _request_permit = match Arc::clone(&session.agent_requests).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            let _ = send_local_addon_response(
                &mut writer,
                StatusCode::TOO_MANY_REQUESTS.as_u16(),
                serde_json::json!({
                    "success": false,
                    "error_code": "TOO_MANY_REQUESTS",
                    "message": "Too many concurrent addon operations."
                }),
            )
            .await;
            return;
        }
    };

    let (status, body) = match state
        .config
        .client
        .send_web_ide_addon_tool(
            server_uuid,
            session_uuid,
            &request.operation,
            &request.input,
        )
        .await
    {
        Ok((status, body)) => {
            let body = serde_json::from_str::<serde_json::Value>(&body).unwrap_or_else(|_| {
                serde_json::json!({
                    "success": false,
                    "error_code": "INVALID_PANEL_RESPONSE",
                    "message": "The panel returned an invalid addon response."
                })
            });
            (status, body)
        }
        Err(error) => {
            tracing::warn!(
                server = %server_uuid,
                session = %session_uuid,
                user = %session.user_uuid,
                error = %error,
                "failed to forward local Web IDE addon tool request to panel"
            );
            (
                StatusCode::BAD_GATEWAY.as_u16(),
                serde_json::json!({
                    "success": false,
                    "error_code": "PANEL_UNAVAILABLE",
                    "message": "The panel could not complete the addon operation."
                }),
            )
        }
    };

    let _ = send_local_addon_response(&mut writer, status, body).await;
    let _ = server;
}

async fn bridge_local_wings_shell(
    state: GetState,
    stream: tokio::net::UnixStream,
    server: crate::server::Server,
    session: crate::server::web_ide::WebIdeSession,
    server_uuid: uuid::Uuid,
    session_uuid: uuid::Uuid,
) {
    let (mut reader, mut writer) = stream.into_split();
    let (input_tx, input_rx) = mpsc::channel::<String>(64);
    let (output_tx, mut output_rx) = mpsc::channel::<Vec<u8>>(256);
    let receive = async move {
        let mut buffer = vec![0u8; 8192];
        loop {
            let read = match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let input = String::from_utf8_lossy(&buffer[..read]).into_owned();
            if input_tx.send(input).await.is_err() {
                break;
            }
        }
    };
    let send = async move {
        while let Some(output) = output_rx.recv().await {
            if writer.write_all(&output).await.is_err() {
                break;
            }
        }
    };
    let transport = async {
        tokio::select! {
            _ = receive => {},
            _ = send => {},
        }
    };
    let shell = run_wings_shell(
        state,
        server,
        session,
        server_uuid,
        session_uuid,
        TerminalAuthorization::Local,
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        input_rx,
        output_tx,
        TerminalTransport::NativeProcess,
    );
    tokio::select! {
        _ = transport => {},
        _ = shell => {},
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_wings_shell(
    state: GetState,
    server: crate::server::Server,
    session: crate::server::web_ide::WebIdeSession,
    server_uuid: uuid::Uuid,
    session_uuid: uuid::Uuid,
    authorization: TerminalAuthorization,
    user_ip: std::net::IpAddr,
    mut input_rx: mpsc::Receiver<String>,
    output_tx: mpsc::Sender<Vec<u8>>,
    transport: TerminalTransport,
) {
    let mut daemon_events = server.websocket.subscribe();
    // The server websocket bus carries daemon/status messages, but the actual
    // container stdout stream is a separate broadcast channel.  Reading only
    // the former makes commands appear to be sent successfully while their
    // output is invisible until a second terminal requests a fresh log tail.
    // Keep a dedicated stdout pump attached to the same receiver used by the
    // normal Wings SSH shell and re-attach it when the game container starts
    // or is replaced.
    let (stdout_sender, mut stdout_receiver) =
        tokio::sync::mpsc::channel::<Arc<compact_str::CompactString>>(256);
    let stdout_server = server.clone();
    let stdout_pump = tokio::spawn(async move {
        let mut container_stdout = None;
        loop {
            if container_stdout.is_none() {
                container_stdout = stdout_server.container_stdout().await;
                if container_stdout.is_none() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            }

            let result = container_stdout
                .as_mut()
                .expect("container stdout receiver was just checked")
                .recv()
                .await;
            match result {
                Ok(line) => {
                    if stdout_sender.send(line).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // The container was stopped or replaced.  Re-check the
                    // server so a subsequent power start is streamed too.
                    container_stdout = None;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // The daemon's normal console policy already bounds this
                    // channel.  Continue with the newest output rather than
                    // closing an otherwise authorized terminal.
                }
            }
        }
    });
    let mut shell = crate::ssh::shell::ShellSession {
        state: Arc::clone(&state),
        server: server.clone(),
        user_ip,
        user_uuid: session.user_uuid,
        mode: crate::ssh::shell::ShellMode::Normal,
        permission_override: Some(session.permissions.clone()),
        activity_source: "webide",
    };
    let mut current_line = String::with_capacity(1024);
    let mut authorization_tick = tokio::time::interval(Duration::from_secs(15));

    let prelude = ansi_term::Color::Yellow
        .bold()
        .paint(format!("[{} Daemon]:", state.config.app_name));
    if output_tx
        .send(
            format!(
                "{prelude} Wings shell connected; server marked as {}. Type `.wings help` for daemon commands.\r\n\x1b[2K",
                server.state.get_state().to_str()
            )
            .into_bytes()
        )
        .await
        .is_err()
    {
        return;
    }

    // Interactive browser terminals need the normal log tail. Native Copilot
    // Execute terminals are short-lived command transports; replaying the log
    // tail there can complete the tool on stale output before its command is
    // dispatched and floods the result card with unrelated server history.
    if transport == TerminalTransport::Browser
        && (server.state.get_state() != crate::server::state::ServerState::Offline
            || state.config.api.send_offline_server_logs)
    {
        let mut logs = server
            .read_log(Some(state.config.system.websocket_log_count))
            .await;
        while let Some(Ok(line)) = logs.next().await {
            if output_tx.send(line.to_string().into_bytes()).await.is_err() {
                stdout_pump.abort();
                let _ = stdout_pump.await;
                return;
            }
        }
    }

    loop {
        tokio::select! {
            line = stdout_receiver.recv() => {
                let Some(line) = line else { break; };
                if output_tx
                    .send(format!("{}\r\n\x1b[2K", line).into_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            event = daemon_events.recv() => {
                use crate::server::websocket::WebsocketEvent;
                let text = match event {
                    Ok(message) => match message.event {
                        WebsocketEvent::ServerConsoleOutput | WebsocketEvent::ServerDaemonMessage => {
                            Some(format!("{}\r\n\x1b[2K", message.args.join(" ")))
                        }
                        WebsocketEvent::ServerStatus => {
                            Some(format!("{prelude} Server marked as {}...\r\n\x1b[2K", message.args.first().map(|value| value.as_ref()).unwrap_or("unknown")))
                        }
                        WebsocketEvent::ServerInstallOutput
                            if session.permissions.has_permission(Permission::AdminWebsocketInstall) => {
                                Some(format!("{}\r\n\x1b[2K", message.args.join(" ")))
                            }
                        WebsocketEvent::ServerTransferLogs
                            if session.permissions.has_permission(Permission::AdminWebsocketTransfer) => {
                                Some(format!("{}\r\n\x1b[2K", message.args.join(" ")))
                            }
                        _ => None,
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => None,
                };
                if let Some(text) = text
                    && output_tx.send(text.into_bytes()).await.is_err()
                {
                    break;
                }
            }
            message = input_rx.recv() => {
                match message {
                    Some(data) if data.len() <= 64 * 1024 => {
                        if authorization.authenticate(&state, session_uuid, server_uuid, true).await.is_none() { break; }
                        for character in data.chars() {
                            match character {
                                '\r' | '\n' => {
                                    let line = std::mem::take(&mut current_line);
                                    // Native Execute terminals do not locally
                                    // echo their stdin. Broadcast one labeled
                                    // echo so the already-open Jexactyl terminal
                                    // and panel console visibly confirm exactly
                                    // what the agent submitted.
                                    if transport == TerminalTransport::NativeProcess && !line.is_empty() {
                                        server.websocket.send(
                                            crate::server::websocket::WebsocketMessage::new(
                                                crate::server::websocket::WebsocketEvent::ServerConsoleOutput,
                                                [compact_str::format_compact!("[Web IDE Agent] > {line}")].into(),
                                            ),
                                        ).ok();
                                    }
                                    let output = if line.is_empty() {
                                        b"\r\n".to_vec()
                                    } else {
                                        shell.execute_webide_line(&line).await
                                    };
                                    if output_tx.send(output).await.is_err() { break; }
                                }
                                '\u{8}' | '\u{7f}' => {
                                    if current_line.pop().is_some()
                                        && transport == TerminalTransport::Browser
                                        && output_tx.send(b"\x08 \x08".to_vec()).await.is_err()
                                    { return; }
                                }
                                value if !value.is_control() && current_line.len() < 1024 => {
                                    current_line.push(value);
                                    if transport == TerminalTransport::Browser
                                        && output_tx.send(value.to_string().into_bytes()).await.is_err()
                                    { return; }
                                }
                                _ => {}
                            }
                        }
                    }
                    None => break,
                    _ => {}
                }
            }
            _ = authorization_tick.tick() => {
                if authorization.authenticate(&state, session_uuid, server_uuid, false).await.is_none() { break; }
            }
        }
    }

    stdout_pump.abort();
    let _ = stdout_pump.await;
}

async fn console(
    state: GetState,
    Path((server_uuid, session_uuid)): Path<(uuid::Uuid, uuid::Uuid)>,
    headers: HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    let (credential, protocol) = match session_credential(&headers, session_uuid) {
        Some(value) => value,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !credential_permits_request(&state, &headers, &credential) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let session = match credential
        .authenticate(&state, session_uuid, server_uuid, true)
        .await
    {
        Some(session) if session.can_use_console => session,
        _ => return StatusCode::FORBIDDEN.into_response(),
    };
    let server = match state.server_manager.get_server(server_uuid).await {
        Some(server) => server,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let container = match server.container.read().await.as_ref() {
        Some(container) => Arc::clone(container),
        None => return StatusCode::CONFLICT.into_response(),
    };
    let user_ip = Some(state.config.find_ip(&headers, connect_info));
    let server_for_activity = server.clone();

    let websocket = match protocol {
        Some(protocol) => websocket.protocols([protocol]),
        None => websocket,
    };
    websocket.on_upgrade(move |socket| async move {
        let (mut client_tx, mut client_rx) = socket.split();
        let mut output = container.stdout.resubscribe();
        let input = container.stdin.clone();
        let mut authorization_tick = tokio::time::interval(Duration::from_secs(15));
        tracing::info!(server = %server_uuid, session = %session_uuid, user = %session.user_uuid, "opened Web IDE console");
        loop {
            tokio::select! {
                line = output.recv() => {
                    match line {
                        Ok(line) if client_tx.send(ClientMessage::Text(line.to_string().into())).await.is_err() => break,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        _ => {}
                    }
                }
                message = client_rx.next() => {
                    match message {
                        Some(Ok(ClientMessage::Text(command))) if command.len() <= 4096 && !command.contains('\0') => {
                            if credential.authenticate(&state, session_uuid, server_uuid, true).await.is_none() { break; }
                            let raw_command = command.trim_end_matches(['\r', '\n']).to_string();
                            let command = format!("{raw_command}\n");
                            if input.send(command.into()).await.is_err() { break; }
                            server_for_activity.activity.log_activity(Activity {
                                event: ActivityEvent::ConsoleCommand,
                                user: Some(session.user_uuid),
                                ip: user_ip,
                                metadata: Some(serde_json::json!({ "command_length": raw_command.len() })),
                                schedule: None,
                                timestamp: chrono::Utc::now(),
                            }).await;
                        }
                        Some(Ok(ClientMessage::Close(_))) | None => break,
                        _ => {}
                    }
                }
                _ = authorization_tick.tick() => {
                    if credential.authenticate(&state, session_uuid, server_uuid, false).await.is_none() { break; }
                }
            }
        }
        tracing::info!(server = %server_uuid, session = %session_uuid, user = %session.user_uuid, "closed Web IDE console");
    }).into_response()
}

#[axum::debug_handler(state = crate::routes::State)]
async fn collaboration(
    state: GetState,
    Path((server_uuid, session_uuid, room_name)): Path<(uuid::Uuid, uuid::Uuid, String)>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    let (credential, protocol) = match session_credential(&headers, session_uuid) {
        Some(value) => value,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !credential_permits_request(&state, &headers, &credential) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let session = match credential
        .authenticate(&state, session_uuid, server_uuid, true)
        .await
    {
        Some(session) => session,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let server = match state.server_manager.get_server(server_uuid).await {
        Some(server) => server,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let room = match state.web_ide.collaboration_room(&server, &room_name).await {
        Ok(room) => room,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let websocket = match protocol {
        Some(protocol) => websocket.protocols([protocol]),
        None => websocket,
    };
    websocket
        .max_message_size(2 * 1024 * 1024)
        .max_frame_size(2 * 1024 * 1024)
        .on_upgrade(move |socket| async move {
            let (sink, stream) = socket.split();
            let sink = Arc::new(Mutex::new(AxumSink::from(sink)));
            let stream = AxumStream::from(stream);
            let connection_uuid = uuid::Uuid::new_v4();
            let claimed_client = Arc::new(StdMutex::new(None));
            let subscription = room.group.subscribe_with(
                sink,
                stream,
                AuthenticatedCollaborationProtocol {
                    display_name: session.display_name.clone(),
                    connection_uuid,
                    claimed_client: Arc::clone(&claimed_client),
                    awareness_clients: Arc::clone(&room.awareness_clients),
                },
            );
            let authorization = async {
                loop {
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    if credential.authenticate(&state, session_uuid, server_uuid, false).await.is_none() {
                        return;
                    }
                }
            };
            tracing::info!(server = %server_uuid, session = %session_uuid, user = %session.user_uuid, room = %room_name, "joined Web IDE collaboration room");
            tokio::select! {
                _ = subscription.completed() => {},
                _ = authorization => {},
                _ = room.limit_notifier.notified() => {
                    tracing::warn!(server = %server_uuid, room = %room_name, "closed collaboration room after document exceeded its configured limit");
                },
            }
            let claimed_client_id = *claimed_client
                .lock()
                .expect("collaboration client lock poisoned");
            if let Some(client_id) = claimed_client_id {
                let removed = {
                    let mut clients = room.awareness_clients.lock().expect("collaboration client map lock poisoned");
                    if clients.get(&client_id) == Some(&connection_uuid) {
                        clients.remove(&client_id);
                        true
                    } else {
                        false
                    }
                };
                if removed {
                    room.group.awareness().write().await.remove_state(client_id);
                }
            }
            tracing::info!(server = %server_uuid, session = %session_uuid, user = %session.user_uuid, room = %room_name, "left Web IDE collaboration room");
        })
        .into_response()
}

async fn proxy(
    state: GetState,
    Path(path): Path<ProxyPath>,
    request: Request,
) -> impl IntoResponse {
    let server = path.server;
    let session = path.session;
    let _wildcard_path = path.path;
    let is_websocket = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let (mut parts, body) = request.into_parts();
    let websocket = if is_websocket {
        WebSocketUpgrade::from_request_parts(&mut parts, &state.0)
            .await
            .ok()
    } else {
        None
    };
    let request = Request::from_parts(parts, body);
    let secret = match cookie_secret(request.headers(), session) {
        Some(secret) => secret,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if request.method() != Method::GET
        && request.method() != Method::HEAD
        && !same_origin(&state, request.headers())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let session = match state
        .web_ide
        .authenticate_cookie(session, server, secret, false)
        .await
    {
        Some(session) => session,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if let Some(mut websocket) = websocket {
        if !same_origin(&state, request.headers()) {
            return StatusCode::FORBIDDEN.into_response();
        }
        let protocol = request
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok())
            .and_then(|protocols| {
                protocols
                    .split(',')
                    .map(str::trim)
                    .find(|value| !value.is_empty())
                    .map(str::to_string)
            });
        if let Some(protocol) = &protocol {
            websocket = websocket.protocols(std::iter::once(protocol.clone()).collect::<Vec<_>>());
        }
        let upstream_path = upstream_path_and_query(request.uri(), server, session.uuid);
        let socket_path = session.socket_path.clone();
        return websocket
            .max_message_size(16 * 1024 * 1024)
            .max_frame_size(4 * 1024 * 1024)
            .on_upgrade(move |client| proxy_websocket(client, socket_path, upstream_path, protocol))
            .into_response();
    }

    let method = request.method().clone();
    let uri = request.uri().clone();
    let request_headers = request.headers().clone();
    let body =
        match body::to_bytes(request.into_body(), state.config.web_ide.max_request_bytes).await {
            Ok(body) => body,
            Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        };

    let client = match reqwest::Client::builder()
        .unix_socket(session.socket_path.clone())
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let upstream_path = upstream_path_and_query(&uri, server, session.uuid);
    let upstream_uri = format!("http://localhost{upstream_path}");
    let mut upstream = client.request(method, upstream_uri).body(body);
    for (name, value) in request_headers.iter() {
        if !is_request_header_blocked(name) {
            upstream = upstream.header(name, value);
        }
    }
    // The browser Host belongs to Wings, not the Unix-socket service. Keeping
    // it caused code-server to interpret nested Axum paths as its own routes.
    upstream = upstream
        .header(header::HOST, "localhost")
        // The root workbench document is annotated below with a stable
        // server/user storage scope. Request an uncompressed response so a
        // proxy-side HTML marker cannot be inserted into compressed bytes.
        .header(header::ACCEPT_ENCODING, "identity")
        .header("x-forwarded-proto", "https")
        .header(
            "x-forwarded-prefix",
            format!("/api/servers/{server}/web-ide/s/{}/proxy", session.uuid),
        );

    let upstream = match upstream.send().await {
        Ok(response) => response,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let inject_scope = upstream_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"));
    let inject_workbench = is_browser_workbench_script(&upstream_path);
    let buffer_response = inject_scope || inject_workbench;
    let mut response = if buffer_response {
        // Bind browser-backed VS Code storage to the authenticated session.
        // HTML metadata remains a compatibility fallback, while the actual
        // workbench bundle receives the capability directly because
        // code-server redirects can replace the annotated root document.
        // Keep both response types bounded before buffering them.
        match upstream.bytes().await {
            Ok(bytes) if bytes.len() <= 32 * 1024 * 1024 => {
                let scope = browser_storage_scope(server, session.user_uuid);
                let global_scope = browser_user_storage_scope(session.user_uuid);
                let api = format!(
                    "/api/servers/{server}/web-ide/s/{}/browser-state",
                    session.uuid
                );
                let global_api = format!("{api}/global");
                let body = if inject_workbench {
                    match inject_browser_storage_configuration(
                        bytes.to_vec(),
                        &scope,
                        &global_scope,
                        &api,
                        &global_api,
                        &session.browser_storage_token,
                    ) {
                        Ok(body) => body,
                        Err(error) => {
                            tracing::error!(
                                server = %server,
                                session = %session.uuid,
                                error = %error,
                                "failed to bind Web IDE browser storage capability"
                            );
                            return StatusCode::BAD_GATEWAY.into_response();
                        }
                    }
                } else {
                    inject_browser_storage_scope(
                        bytes.to_vec(),
                        &scope,
                        &global_scope,
                        &api,
                        &global_api,
                        &session.browser_storage_token,
                    )
                };
                Response::new(Body::from(body))
            }
            Ok(_) => return StatusCode::BAD_GATEWAY.into_response(),
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        }
    } else {
        Response::new(Body::from_stream(upstream.bytes_stream()))
    };
    *response.status_mut() = status;
    for (name, value) in upstream_headers.iter() {
        if buffer_response && name == header::CONTENT_LENGTH {
            continue;
        }
        if !is_response_header_blocked(name) {
            response.headers_mut().append(name, value.clone());
        }
    }
    add_security_headers(response.headers_mut());
    response.into_response()
}

async fn proxy_websocket(
    client: WebSocket,
    socket_path: std::path::PathBuf,
    path: String,
    protocol: Option<String>,
) {
    let stream = match tokio::net::UnixStream::connect(socket_path).await {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let mut request = match format!("ws://localhost{path}").into_client_request() {
        Ok(request) => request,
        Err(_) => return,
    };
    request
        .headers_mut()
        .insert(header::ORIGIN, HeaderValue::from_static("http://localhost"));
    if let Some(protocol) = protocol.and_then(|value| HeaderValue::from_str(&value).ok()) {
        request
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
    }
    let upstream = match tokio_tungstenite::client_async(request, stream).await {
        Ok((upstream, _)) => upstream,
        Err(_) => return,
    };

    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();
    let client_to_upstream = async {
        while let Some(Ok(message)) = client_rx.next().await {
            let message = match message {
                ClientMessage::Text(value) => {
                    tokio_tungstenite::tungstenite::Message::Text(value.to_string().into())
                }
                ClientMessage::Binary(value) => {
                    tokio_tungstenite::tungstenite::Message::Binary(value)
                }
                ClientMessage::Ping(value) => tokio_tungstenite::tungstenite::Message::Ping(value),
                ClientMessage::Pong(value) => tokio_tungstenite::tungstenite::Message::Pong(value),
                ClientMessage::Close(_) => tokio_tungstenite::tungstenite::Message::Close(None),
            };
            if upstream_tx.send(message).await.is_err() {
                break;
            }
        }
    };
    let upstream_to_client = async {
        while let Some(Ok(message)) = upstream_rx.next().await {
            let message = match message {
                tokio_tungstenite::tungstenite::Message::Text(value) => {
                    ClientMessage::Text(value.to_string().into())
                }
                tokio_tungstenite::tungstenite::Message::Binary(value) => {
                    ClientMessage::Binary(value)
                }
                tokio_tungstenite::tungstenite::Message::Ping(value) => ClientMessage::Ping(value),
                tokio_tungstenite::tungstenite::Message::Pong(value) => ClientMessage::Pong(value),
                tokio_tungstenite::tungstenite::Message::Close(_) => ClientMessage::Close(None),
                tokio_tungstenite::tungstenite::Message::Frame(_) => continue,
            };
            if client_tx.send(message).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = client_to_upstream => {},
        _ = upstream_to_client => {},
    }
}

pub fn public_router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .route("/servers/{server}/web-ide/bootstrap", get(bootstrap))
        .route("/servers/{server}/web-ide/auth", post(exchange))
        .route(
            "/servers/{server}/web-ide/s/{session}/heartbeat",
            post(heartbeat),
        )
        .route(
            "/servers/{server}/web-ide/s/{session}/browser-state",
            post(browser_state).layer(DefaultBodyLimit::max(4 * 1024 * 1024)),
        )
        .route(
            "/servers/{server}/web-ide/s/{session}/browser-state/global",
            post(browser_state_global).layer(DefaultBodyLimit::max(4 * 1024 * 1024)),
        )
        .route(
            "/servers/{server}/web-ide/s/{session}/terminal",
            get(terminal),
        )
        .route(
            "/servers/{server}/web-ide/s/{session}/agent/chat",
            post(agent_chat),
        )
        .route(
            "/servers/{server}/web-ide/s/{session}/addon-tools",
            post(addon_tools).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/servers/{server}/web-ide/s/{session}/profile/theme",
            get(user_theme)
                .post(update_user_theme)
                .layer(DefaultBodyLimit::max(4 * 1024)),
        )
        .route(
            "/servers/{server}/web-ide/s/{session}/profile/theme/live",
            get(user_theme_live),
        )
        .route(
            "/servers/{server}/web-ide/s/{session}/console",
            get(console),
        )
        .route(
            "/servers/{server}/web-ide/s/{session}/collaboration/{room}",
            get(collaboration),
        )
        .route("/servers/{server}/web-ide/s/{session}/proxy/", any(proxy))
        .route(
            "/servers/{server}/web-ide/s/{session}/proxy/{*path}",
            any(proxy),
        )
        .with_state(state.clone())
}

fn cookie_name(session: uuid::Uuid) -> String {
    format!("jexide_{}", session.simple())
}

fn cookie_secret(headers: &HeaderMap, session: uuid::Uuid) -> Option<&str> {
    let name = cookie_name(session);
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn session_credential(
    headers: &HeaderMap,
    session: uuid::Uuid,
) -> Option<(SessionCredential, Option<String>)> {
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| value.len() <= 128 && !value.is_empty())
    {
        return Some((SessionCredential::Extension(token.to_string()), None));
    }
    if let Some(protocol) = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .split(',')
                .map(str::trim)
                .find(|protocol| protocol.starts_with("jexactyl-auth.") && protocol.len() <= 160)
        })
        .and_then(|protocol| {
            protocol
                .strip_prefix("jexactyl-auth.")
                .filter(|token| !token.is_empty())
                .map(|token| {
                    (
                        SessionCredential::Extension(token.to_string()),
                        Some(protocol.to_string()),
                    )
                })
        })
    {
        return Some(protocol);
    }
    cookie_secret(headers, session)
        .map(|cookie| (SessionCredential::Cookie(cookie.to_string()), None))
}

fn same_origin(state: &GetState, headers: &HeaderMap) -> bool {
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| {
            origin.trim_end_matches('/') == state.config.web_ide.public_url.trim_end_matches('/')
        })
}

fn credential_permits_request(
    state: &GetState,
    headers: &HeaderMap,
    credential: &SessionCredential,
) -> bool {
    same_origin(state, headers) || matches!(credential, SessionCredential::Extension(_))
}

fn is_request_header_blocked(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "accept-encoding"
            | "cookie"
            | "connection"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "forwarded"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-real-ip"
            | "te"
            | "trailer"
            | "transfer-encoding"
    )
}

fn is_response_header_blocked(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "set-cookie"
            | "connection"
            | "upgrade"
            | "proxy-authenticate"
            | "te"
            | "trailer"
            | "transfer-encoding"
    )
}

fn add_security_headers(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    // VS Code webviews are same-origin frames; SAMEORIGIN preserves them while
    // preventing another site from embedding an authenticated IDE session.
    headers.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
}

fn browser_storage_scope(server: uuid::Uuid, user: uuid::Uuid) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"jexactyl-webide-browser-storage:v1:");
    hasher.update(server.as_bytes());
    hasher.update(b":");
    hasher.update(user.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn browser_user_storage_scope(user: uuid::Uuid) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"jexactyl-webide-user-browser-storage:v1:");
    hasher.update(user.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize())
}

const BROWSER_STORAGE_SCOPE_PLACEHOLDER: &str = "__JEXACTYL_WEBIDE_STORAGE_SCOPE__";
const BROWSER_GLOBAL_STORAGE_SCOPE_PLACEHOLDER: &str = "__JEXACTYL_WEBIDE_GLOBAL_STORAGE_SCOPE__";
const BROWSER_STORAGE_API_PLACEHOLDER: &str = "__JEXACTYL_WEBIDE_STORAGE_API__";
const BROWSER_GLOBAL_STORAGE_API_PLACEHOLDER: &str = "__JEXACTYL_WEBIDE_GLOBAL_STORAGE_API__";
const BROWSER_STORAGE_TOKEN_PLACEHOLDER: &str = "__JEXACTYL_WEBIDE_STORAGE_TOKEN__";

fn is_browser_workbench_script(path_and_query: &str) -> bool {
    let path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(path, _)| path);
    path.ends_with("/static/out/vs/code/browser/workbench/workbench.js")
        || path.ends_with("/static/out/vs/workbench/workbench.web.main.internal.js")
}

fn inject_browser_storage_configuration(
    body: Vec<u8>,
    scope: &str,
    global_scope: &str,
    api: &str,
    global_api: &str,
    token: &str,
) -> Result<Vec<u8>, &'static str> {
    let javascript = String::from_utf8(body).map_err(|_| "workbench bundle is not UTF-8")?;
    if !javascript.contains(BROWSER_STORAGE_SCOPE_PLACEHOLDER)
        || !javascript.contains(BROWSER_GLOBAL_STORAGE_SCOPE_PLACEHOLDER)
        || !javascript.contains(BROWSER_STORAGE_API_PLACEHOLDER)
        || !javascript.contains(BROWSER_GLOBAL_STORAGE_API_PLACEHOLDER)
        || !javascript.contains(BROWSER_STORAGE_TOKEN_PLACEHOLDER)
    {
        return Err("workbench bundle is missing a browser storage placeholder");
    }

    let javascript = javascript
        .replace(BROWSER_STORAGE_SCOPE_PLACEHOLDER, scope)
        .replace(BROWSER_GLOBAL_STORAGE_SCOPE_PLACEHOLDER, global_scope)
        .replace(BROWSER_STORAGE_API_PLACEHOLDER, api)
        .replace(BROWSER_GLOBAL_STORAGE_API_PLACEHOLDER, global_api)
        .replace(BROWSER_STORAGE_TOKEN_PLACEHOLDER, token);
    if javascript.contains(BROWSER_STORAGE_SCOPE_PLACEHOLDER)
        || javascript.contains(BROWSER_GLOBAL_STORAGE_SCOPE_PLACEHOLDER)
        || javascript.contains(BROWSER_STORAGE_API_PLACEHOLDER)
        || javascript.contains(BROWSER_GLOBAL_STORAGE_API_PLACEHOLDER)
        || javascript.contains(BROWSER_STORAGE_TOKEN_PLACEHOLDER)
    {
        return Err("workbench browser storage placeholder replacement was incomplete");
    }

    Ok(javascript.into_bytes())
}

fn inject_browser_storage_scope(
    body: Vec<u8>,
    scope: &str,
    global_scope: &str,
    api: &str,
    global_api: &str,
    token: &str,
) -> Vec<u8> {
    let mut html = match String::from_utf8(body) {
        Ok(html) => html,
        Err(error) => return error.into_bytes(),
    };
    let lower = html.to_ascii_lowercase();
    let Some(index) = lower.find("</head>") else {
        return html.into_bytes();
    };
    let marker = format!(
        "<meta name=\"jexactyl-webide-scope\" content=\"{scope}\"><meta name=\"jexactyl-webide-global-scope\" content=\"{global_scope}\"><meta name=\"jexactyl-webide-storage-api\" content=\"{api}\"><meta name=\"jexactyl-webide-global-storage-api\" content=\"{global_api}\"><meta name=\"jexactyl-webide-storage-token\" content=\"{token}\">"
    );
    html.insert_str(index, &marker);
    html.into_bytes()
}

fn random_nonce() -> String {
    use base64::Engine;
    let mut value = [0u8; 18];
    rand::rng().fill_bytes(&mut value);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value)
}

fn upstream_path_and_query(
    uri: &axum::http::Uri,
    server: uuid::Uuid,
    session: uuid::Uuid,
) -> String {
    // Axum's nested routers may expose either the original URI (`/api/...`) or
    // the URI with the nest prefix stripped (`/servers/...`). Anchor on the
    // authenticated server/session suffix so neither form can leak the public
    // proxy route into code-server (which responds with `Not found.`).
    let marker = format!("/servers/{server}/web-ide/s/{session}/proxy");
    let path = uri
        .path()
        .find(&marker)
        .map(|offset| &uri.path()[offset + marker.len()..])
        .unwrap_or(uri.path());
    let path = if path.is_empty() { "/" } else { path };
    match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_path_is_scoped_and_stripped_before_unix_socket_forwarding() {
        let server = uuid::Uuid::new_v4();
        let session = uuid::Uuid::new_v4();
        let uri: axum::http::Uri =
            format!("/api/servers/{server}/web-ide/s/{session}/proxy/stable/asset.js?v=1")
                .parse()
                .unwrap();
        assert_eq!(
            upstream_path_and_query(&uri, server, session),
            "/stable/asset.js?v=1"
        );

        let root: axum::http::Uri = format!("/api/servers/{server}/web-ide/s/{session}/proxy/")
            .parse()
            .unwrap();
        assert_eq!(upstream_path_and_query(&root, server, session), "/");

        let nested: axum::http::Uri =
            format!("/servers/{server}/web-ide/s/{session}/proxy/?folder=/home/container")
                .parse()
                .unwrap();
        assert_eq!(
            upstream_path_and_query(&nested, server, session),
            "/?folder=/home/container"
        );
    }

    #[test]
    fn browser_storage_scope_is_stable_but_user_scoped() {
        let server = uuid::Uuid::new_v4();
        let user = uuid::Uuid::new_v4();
        assert_eq!(
            browser_storage_scope(server, user),
            browser_storage_scope(server, user)
        );
        assert_ne!(
            browser_storage_scope(server, user),
            browser_storage_scope(server, uuid::Uuid::new_v4())
        );
        assert_eq!(
            browser_user_storage_scope(user),
            browser_user_storage_scope(user)
        );
        assert_ne!(
            browser_user_storage_scope(user),
            browser_user_storage_scope(uuid::Uuid::new_v4())
        );
        let body = inject_browser_storage_scope(
            b"<html><head></head></html>".to_vec(),
            "scope",
            "global-scope",
            "/browser-state",
            "/browser-state/global",
            "token",
        );
        assert!(
            String::from_utf8(body)
                .unwrap()
                .contains("jexactyl-webide-storage-token")
        );
    }

    #[test]
    fn workbench_storage_configuration_is_bound_and_fail_closed() {
        assert!(is_request_header_blocked(&header::ACCEPT_ENCODING));
        assert!(is_browser_workbench_script(
            "/stable/static/out/vs/code/browser/workbench/workbench.js?v=1"
        ));
        assert!(is_browser_workbench_script(
            "/static/out/vs/workbench/workbench.web.main.internal.js"
        ));
        assert!(!is_browser_workbench_script("/static/out/main.js"));

        let body = format!(
            "const scope='{BROWSER_STORAGE_SCOPE_PLACEHOLDER}',globalScope='{BROWSER_GLOBAL_STORAGE_SCOPE_PLACEHOLDER}',api='{BROWSER_STORAGE_API_PLACEHOLDER}',globalApi='{BROWSER_GLOBAL_STORAGE_API_PLACEHOLDER}',token='{BROWSER_STORAGE_TOKEN_PLACEHOLDER}';"
        );
        let injected = inject_browser_storage_configuration(
            body.into_bytes(),
            "per-user-scope",
            "per-user-global-scope",
            "/browser-state",
            "/browser-state/global",
            "ephemeral-token",
        )
        .unwrap();
        let injected = String::from_utf8(injected).unwrap();
        assert!(injected.contains("per-user-scope"));
        assert!(injected.contains("per-user-global-scope"));
        assert!(injected.contains("/browser-state"));
        assert!(injected.contains("/browser-state/global"));
        assert!(injected.contains("ephemeral-token"));
        assert!(!injected.contains("__JEXACTYL_WEBIDE_"));
        assert!(
            inject_browser_storage_configuration(
                b"const unrelated=true;".to_vec(),
                "scope",
                "global-scope",
                "/browser-state",
                "/browser-state/global",
                "token",
            )
            .is_err()
        );
    }

    #[test]
    fn extension_credentials_are_bounded_and_not_taken_from_query_strings() {
        let session = uuid::Uuid::new_v4();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer ephemeral-token"),
        );
        assert!(matches!(
            session_credential(&headers, session),
            Some((SessionCredential::Extension(token), None)) if token == "ephemeral-token"
        ));

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", "x".repeat(129))).unwrap(),
        );
        assert!(session_credential(&headers, session).is_none());
    }

    #[test]
    fn collaboration_presence_name_is_overwritten_from_authenticated_session() {
        use std::collections::HashMap;
        use yrs::sync::Protocol;

        let mut awareness = yrs::sync::Awareness::new(yrs::Doc::new());
        let awareness_clients = Arc::new(StdMutex::new(HashMap::new()));
        let connection_uuid = uuid::Uuid::new_v4();
        let update = yrs::sync::AwarenessUpdate {
            clients: HashMap::from([(
                42,
                yrs::sync::awareness::AwarenessUpdateEntry {
                    clock: 1,
                    json: serde_json::json!({
                        "user": { "name": "spoofed-admin" },
                        "cursor": { "file": "server.properties", "anchor": 1, "head": 4 }
                    })
                    .to_string(),
                },
            )]),
        };
        AuthenticatedCollaborationProtocol {
            display_name: "panel-user".to_string(),
            connection_uuid,
            claimed_client: Arc::new(StdMutex::new(None)),
            awareness_clients: Arc::clone(&awareness_clients),
        }
        .handle_awareness_update(&mut awareness, update)
        .unwrap();

        let state: serde_json::Value =
            serde_json::from_str(awareness.clients().get(&42).unwrap()).unwrap();
        assert_eq!(state["user"]["name"], "panel-user");
        assert_eq!(state["cursor"]["head"], 4);

        let hijack = yrs::sync::AwarenessUpdate {
            clients: HashMap::from([(
                42,
                yrs::sync::awareness::AwarenessUpdateEntry {
                    clock: 2,
                    json: serde_json::json!({ "user": { "name": "another-user" } }).to_string(),
                },
            )]),
        };
        let result = AuthenticatedCollaborationProtocol {
            display_name: "other-panel-user".to_string(),
            connection_uuid: uuid::Uuid::new_v4(),
            claimed_client: Arc::new(StdMutex::new(None)),
            awareness_clients,
        }
        .handle_awareness_update(&mut awareness, hijack);
        assert!(
            result.is_err(),
            "another connection must not hijack an active awareness client ID"
        );
        let state: serde_json::Value =
            serde_json::from_str(awareness.clients().get(&42).unwrap()).unwrap();
        assert_eq!(state["user"]["name"], "panel-user");
    }
}
