use super::State;
use std::collections::{HashMap, HashSet};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use utoipa_axum::{router::OpenApiRouter, routes};

#[derive(utoipa::ToSchema, serde::Deserialize, serde::Serialize, Clone, Default)]
pub struct WebHostingDomain {
    pub hostname: String,
    #[serde(default)]
    pub tls_mode: Option<String>,
    #[serde(default)]
    pub ssl_enabled: Option<bool>,
}

#[derive(utoipa::ToSchema, serde::Deserialize, serde::Serialize, Clone, Default)]
pub struct WebHostingUpstream {
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
}

#[derive(utoipa::ToSchema, serde::Deserialize, serde::Serialize, Clone, Default)]
pub struct WebHostingDatabase {
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub database: String,
    pub username: String,
    pub password: String,
}

#[derive(utoipa::ToSchema, serde::Deserialize, serde::Serialize, Clone, Default)]
pub struct WebHostingPayload {
    #[serde(default)]
    pub server_uuid: Option<uuid::Uuid>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub document_root: Option<String>,
    #[serde(default)]
    pub primary_domain: Option<String>,
    #[serde(default)]
    pub upstream: Option<WebHostingUpstream>,
    #[serde(default)]
    pub domains: Option<Vec<WebHostingDomain>>,
    #[serde(default)]
    pub ssl_mode: Option<String>,
    #[serde(default)]
    #[schema(value_type = serde_json::Value)]
    pub waf: serde_json::Value,
    #[serde(default)]
    #[schema(value_type = serde_json::Value)]
    pub hotlink_protection: serde_json::Value,
    #[serde(default)]
    #[schema(value_type = serde_json::Value)]
    pub basic_auth: serde_json::Value,
    #[serde(default)]
    #[schema(value_type = serde_json::Value)]
    pub git_deploy: serde_json::Value,
    #[serde(default)]
    #[schema(value_type = serde_json::Value)]
    pub staging: serde_json::Value,
    #[serde(default)]
    #[schema(value_type = serde_json::Value)]
    pub backup_policy: serde_json::Value,
    #[serde(default)]
    #[schema(value_type = serde_json::Value)]
    pub load_balancing: serde_json::Value,
    #[serde(default)]
    #[schema(value_type = serde_json::Value)]
    pub redis_policy: serde_json::Value,
    #[serde(default)]
    pub operation_uuid: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub site_title: Option<String>,
    #[serde(default)]
    pub admin_username: Option<String>,
    #[serde(default)]
    pub admin_password: Option<String>,
    #[serde(default)]
    pub admin_email: Option<String>,
    #[serde(default)]
    pub database: Option<WebHostingDatabase>,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct WebHostingState {
    #[serde(default = "web_hosting_state_version")]
    state_version: u8,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    document_root: Option<String>,
    #[serde(default)]
    primary_domain: Option<String>,
    #[serde(default)]
    upstream: Option<WebHostingUpstream>,
    #[serde(default)]
    domains: Option<Vec<WebHostingDomain>>,
    #[serde(default)]
    ssl_mode: Option<String>,
}

fn web_hosting_state_version() -> u8 {
    1
}

impl WebHostingState {
    fn from_payload(payload: &WebHostingPayload) -> Self {
        Self {
            state_version: web_hosting_state_version(),
            enabled: payload.enabled,
            document_root: payload.document_root.clone(),
            primary_domain: payload.primary_domain.clone(),
            upstream: payload.upstream.clone(),
            domains: payload.domains.clone(),
            ssl_mode: payload.ssl_mode.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WebHostingDatabase, WebHostingPayload, WebHostingState};

    #[test]
    fn persisted_state_excludes_install_credentials_and_arbitrary_payload() {
        let basic_auth_password = ["unit", "basic-auth", "sentinel"].join("-");
        let payload = WebHostingPayload {
            admin_password: Some(["unit", "admin", "sentinel"].join("-")),
            database: Some(WebHostingDatabase {
                host: "db.internal".to_string(),
                port: Some(3306),
                database: "site".to_string(),
                username: "site-user".to_string(),
                password: ["unit", "database", "sentinel"].join("-"),
            }),
            basic_auth: serde_json::json!({"password": basic_auth_password}),
            ..Default::default()
        };

        let state = serde_json::to_string(&WebHostingState::from_payload(&payload)).unwrap();

        assert!(!state.contains("unit-admin-sentinel"));
        assert!(!state.contains("unit-database-sentinel"));
        assert!(!state.contains("unit-basic-auth-sentinel"));
        assert!(!state.contains("database"));
        assert!(!state.contains("basic_auth"));
    }

    #[test]
    fn credential_bearing_git_urls_are_rejected() {
        assert!(super::git_url_contains_credentials(
            "https://deploy:password@example.com/site.git"
        ));
        assert!(super::git_url_contains_credentials(
            "https://example.com/site.git?access_token=test-token"
        ));
        assert!(!super::git_url_contains_credentials(
            "git@github.com:example/site.git"
        ));
        assert!(!super::git_url_contains_credentials(
            "https://github.com/example/site.git"
        ));
    }
}

pub(crate) mod get {
    use super::*;
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::{GetState, api::servers::_server_::GetServer},
    };

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = serde_json::Value),
    ), params(
        (
            "server" = uuid::Uuid,
            description = "The server uuid",
            example = "123e4567-e89b-12d3-a456-426614174000",
        ),
    ))]
    pub async fn route(state: GetState, server: GetServer) -> ApiResponseResult {
        ApiResponse::new_serialized(status_payload(&state.config, &server).await).ok()
    }
}

pub(crate) mod put {
    use super::*;
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::{GetState, api::servers::_server_::GetServer},
    };

    #[utoipa::path(put, path = "/", responses(
        (status = OK, body = serde_json::Value),
    ), params(
        (
            "server" = uuid::Uuid,
            description = "The server uuid",
            example = "123e4567-e89b-12d3-a456-426614174000",
        ),
    ), request_body = inline(WebHostingPayload))]
    pub async fn route(
        state: GetState,
        server: GetServer,
        crate::Payload(payload): crate::Payload<WebHostingPayload>,
    ) -> ApiResponseResult {
        match sync_web_hosting(&state.config, &server, &payload).await {
            Ok(result) => ApiResponse::new_serialized(result).ok(),
            Err(err) => ApiResponse::from(err).ok(),
        }
    }
}

pub(crate) mod post {
    use super::*;
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::{GetState, api::servers::_server_::GetServer},
    };
    use axum::extract::Path;

    #[utoipa::path(post, path = "/{action}", responses(
        (status = OK, body = serde_json::Value),
    ), params(
        (
            "server" = uuid::Uuid,
            description = "The server uuid",
            example = "123e4567-e89b-12d3-a456-426614174000",
        ),
        (
            "action" = String,
            description = "The web hosting action",
        ),
    ), request_body = inline(WebHostingPayload))]
    pub async fn route(
        state: GetState,
        server: GetServer,
        Path((_server, action)): Path<(uuid::Uuid, String)>,
        data: Result<crate::Payload<WebHostingPayload>, crate::payload::PayloadRejection>,
    ) -> ApiResponseResult {
        let payload = data.map(|payload| payload.0).unwrap_or_default();

        let result = match action.as_str() {
            "sync-vhost" => sync_web_hosting(&state.config, &server, &payload).await,
            "fix-permissions" => fix_permissions(&server, &payload).await,
            "scan" => scan_files(&server, &payload).await,
            "backup" => backup_files(&state.config, &server, &payload).await,
            "repair-wordpress" => repair_wordpress(&server, &payload).await,
            "cache-purge" => purge_cache(&server, &payload).await,
            "deploy" => git_deploy(&server, &payload).await,
            "runtime-info" => runtime_info(&server).await,
            "run-command" | "app-command" => run_app_command(&server, &payload).await,
            "audit" => audit_dependencies(&server, &payload).await,
            "stage" | "push-staging" | "restore" | "quarantine" | "redis-provision" => {
                Ok(operation_result(
                    &action,
                    "accepted",
                    10,
                    serde_json::json!({
                        "message": "Action accepted by Wings-RS. Panel-side orchestration is responsible for this workflow."
                    }),
                ))
            }
            "install-app" | "app-install" => install_app(&server, &payload).await,
            _ => Ok(operation_result(
                &action,
                "failed",
                100,
                serde_json::json!({
                    "message": "Unsupported web hosting action."
                }),
            )),
        };

        match result {
            Ok(result) => ApiResponse::new_serialized(result).ok(),
            Err(err) => ApiResponse::from(err).ok(),
        }
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .routes(routes!(put::route))
        .routes(routes!(post::route))
        .with_state(state.clone())
}

async fn status_payload(
    config: &crate::config::Config,
    server: &crate::server::Server,
) -> serde_json::Value {
    let config_snapshot = config.load();
    let web_config = &config_snapshot.web_hosting;
    let state_file = state_path(config, server);
    let vhost_file = vhost_path(config, server);
    let saved_state = match tokio::fs::read_to_string(&state_file).await {
        Ok(contents) => match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(raw) => match serde_json::from_value::<WebHostingState>(raw.clone()) {
                Ok(state) => match serde_json::to_value(&state) {
                    Ok(sanitized) => {
                        if raw != sanitized
                            && let Ok(body) = serde_json::to_vec_pretty(&sanitized)
                        {
                            if let Err(error) = write_private_state(&state_file, &body).await {
                                tracing::warn!(
                                    server = %server.uuid,
                                    %error,
                                    "failed to migrate web hosting state to its redacted form"
                                );
                            }
                        }
                        Some(sanitized)
                    }
                    Err(error) => {
                        discard_unreadable_state(&state_file, server, error.to_string()).await;
                        None
                    }
                },
                Err(error) => {
                    discard_unreadable_state(&state_file, server, error.to_string()).await;
                    None
                }
            },
            Err(error) => {
                discard_unreadable_state(&state_file, server, error.to_string()).await;
                None
            }
        },
        Err(_) => None,
    };

    let container_ip = crate::server::helper::resolve_container_ip(server)
        .await
        .ok();
    let runtime = runtime_info_payload(server).await.unwrap_or_else(|err| {
        serde_json::json!({
            "available": false,
            "error": err.to_string(),
        })
    });

    serde_json::json!({
        "enabled": web_config.enabled,
        "server_uuid": server.uuid,
        "document_root": saved_state.as_ref().and_then(|value| value.get("document_root")).and_then(|value| value.as_str()).unwrap_or("/public_html"),
        "state_path": state_file,
        "vhost_path": vhost_file,
        "vhost_exists": tokio::fs::metadata(&vhost_file).await.is_ok(),
        "vhost_dir_writable": writable_directory(&web_config.vhost_dir).await,
        "reload_helper_present": std::path::Path::new(&web_config.reload_helper).exists(),
        "container_ip": container_ip,
        "upstream": saved_state.as_ref().and_then(|value| value.get("upstream")).cloned(),
        "runtime": runtime,
        "security": security_events_payload(server).await,
        "last_payload": saved_state,
    })
}

async fn sync_web_hosting(
    config: &crate::config::Config,
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    persist_payload(config, server, payload).await?;

    if !payload.enabled.unwrap_or(true) || all_hostnames(payload).is_empty() {
        remove_vhost(config, server).await?;
        let reload = reload_openresty(config).await?;

        return Ok(operation_result(
            "sync-vhost",
            "completed",
            100,
            serde_json::json!({
                "vhost_removed": true,
                "reload": reload,
            }),
        ));
    }

    let upstream = resolve_upstream(server, payload).await?;
    let rendered = render_vhost(config, server, payload, &upstream)?;
    let vhost_file = vhost_path(config, server);
    write_atomic(&vhost_file, rendered.as_bytes(), 0o640).await?;
    let reload = reload_openresty(config).await?;

    Ok(operation_result(
        "sync-vhost",
        "completed",
        100,
        serde_json::json!({
            "vhost_path": vhost_file,
            "upstream": upstream,
            "reload": reload,
        }),
    ))
}

async fn fix_permissions(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    let path = document_root_path(server, payload)?;
    if tokio::fs::metadata(&path).await.is_err() {
        tokio::fs::create_dir_all(&path).await?;
    }

    server.filesystem.chown_path(&path)?;

    Ok(operation_result(
        "fix-permissions",
        "completed",
        100,
        serde_json::json!({
            "path": path,
        }),
    ))
}

async fn scan_files(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    let path = document_root_path(server, payload)?;
    let scanner = if command_exists("clamdscan") {
        "clamdscan"
    } else {
        "clamscan"
    };

    if !command_exists(scanner) {
        return Ok(operation_result(
            "scan",
            "failed",
            100,
            serde_json::json!({
                "message": "Neither clamdscan nor clamscan is installed on this node."
            }),
        ));
    }

    let output = tokio::process::Command::new(scanner)
        .arg("--recursive")
        .arg(&path)
        .output()
        .await?;
    Ok(operation_result(
        "scan",
        if output.status.success() {
            "completed"
        } else {
            "failed"
        },
        100,
        serde_json::json!({
            "scanner": scanner,
            "exit_code": output.status.code(),
            "stdout_bytes": output.stdout.len(),
            "stderr_bytes": output.stderr.len(),
        }),
    ))
}

async fn backup_files(
    config: &crate::config::Config,
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    let config = config.load();
    let source = document_root_path(server, payload)?;
    let backup_dir = std::path::Path::new(&config.web_hosting.state_dir)
        .join("backups")
        .join(server.uuid.to_string());
    tokio::fs::create_dir_all(&backup_dir).await?;

    let archive = backup_dir.join(format!(
        "web-{}.tar.gz",
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    ));
    let output = tokio::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&source)
        .arg(".")
        .output()
        .await?;

    Ok(operation_result(
        "backup",
        if output.status.success() {
            "completed"
        } else {
            "failed"
        },
        100,
        serde_json::json!({
            "archive": archive,
            "exit_code": output.status.code(),
            "stderr_bytes": output.stderr.len(),
        }),
    ))
}

async fn repair_wordpress(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    let root = document_root_path(server, payload)?;
    tokio::fs::create_dir_all(root.join("wp-content/cache")).await?;
    tokio::fs::create_dir_all(root.join("wp-content/uploads")).await?;
    server.filesystem.chown_path(&root)?;

    Ok(operation_result(
        "repair-wordpress",
        "completed",
        100,
        serde_json::json!({
            "path": root,
        }),
    ))
}

async fn purge_cache(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    let root = document_root_path(server, payload)?;
    let mut removed = Vec::new();

    for relative in ["wp-content/cache", ".cache", "storage/framework/cache"] {
        let path = safe_join(&root, relative)?;
        if tokio::fs::metadata(&path).await.is_ok() {
            tokio::fs::remove_dir_all(&path).await.ok();
            removed.push(relative);
        }
    }

    Ok(operation_result(
        "cache-purge",
        "completed",
        100,
        serde_json::json!({
            "removed": removed,
        }),
    ))
}

async fn git_deploy(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    let root = document_root_path(server, payload)?;
    let repo_url = payload
        .git_deploy
        .get("repo_url")
        .or_else(|| payload.git_deploy.get("repository"))
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let branch = payload
        .git_deploy
        .get("branch")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("main");

    if repo_url.is_empty() {
        return Ok(operation_result(
            "deploy",
            "accepted",
            10,
            serde_json::json!({
                "message": "No repository URL is configured for this server."
            }),
        ));
    }

    if git_url_contains_credentials(repo_url) {
        return Ok(operation_result(
            "deploy",
            "failed",
            100,
            serde_json::json!({
                "message": "Repository URLs containing credentials are not permitted. Use a deploy key or credential helper.",
            }),
        ));
    }

    if !is_safe_git_ref(branch) {
        return Ok(operation_result(
            "deploy",
            "failed",
            100,
            serde_json::json!({
                "message": "The requested Git branch is invalid.",
            }),
        ));
    }

    let output = if tokio::fs::metadata(root.join(".git")).await.is_ok() {
        let remote = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["config", "--get", "remote.origin.url"])
            .output()
            .await?;
        let configured_remote = String::from_utf8_lossy(&remote.stdout);
        if remote.status.success() && git_url_contains_credentials(configured_remote.trim()) {
            return Ok(operation_result(
                "deploy",
                "failed",
                100,
                serde_json::json!({
                    "message": "The existing Git remote contains embedded credentials. Remove and rotate them before deploying again.",
                }),
            ));
        }

        let checkout = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .arg("checkout")
            .arg(branch)
            .output()
            .await?;

        if !checkout.status.success() {
            return Ok(operation_result(
                "deploy",
                "failed",
                100,
                serde_json::json!({
                    "exit_code": checkout.status.code(),
                    "stdout_bytes": checkout.stdout.len(),
                    "stderr_bytes": checkout.stderr.len(),
                }),
            ));
        }

        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .arg("pull")
            .arg("--ff-only")
            .output()
            .await?
    } else {
        tokio::fs::create_dir_all(&root).await?;
        tokio::process::Command::new("git")
            .arg("clone")
            .arg("--depth=1")
            .arg("--branch")
            .arg(branch)
            .arg("--")
            .arg(repo_url)
            .arg(&root)
            .output()
            .await?
    };

    server.filesystem.chown_path(&root)?;

    Ok(operation_result(
        "deploy",
        if output.status.success() {
            "completed"
        } else {
            "failed"
        },
        100,
        serde_json::json!({
            "exit_code": output.status.code(),
            "stdout_bytes": output.stdout.len(),
            "stderr_bytes": output.stderr.len(),
        }),
    ))
}

async fn runtime_info(server: &crate::server::Server) -> Result<serde_json::Value, anyhow::Error> {
    let info = runtime_info_payload(server).await?;

    Ok(operation_result(
        "runtime-info",
        if info
            .get("available")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            "completed"
        } else {
            "failed"
        },
        100,
        info,
    ))
}

async fn audit_dependencies(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    let root = document_root_path(server, payload)?;
    let working_dir = container_path_for_host_path(server, &root)?;
    let mut tools = Vec::new();

    tools.push(audit_npm_dependencies(server, &root, &working_dir).await);
    tools.push(audit_composer_dependencies(server, &root, &working_dir).await);

    let vulnerabilities = tools
        .iter()
        .map(|tool| {
            tool.get("vulnerabilities")
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        })
        .sum::<u64>();
    let outdated = tools
        .iter()
        .map(|tool| {
            tool.get("outdated")
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        })
        .sum::<u64>();
    let checked = tools
        .iter()
        .filter(|tool| {
            !tool
                .get("skipped")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
        })
        .count();

    Ok(operation_result(
        "audit",
        "completed",
        100,
        serde_json::json!({
            "checked_at": chrono::Utc::now().to_rfc3339(),
            "summary": {
                "tools_checked": checked,
                "vulnerabilities": vulnerabilities,
                "outdated": outdated,
            },
            "tools": tools,
        }),
    ))
}

async fn audit_npm_dependencies(
    server: &crate::server::Server,
    root: &std::path::Path,
    working_dir: &str,
) -> serde_json::Value {
    if tokio::fs::metadata(root.join("package.json"))
        .await
        .is_err()
    {
        return serde_json::json!({
            "name": "npm",
            "skipped": true,
            "message": "package.json was not found.",
        });
    }

    let audit = match crate::server::helper::exec_web_hosting_user_command(
        server,
        "npm audit --json --omit=dev",
        Some(working_dir),
        Some(180),
    )
    .await
    {
        Ok(output) => output,
        Err(err) => {
            return serde_json::json!({
                "name": "npm",
                "available": false,
                "error": err.to_string(),
            });
        }
    };
    let parsed = serde_json::from_str::<serde_json::Value>(&audit.stdout).ok();
    let severities = parsed
        .as_ref()
        .and_then(|value| value.get("metadata"))
        .and_then(|value| value.get("vulnerabilities"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let vulnerabilities = severities
        .get("total")
        .and_then(|value| value.as_u64())
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|value| value.get("vulnerabilities"))
                .and_then(|value| value.as_object())
                .map(|items| items.len() as u64)
        })
        .unwrap_or(0);
    let outdated = dependency_outdated_count(server, "npm outdated --json", working_dir, 90).await;

    serde_json::json!({
        "name": "npm",
        "available": true,
        "manifest": "package.json",
        "exit_code": audit.exit_code,
        "timed_out": audit.timed_out,
        "truncated": audit.truncated,
        "vulnerabilities": vulnerabilities,
        "severities": severities,
        "outdated": outdated,
        "stderr_bytes": audit.stderr.len(),
    })
}

async fn audit_composer_dependencies(
    server: &crate::server::Server,
    root: &std::path::Path,
    working_dir: &str,
) -> serde_json::Value {
    if tokio::fs::metadata(root.join("composer.lock"))
        .await
        .is_err()
        && tokio::fs::metadata(root.join("composer.json"))
            .await
            .is_err()
    {
        return serde_json::json!({
            "name": "composer",
            "skipped": true,
            "message": "composer.json was not found.",
        });
    }

    let audit = match crate::server::helper::exec_web_hosting_user_command(
        server,
        "composer audit --format=json --no-interaction",
        Some(working_dir),
        Some(180),
    )
    .await
    {
        Ok(output) => output,
        Err(err) => {
            return serde_json::json!({
                "name": "composer",
                "available": false,
                "error": err.to_string(),
            });
        }
    };
    let parsed = serde_json::from_str::<serde_json::Value>(&audit.stdout).ok();
    let vulnerabilities = parsed
        .as_ref()
        .and_then(|value| value.get("advisories"))
        .and_then(|value| value.as_object())
        .map(|packages| {
            packages
                .values()
                .map(|value| value.as_array().map(|items| items.len()).unwrap_or(0))
                .sum::<usize>() as u64
        })
        .unwrap_or(0);
    let outdated = dependency_outdated_count(
        server,
        "composer outdated --format=json --direct --no-interaction",
        working_dir,
        90,
    )
    .await;

    serde_json::json!({
        "name": "composer",
        "available": true,
        "manifest": "composer.json",
        "exit_code": audit.exit_code,
        "timed_out": audit.timed_out,
        "truncated": audit.truncated,
        "vulnerabilities": vulnerabilities,
        "outdated": outdated,
        "stderr_bytes": audit.stderr.len(),
    })
}

async fn dependency_outdated_count(
    server: &crate::server::Server,
    command: &str,
    working_dir: &str,
    timeout_seconds: u64,
) -> u64 {
    let output = match crate::server::helper::exec_web_hosting_user_command(
        server,
        command,
        Some(working_dir),
        Some(timeout_seconds),
    )
    .await
    {
        Ok(output) => output,
        Err(_) => return 0,
    };

    let parsed = serde_json::from_str::<serde_json::Value>(&output.stdout).ok();
    if let Some(object) = parsed.as_ref().and_then(|value| value.as_object()) {
        if let Some(installed) = object.get("installed").and_then(|value| value.as_array()) {
            return installed.len() as u64;
        }

        return object.len() as u64;
    }

    0
}

#[derive(Default)]
struct SecurityIncidentAccumulator {
    ip: String,
    first_seen: Option<String>,
    last_seen: Option<String>,
    requests: u64,
    blocked: u64,
    suspicious: u64,
    status_codes: HashMap<String, u64>,
    reasons: Vec<String>,
    paths: Vec<String>,
}

struct AccessLogEntry {
    ip: String,
    timestamp: String,
    method: String,
    status: u16,
    path: String,
    suspicious: bool,
    reason: Option<String>,
}

async fn security_events_payload(server: &crate::server::Server) -> serde_json::Value {
    let path = std::path::Path::new("/var/log/jexactyl-webhosting")
        .join(format!("{}-access.log", server.uuid));
    let contents = match read_tail(&path, 256 * 1024).await {
        Ok(contents) => contents,
        Err(_) => {
            return serde_json::json!({
                "log_available": false,
                "message": "No edge access log is available for this server yet.",
                "incidents": [],
                "summary": {
                    "presumed_attacks": 0,
                    "blocked_requests": 0,
                    "suspicious_requests": 0,
                    "unique_ips": 0,
                },
                "last_checked_at": chrono::Utc::now().to_rfc3339(),
            });
        }
    };

    let mut incidents: HashMap<String, SecurityIncidentAccumulator> = HashMap::new();
    let mut unique_source_ips: HashSet<String> = HashSet::new();
    let mut events = Vec::new();
    let mut blocked_requests = 0;
    let mut suspicious_requests = 0;

    for line in contents.lines() {
        let Some(entry) = parse_access_log_line(line) else {
            continue;
        };

        let blocked = matches!(entry.status, 401 | 403 | 429)
            || (entry.suspicious && matches!(entry.status, 400 | 404));
        if !entry.suspicious && !blocked {
            continue;
        }

        if blocked {
            blocked_requests += 1;
        }

        if entry.suspicious {
            suspicious_requests += 1;
        }

        unique_source_ips.insert(entry.ip.clone());

        if events.len() >= 50 {
            events.remove(0);
        }
        events.push(serde_json::json!({
            "ip": entry.ip.clone(),
            "timestamp": entry.timestamp.clone(),
            "method": entry.method.clone(),
            "path": entry.path.clone(),
            "status": entry.status,
            "blocked": blocked,
            "suspicious": entry.suspicious,
            "reason": entry.reason.clone(),
        }));

        let incident =
            incidents
                .entry(entry.ip.clone())
                .or_insert_with(|| SecurityIncidentAccumulator {
                    ip: entry.ip.clone(),
                    ..Default::default()
                });

        incident.requests += 1;
        incident.last_seen = Some(entry.timestamp.clone());
        if incident.first_seen.is_none() {
            incident.first_seen = Some(entry.timestamp.clone());
        }
        if blocked {
            incident.blocked += 1;
        }
        if entry.suspicious {
            incident.suspicious += 1;
        }
        *incident
            .status_codes
            .entry(entry.status.to_string())
            .or_insert(0) += 1;
        if let Some(reason) = &entry.reason
            && !incident.reasons.iter().any(|value| value == reason)
            && incident.reasons.len() < 6
        {
            incident.reasons.push(reason.clone());
        }
        if !incident.paths.iter().any(|path| path == &entry.path) && incident.paths.len() < 6 {
            incident.paths.push(entry.path);
        }
    }

    let mut incident_values = incidents
        .into_values()
        .filter(|incident| {
            incident.requests >= 3 || incident.blocked >= 2 || incident.suspicious >= 2
        })
        .collect::<Vec<_>>();
    incident_values.sort_by(|a, b| b.requests.cmp(&a.requests));
    incident_values.truncate(8);
    let presumed_attacks = incident_values.len();
    let unique_ips = unique_source_ips.len();
    let incidents = incident_values
        .into_iter()
        .map(|incident| {
            serde_json::json!({
                "ip": incident.ip,
                "first_seen": incident.first_seen,
                "last_seen": incident.last_seen,
                "requests": incident.requests,
                "blocked_requests": incident.blocked,
                "suspicious_requests": incident.suspicious,
                "status_codes": incident.status_codes,
                "reasons": incident.reasons,
                "paths": incident.paths,
            })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "log_available": true,
        "incidents": incidents,
        "events": events,
        "summary": {
            "presumed_attacks": presumed_attacks,
            "blocked_requests": blocked_requests,
            "suspicious_requests": suspicious_requests,
            "unique_ips": unique_ips,
        },
        "last_checked_at": chrono::Utc::now().to_rfc3339(),
    })
}

async fn read_tail(path: &std::path::Path, max_bytes: u64) -> Result<String, anyhow::Error> {
    let mut file = tokio::fs::File::open(path).await?;
    let metadata = file.metadata().await?;
    let start = metadata.len().saturating_sub(max_bytes);

    file.seek(std::io::SeekFrom::Start(start)).await?;

    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).await?;

    if start > 0
        && let Some(position) = buffer.iter().position(|byte| *byte == b'\n')
    {
        buffer.drain(..=position);
    }

    Ok(String::from_utf8_lossy(&buffer).to_string())
}

fn parse_access_log_line(line: &str) -> Option<AccessLogEntry> {
    let ip = line.split_whitespace().next()?.to_string();
    let timestamp_start = line.find('[')? + 1;
    let timestamp_end = line[timestamp_start..].find(']')? + timestamp_start;
    let timestamp_raw = &line[timestamp_start..timestamp_end];
    let timestamp = chrono::DateTime::parse_from_str(timestamp_raw, "%d/%b/%Y:%H:%M:%S %z")
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|_| timestamp_raw.to_string());
    let after_timestamp = &line[timestamp_end + 1..];
    let request_start = after_timestamp.find('"')? + 1;
    let request_tail = &after_timestamp[request_start..];
    let request_end = request_tail.find('"')?;
    let request = &request_tail[..request_end];
    let status = request_tail[request_end + 1..]
        .split_whitespace()
        .next()?
        .parse::<u16>()
        .ok()?;
    let mut request_parts = request.split_whitespace();
    let method = request_parts.next().unwrap_or("GET").to_string();
    let path = request_parts
        .next()
        .unwrap_or("-")
        .split(['?', '#'])
        .next()
        .unwrap_or("-")
        .to_string();
    let reason = suspicious_reason(&path).map(str::to_string);
    let suspicious = reason.is_some();

    Some(AccessLogEntry {
        ip,
        timestamp,
        method,
        status,
        path,
        suspicious,
        reason,
    })
}

fn suspicious_reason(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();

    for (needle, reason) in [
        ("/.env", "Environment file probe"),
        ("/.git", "Git metadata probe"),
        ("/wp-login.php", "WordPress login probing"),
        ("/xmlrpc.php", "WordPress XML-RPC probing"),
        ("/phpmyadmin", "Database admin probe"),
        ("/pma", "Database admin probe"),
        ("/adminer", "Database admin probe"),
        ("/vendor/", "Composer vendor exposure probe"),
        ("/cgi-bin/", "Legacy CGI probe"),
        ("../", "Path traversal attempt"),
        ("%2e%2e", "Encoded path traversal attempt"),
        ("union%20select", "SQL injection attempt"),
        ("select%20", "SQL injection attempt"),
        ("<script", "XSS attempt"),
    ] {
        if lower.contains(needle) {
            return Some(reason);
        }
    }

    None
}

async fn runtime_info_payload(
    server: &crate::server::Server,
) -> Result<serde_json::Value, anyhow::Error> {
    let php = crate::server::helper::exec_container_shell_command(
        server,
        r#"php -r 'echo json_encode(["version"=>PHP_VERSION,"sapi"=>PHP_SAPI,"extensions"=>get_loaded_extensions(),"memory_limit"=>ini_get("memory_limit"),"upload_max_filesize"=>ini_get("upload_max_filesize"),"post_max_size"=>ini_get("post_max_size"),"max_execution_time"=>ini_get("max_execution_time")]);'"#,
        "/home/container",
        std::time::Duration::from_secs(15),
        crate::server::helper::WEB_HOSTING_MAX_COMMAND_OUTPUT_BYTES,
    )
    .await?;

    if php.exit_code != Some(0) {
        return Ok(serde_json::json!({
            "available": false,
            "error": "PHP runtime probe failed",
            "exit_code": php.exit_code,
        }));
    }

    let php_json = serde_json::from_str::<serde_json::Value>(&php.stdout)
        .unwrap_or_else(|_| serde_json::json!({}));
    let extensions = php_json
        .get("extensions")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(|value| value.to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut extension_status = serde_json::Map::new();
    for extension in required_php_extensions() {
        extension_status.insert(
            extension.to_string(),
            serde_json::Value::Bool(php_extension_loaded(&extensions, extension)),
        );
    }

    Ok(serde_json::json!({
        "available": true,
        "php": {
            "version": php_json.get("version").and_then(|value| value.as_str()).unwrap_or_default(),
            "sapi": php_json.get("sapi").and_then(|value| value.as_str()).unwrap_or_default(),
            "memory_limit": php_json.get("memory_limit").and_then(|value| value.as_str()).unwrap_or_default(),
            "upload_max_filesize": php_json.get("upload_max_filesize").and_then(|value| value.as_str()).unwrap_or_default(),
            "post_max_size": php_json.get("post_max_size").and_then(|value| value.as_str()).unwrap_or_default(),
            "max_execution_time": php_json.get("max_execution_time").and_then(|value| value.as_str()).unwrap_or_default(),
        },
        "extensions": extensions,
        "required_extensions": extension_status,
        "tools": {
            "composer": tool_version(server, "composer --version --no-ansi", 10).await,
            "wp_cli": tool_version(server, "wp --info", 10).await,
            "node": tool_version(server, "node --version", 10).await,
            "npm": tool_version(server, "npm --version", 10).await,
        }
    }))
}

async fn tool_version(
    server: &crate::server::Server,
    command: &str,
    timeout_seconds: u64,
) -> serde_json::Value {
    match crate::server::helper::exec_container_shell_command(
        server,
        command,
        "/home/container",
        std::time::Duration::from_secs(timeout_seconds),
        8192,
    )
    .await
    {
        Ok(output) => serde_json::json!({
            "available": output.exit_code == Some(0),
            "version": if output.stdout.is_empty() { output.stderr } else { output.stdout },
            "exit_code": output.exit_code,
        }),
        Err(err) => serde_json::json!({
            "available": false,
            "error": err.to_string(),
        }),
    }
}

async fn run_app_command(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    let preset = payload
        .preset
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let command = match preset.and_then(command_preset) {
        Some(command) => command.to_string(),
        None => payload
            .command
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("command cannot be empty"))?
            .to_string(),
    };

    let working_dir = match crate::server::helper::web_hosting_working_directory(
        payload.working_directory.as_deref(),
    ) {
        Ok(working_dir) => working_dir,
        Err(err) => {
            return Ok(operation_result(
                "run-command",
                "failed",
                100,
                serde_json::json!({
                    "preset": preset,
                    "message": err.to_string(),
                }),
            ));
        }
    };
    let timeout_seconds = payload
        .timeout_seconds
        .unwrap_or(crate::server::helper::WEB_HOSTING_DEFAULT_COMMAND_TIMEOUT_SECONDS)
        .clamp(
            1,
            crate::server::helper::WEB_HOSTING_MAX_COMMAND_TIMEOUT_SECONDS,
        );
    let output = match crate::server::helper::exec_web_hosting_user_command(
        server,
        &command,
        Some(&working_dir),
        Some(timeout_seconds),
    )
    .await
    {
        Ok(output) => output,
        Err(err) => {
            return Ok(operation_result(
                "run-command",
                "failed",
                100,
                serde_json::json!({
                    "preset": preset,
                    "working_directory": working_dir,
                    "timeout_seconds": timeout_seconds,
                    "message": err.to_string(),
                }),
            ));
        }
    };
    let success = output.exit_code == Some(0) && !output.timed_out;

    Ok(operation_result(
        "run-command",
        if success { "completed" } else { "failed" },
        100,
        serde_json::json!({
            "preset": preset,
            "working_directory": working_dir,
            "timeout_seconds": timeout_seconds,
            "exit_code": output.exit_code,
            "stdout_bytes": output.stdout.len(),
            "stderr_bytes": output.stderr.len(),
            "timed_out": output.timed_out,
            "truncated": output.truncated,
        }),
    ))
}

async fn install_app(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    let template = payload
        .template
        .as_deref()
        .or(payload.app.as_deref())
        .unwrap_or("static");
    let root = document_root_path(server, payload)?;
    tokio::fs::create_dir_all(&root).await?;

    if directory_has_entries(&root).await? && !payload.overwrite {
        return Ok(operation_result(
            "install-app",
            "failed",
            100,
            serde_json::json!({
                "message": "Document root is not empty. Enable overwrite to replace existing starter files.",
                "path": root,
            }),
        ));
    }

    if payload.overwrite {
        clear_directory(&root).await?;
    }

    let app_result = match template {
        "wordpress" => install_wordpress(server, payload, &root).await?,
        "laravel" => install_laravel(server, payload, &root).await?,
        "react" | "npm" | "node" | "nodejs" => {
            install_react_starter(server, payload, &root).await?
        }
        "adminer" => install_adminer(payload, &root).await?,
        "php" => install_php_starter(payload, &root).await?,
        "static" => install_static_starter(payload, &root).await?,
        _ => {
            return Ok(operation_result(
                "install-app",
                "failed",
                100,
                serde_json::json!({
                    "message": "Unknown app template.",
                    "template": template,
                }),
            ));
        }
    };

    server.filesystem.chown_path(&root)?;

    Ok(operation_result(
        "install-app",
        "completed",
        100,
        serde_json::json!({
            "template": template,
            "path": root,
            "app": app_result,
        }),
    ))
}

async fn install_wordpress(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
    root: &std::path::Path,
) -> Result<serde_json::Value, anyhow::Error> {
    let database = payload
        .database
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("WordPress database credentials were not provided"))?;
    let admin_username =
        required_payload_value(payload.admin_username.as_deref(), "admin username")?;
    let admin_password =
        required_payload_value(payload.admin_password.as_deref(), "admin password")?;
    let admin_email = payload
        .admin_email
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_admin_email(payload));
    let site_url = site_url(payload);

    let bytes = reqwest::get("https://wordpress.org/latest.tar.gz")
        .await?
        .bytes()
        .await?;
    let host_root = root.to_path_buf();
    let extract_root = host_root.clone();

    tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
        let cursor = std::io::Cursor::new(bytes);
        let gz = flate2::read::GzDecoder::new(cursor);
        let mut archive = tar::Archive::new(gz);

        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_path_buf();
            let relative = strip_wordpress_prefix(&path)?;
            if relative.as_os_str().is_empty() {
                continue;
            }

            entry.unpack(extract_root.join(relative))?;
        }

        Ok(())
    })
    .await??;

    let working_dir = container_path_for_host_path(server, &host_root)?;
    server.filesystem.chown_path(&host_root)?;
    let db_host = match database.port {
        Some(port) if port != 3306 => format!("{}:{}", database.host, port),
        _ => database.host.clone(),
    };
    let setup_env = vec![
        format!("JEXACTYL_WP_DB_PASSWORD={}", database.password),
        format!("JEXACTYL_WP_ADMIN_PASSWORD={}", admin_password),
    ];
    run_checked_setup_command_with_env(
        server,
        format!(
            r#"wp --allow-root config create --dbname={} --dbuser={} --dbpass="$JEXACTYL_WP_DB_PASSWORD" --dbhost={} --skip-check --force && chmod 600 wp-config.php"#,
            shell_quote(&database.database),
            shell_quote(&database.username),
            shell_quote(&db_host),
        ),
        &working_dir,
        180,
        &setup_env,
    )
    .await?;
    run_checked_setup_command_with_env(
        server,
        format!(
            r#"wp --allow-root core install --url={} --title={} --admin_user={} --admin_password="$JEXACTYL_WP_ADMIN_PASSWORD" --admin_email={} --skip-email"#,
            shell_quote(&site_url),
            shell_quote(payload.site_title.as_deref().unwrap_or("WordPress Site")),
            shell_quote(&admin_username),
            shell_quote(&admin_email),
        ),
        &working_dir,
        180,
        &setup_env,
    )
    .await?;

    Ok(serde_json::json!({
        "name": "WordPress",
        "site_url": site_url,
        "admin_username": admin_username,
        "login_path": "/wp-admin/",
        "database": database.database.clone(),
    }))
}

async fn install_react_starter(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
    root: &std::path::Path,
) -> Result<serde_json::Value, anyhow::Error> {
    let title = payload.site_title.as_deref().unwrap_or("Jexactyl Web App");
    let app_dir = server.filesystem.base_path.join(".jexactyl/apps/react");
    let source_dir = app_dir.join("src");
    if payload.overwrite && tokio::fs::metadata(&app_dir).await.is_ok() {
        tokio::fs::remove_dir_all(&app_dir).await?;
    }

    tokio::fs::create_dir_all(&source_dir).await?;
    tokio::fs::write(
        app_dir.join("package.json"),
        format!(
            r#"{{
  "name": "jexactyl-react-site",
  "private": true,
  "type": "module",
  "scripts": {{
    "build": "vite build"
  }},
  "dependencies": {{
    "@vitejs/plugin-react": "latest",
    "vite": "latest",
    "typescript": "latest",
    "react": "latest",
    "react-dom": "latest"
  }},
  "devDependencies": {{}}
}}
"#
        ),
    )
    .await?;
    tokio::fs::write(
        app_dir.join("index.html"),
        format!(
            r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>{}</title>
  </head>
  <body>
    <div id="root"></div>
    <script type="module" src="/src/main.jsx"></script>
  </body>
</html>
"#,
            html_escape(title)
        ),
    )
    .await?;
    tokio::fs::write(
        source_dir.join("main.jsx"),
        format!(
            r#"import React from 'react';
import {{ createRoot }} from 'react-dom/client';
import './style.css';

createRoot(document.getElementById('root')).render(
  <main className="app">
    <h1>{}</h1>
    <p>Your NPM app is ready to build and deploy from Jexactyl.</p>
  </main>
);
"#,
            js_escape(title)
        ),
    )
    .await?;
    tokio::fs::write(source_dir.join("style.css"), "body{margin:0;font-family:Inter,system-ui,sans-serif;background:#101318;color:#f7fafc}.app{min-height:100vh;display:grid;place-content:center;text-align:center;padding:2rem}.app h1{font-size:clamp(2rem,6vw,4rem);margin:0 0 1rem}.app p{color:#aab4c0}\n").await?;

    let app_working_dir = container_path_for_host_path(server, &app_dir)?;
    let root_working_dir = container_path_for_host_path(server, root)?;
    server.filesystem.chown_path(&app_dir)?;
    server.filesystem.chown_path(root)?;
    run_checked_setup_command(
        server,
        "npm install --no-audit --no-fund".to_string(),
        &app_working_dir,
        600,
    )
    .await?;
    run_checked_setup_command(
        server,
        format!(
            "npm run build -- --outDir {} --emptyOutDir",
            shell_quote(&root_working_dir)
        ),
        &app_working_dir,
        600,
    )
    .await?;

    Ok(serde_json::json!({
        "name": "React / Vite",
        "site_url": site_url(payload),
        "source_path": "/home/container/.jexactyl/apps/react",
        "build_output": root_working_dir,
    }))
}

async fn install_laravel(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
    root: &std::path::Path,
) -> Result<serde_json::Value, anyhow::Error> {
    let title = payload.site_title.as_deref().unwrap_or("Laravel App");
    let app_dir = server.filesystem.base_path.join(".jexactyl/apps/laravel");
    let app_container_dir = "/home/container/.jexactyl/apps/laravel";
    if payload.overwrite && tokio::fs::metadata(&app_dir).await.is_ok() {
        tokio::fs::remove_dir_all(&app_dir).await?;
    }
    let apps_parent = app_dir.parent().unwrap_or(&server.filesystem.base_path);
    tokio::fs::create_dir_all(apps_parent).await?;
    server.filesystem.chown_path(apps_parent)?;

    if tokio::fs::metadata(&app_dir).await.is_err() {
        run_checked_setup_command(
            server,
            "composer create-project laravel/laravel .jexactyl/apps/laravel --no-interaction --prefer-dist".to_string(),
            "/home/container",
            600,
        )
        .await?;
    }

    let database_dir = app_dir.join("database");
    tokio::fs::create_dir_all(&database_dir).await?;
    tokio::fs::write(database_dir.join("database.sqlite"), "").await?;
    tokio::fs::write(
        app_dir.join(".env"),
        format!(
            r#"APP_NAME={}
APP_ENV=production
APP_KEY=
APP_DEBUG=false
APP_URL={}
LOG_CHANNEL=stack
LOG_LEVEL=warning
DB_CONNECTION=sqlite
DB_DATABASE=/home/container/.jexactyl/apps/laravel/database/database.sqlite
CACHE_STORE=file
SESSION_DRIVER=file
QUEUE_CONNECTION=database
FILESYSTEM_DISK=local
"#,
            shell_env_value(title),
            site_url(payload),
        ),
    )
    .await?;
    server.filesystem.chown_path(&app_dir)?;
    run_checked_setup_command(
        server,
        "php artisan key:generate --force".to_string(),
        app_container_dir,
        180,
    )
    .await?;
    run_checked_setup_command(
        server,
        "php artisan migrate --force".to_string(),
        app_container_dir,
        300,
    )
    .await?;
    tokio::fs::write(
        root.join("index.php"),
        r#"<?php
use Illuminate\Foundation\Application;
use Illuminate\Http\Request;

define('LARAVEL_START', microtime(true));

require '/home/container/.jexactyl/apps/laravel/vendor/autoload.php';

/** @var Application $app */
$app = require_once '/home/container/.jexactyl/apps/laravel/bootstrap/app.php';

$app->handleRequest(Request::capture());
"#,
    )
    .await?;

    Ok(serde_json::json!({
        "name": "Laravel",
        "site_url": site_url(payload),
        "app_path": app_container_dir,
        "database": "SQLite",
    }))
}

async fn install_adminer(
    payload: &WebHostingPayload,
    root: &std::path::Path,
) -> Result<serde_json::Value, anyhow::Error> {
    let bytes = reqwest::get("https://www.adminer.org/latest.php")
        .await?
        .bytes()
        .await?;
    tokio::fs::write(root.join("adminer.php"), bytes).await?;
    tokio::fs::write(
        root.join("index.php"),
        "<?php header('Location: /adminer.php', true, 302); exit;\n",
    )
    .await?;

    Ok(serde_json::json!({
        "name": "Adminer",
        "site_url": format!("{}/adminer.php", site_url(payload).trim_end_matches('/')),
        "entrypoint": "/adminer.php",
    }))
}

async fn install_php_starter(
    payload: &WebHostingPayload,
    root: &std::path::Path,
) -> Result<serde_json::Value, anyhow::Error> {
    let title = payload.site_title.as_deref().unwrap_or("Jexactyl PHP Site");
    tokio::fs::write(root.join("index.php"), format!(r#"<?php
$title = '{}';
?><!doctype html>
<html lang="en">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title><?= htmlspecialchars($title) ?></title></head>
<body style="margin:0;font-family:Inter,system-ui,sans-serif;background:#101318;color:#f7fafc;min-height:100vh;display:grid;place-content:center;text-align:center">
<main><h1><?= htmlspecialchars($title) ?></h1><p>Your PHP site is live on Jexactyl web hosting.</p></main>
</body>
</html>
"#, php_escape(title))).await?;

    Ok(serde_json::json!({
        "name": "PHP Starter",
        "site_url": site_url(payload),
    }))
}

async fn install_static_starter(
    payload: &WebHostingPayload,
    root: &std::path::Path,
) -> Result<serde_json::Value, anyhow::Error> {
    let title = payload
        .site_title
        .as_deref()
        .unwrap_or("Jexactyl Static Site");
    tokio::fs::write(root.join("index.html"), format!(r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{}</title></head>
<body style="margin:0;font-family:Inter,system-ui,sans-serif;background:#101318;color:#f7fafc;min-height:100vh;display:grid;place-content:center;text-align:center">
<main><h1>{}</h1><p>Your static site is live on Jexactyl web hosting.</p></main>
</body>
</html>
"#, html_escape(title), html_escape(title))).await?;

    Ok(serde_json::json!({
        "name": "Static Site",
        "site_url": site_url(payload),
    }))
}

async fn resolve_upstream(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<serde_json::Value, anyhow::Error> {
    let payload_upstream = payload.upstream.clone().unwrap_or_default();
    let ip = match crate::server::helper::resolve_container_ip(server).await {
        Ok(ip) => ip,
        Err(_) => payload_upstream
            .ip
            .unwrap_or_else(|| "127.0.0.1".to_string()),
    };
    let port = payload_upstream.port.unwrap_or(80);

    if ip.contains(';') || ip.contains(' ') || ip.is_empty() || port == 0 {
        return Err(anyhow::anyhow!("invalid web hosting upstream"));
    }

    Ok(serde_json::json!({ "ip": ip, "port": port }))
}

fn render_vhost(
    config: &crate::config::Config,
    server: &crate::server::Server,
    payload: &WebHostingPayload,
    upstream: &serde_json::Value,
) -> Result<String, anyhow::Error> {
    let config = config.load();
    let hostnames = all_hostnames(payload);
    if hostnames.is_empty() {
        return Err(anyhow::anyhow!("at least one hostname is required"));
    }

    let upstream_ip = upstream
        .get("ip")
        .and_then(|value| value.as_str())
        .unwrap_or("127.0.0.1");
    let upstream_port = upstream
        .get("port")
        .and_then(|value| value.as_u64())
        .unwrap_or(80);
    let web_config = &config.web_hosting;
    let document_root = document_root_path(server, payload)?;
    let waf_profile = payload
        .waf
        .get("profile")
        .and_then(|value| value.as_str())
        .unwrap_or("balanced");
    let server_names = hostnames.join(" ");
    let listen_address = if web_config.standby_bind_address.trim().is_empty() {
        web_config.bind_address.as_str()
    } else {
        web_config.standby_bind_address.as_str()
    };
    let listen_port = if web_config.standby_http_port == 0 {
        web_config.http_port
    } else {
        web_config.standby_http_port
    };

    Ok(format!(
        r#"# Managed by Wings-RS web hosting. Do not edit manually.
server {{
    listen {listen_address}:{listen_port};
    server_name {server_names};

    set_real_ip_from 127.0.0.1;
    set_real_ip_from ::1;
    real_ip_header X-Forwarded-For;
    real_ip_recursive on;

    access_log /var/log/jexactyl-webhosting/{uuid}-access.log combined;
    error_log /var/log/jexactyl-webhosting/{uuid}-error.log warn;

    set $jexactyl_server_uuid "{uuid}";
    set $jexactyl_document_root "{document_root}";
    set $jexactyl_waf_profile "{waf_profile}";

    location ^~ /.well-known/acme-challenge/ {{
        root {acme_root};
        default_type text/plain;
        try_files $uri =404;
    }}

    include /etc/openresty/jexactyl/snippets/proxy-security.conf;

    location / {{
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Jexactyl-Server {uuid};
        proxy_set_header X-Jexactyl-Document-Root {document_root};
        proxy_pass http://{upstream_ip}:{upstream_port};
    }}
}}
"#,
        listen_address = listen_address,
        listen_port = listen_port,
        server_names = server_names,
        uuid = server.uuid,
        document_root = document_root.display(),
        waf_profile = waf_profile,
        acme_root = web_config.acme_root,
        upstream_ip = upstream_ip,
        upstream_port = upstream_port,
    ))
}

async fn persist_payload(
    config: &crate::config::Config,
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<(), anyhow::Error> {
    let path = state_path(config, server);
    let state = WebHostingState::from_payload(payload);
    let body = serde_json::to_vec_pretty(&state)?;
    write_private_state(&path, &body).await
}

async fn remove_vhost(
    config: &crate::config::Config,
    server: &crate::server::Server,
) -> Result<(), anyhow::Error> {
    let path = vhost_path(config, server);
    if tokio::fs::metadata(&path).await.is_ok() {
        tokio::fs::remove_file(path).await?;
    }

    Ok(())
}

async fn reload_openresty(
    config: &crate::config::Config,
) -> Result<serde_json::Value, anyhow::Error> {
    let config = config.load();
    let helper = &config.web_hosting.reload_helper;
    if !std::path::Path::new(helper).exists() {
        return Ok(
            serde_json::json!({ "skipped": true, "reason": "reload helper is not installed" }),
        );
    }

    let output = match tokio::process::Command::new(helper).output().await {
        Ok(output) if output.status.success() => output,
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            tokio::process::Command::new("sudo")
                .arg("-n")
                .arg(helper)
                .output()
                .await?
        }
        Err(err) => return Err(err.into()),
    };

    Ok(serde_json::json!({
        "success": output.status.success(),
        "exit_code": output.status.code(),
        "stdout_bytes": output.stdout.len(),
        "stderr_bytes": output.stderr.len(),
    }))
}

fn operation_result(
    action: &str,
    status: &str,
    progress: u8,
    result: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "action": action,
        "status": status,
        "progress": progress,
        "result": result,
        "completed_at": chrono::Utc::now().to_rfc3339(),
    })
}

fn required_php_extensions() -> &'static [&'static str] {
    &[
        "bcmath",
        "ctype",
        "curl",
        "dom",
        "exif",
        "fileinfo",
        "gd",
        "intl",
        "json",
        "mbstring",
        "mysqli",
        "opcache",
        "openssl",
        "pdo",
        "pdo_mysql",
        "session",
        "simplexml",
        "tokenizer",
        "xml",
        "zip",
    ]
}

fn php_extension_loaded(extensions: &[String], extension: &str) -> bool {
    extensions.iter().any(|loaded| match extension {
        "opcache" => loaded == "opcache" || loaded == "zend opcache",
        _ => loaded == extension,
    })
}

fn command_preset(preset: &str) -> Option<&'static str> {
    match preset {
        "php-version" => Some("php -v"),
        "php-modules" => Some("php -m"),
        "composer-install" => {
            Some("composer install --no-interaction --prefer-dist --optimize-autoloader")
        }
        "composer-update" => {
            Some("composer update --no-interaction --prefer-dist --optimize-autoloader")
        }
        "artisan-list" => Some("php artisan list"),
        "artisan-migrate" => Some("php artisan migrate --force"),
        "artisan-storage-link" => Some("php artisan storage:link"),
        "artisan-optimize-clear" => Some("php artisan optimize:clear"),
        "artisan-cache" => {
            Some("php artisan config:cache && php artisan route:cache && php artisan view:cache")
        }
        "artisan-queue-restart" => Some("php artisan queue:restart"),
        "npm-install" => Some("npm install"),
        "npm-build" => Some("npm run build"),
        "wp-info" => Some("wp --info"),
        "wp-core-version" => Some("wp core version"),
        _ => None,
    }
}

fn git_url_contains_credentials(value: &str) -> bool {
    let value = value.trim();
    if let Ok(url) = reqwest::Url::parse(value) {
        let query_contains_credentials = url.query_pairs().any(|(key, _)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "token"
                    | "access_token"
                    | "api_key"
                    | "apikey"
                    | "password"
                    | "passwd"
                    | "secret"
                    | "credential"
                    | "credentials"
            )
        });

        return url.password().is_some()
            || (!url.username().is_empty() && url.username() != "git")
            || query_contains_credentials;
    }

    value
        .split_once('@')
        .is_some_and(|(userinfo, _)| userinfo != "git" || userinfo.contains(':'))
}

fn is_safe_git_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with('-')
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'/' | b'.'))
}

fn all_hostnames(payload: &WebHostingPayload) -> Vec<String> {
    let mut hostnames = Vec::new();

    if let Some(primary) = payload.primary_domain.as_deref()
        && is_safe_hostname(primary)
    {
        hostnames.push(primary.to_string());
    }

    if let Some(domains) = &payload.domains {
        for domain in domains {
            let hostname = domain.hostname.trim().trim_end_matches('.').to_lowercase();
            if is_safe_hostname(&hostname)
                && !hostnames.iter().any(|existing| existing == &hostname)
            {
                hostnames.push(hostname);
            }
        }
    }

    hostnames
}

fn is_safe_hostname(hostname: &str) -> bool {
    let hostname = hostname.trim().trim_end_matches('.');
    !hostname.is_empty()
        && hostname.len() <= 253
        && hostname.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

fn document_root_path(
    server: &crate::server::Server,
    payload: &WebHostingPayload,
) -> Result<std::path::PathBuf, anyhow::Error> {
    let root = payload.document_root.as_deref().unwrap_or("/public_html");
    safe_join(&server.filesystem.base_path, root.trim_start_matches('/'))
}

fn container_path_for_host_path(
    server: &crate::server::Server,
    path: &std::path::Path,
) -> Result<String, anyhow::Error> {
    let relative = path
        .strip_prefix(&server.filesystem.base_path)
        .map_err(|_| anyhow::anyhow!("path escapes the server filesystem"))?;
    let mut container_path = std::path::PathBuf::from("/home/container");
    container_path.push(relative);

    Ok(container_path.to_string_lossy().to_string())
}

fn safe_join(base: &std::path::Path, relative: &str) -> Result<std::path::PathBuf, anyhow::Error> {
    let relative = std::path::Path::new(relative);
    let mut result = base.to_path_buf();

    for component in relative.components() {
        match component {
            std::path::Component::Normal(segment) => result.push(segment),
            std::path::Component::CurDir => {}
            _ => return Err(anyhow::anyhow!("path escapes the server filesystem")),
        }
    }

    Ok(result)
}

fn state_path(
    config: &crate::config::Config,
    server: &crate::server::Server,
) -> std::path::PathBuf {
    std::path::Path::new(&config.load().web_hosting.state_dir)
        .join("servers")
        .join(format!("{}.json", server.uuid))
}

fn vhost_path(
    config: &crate::config::Config,
    server: &crate::server::Server,
) -> std::path::PathBuf {
    std::path::Path::new(&config.load().web_hosting.vhost_dir).join(format!("{}.conf", server.uuid))
}

async fn write_atomic(
    path: &std::path::Path,
    contents: &[u8],
    mode: u32,
) -> Result<(), anyhow::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    tokio::fs::write(&temp, contents).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode)).await?;
    }

    tokio::fs::rename(temp, path).await?;
    Ok(())
}

async fn write_private_state(path: &std::path::Path, contents: &[u8]) -> Result<(), anyhow::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
        }
    }

    write_atomic(path, contents, 0o600).await
}

async fn discard_unreadable_state(
    path: &std::path::Path,
    server: &crate::server::Server,
    error: String,
) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => tracing::warn!(
            server = %server.uuid,
            %error,
            "discarded unreadable web hosting state without returning its contents"
        ),
        Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => {}
        Err(remove_error) => tracing::warn!(
            server = %server.uuid,
            %error,
            %remove_error,
            "could not discard unreadable web hosting state"
        ),
    }
}

async fn writable_directory(path: &str) -> bool {
    let path = std::path::Path::new(path);
    if tokio::fs::metadata(path)
        .await
        .map(|metadata| !metadata.is_dir())
        .unwrap_or(true)
    {
        return false;
    }

    let probe = path.join(format!(".wings-webhosting-probe-{}", std::process::id()));
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .await
    {
        Ok(_) => {
            let _ = tokio::fs::remove_file(probe).await;
            true
        }
        Err(_) => false,
    }
}

fn command_exists(command: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "command -v {} >/dev/null 2>&1",
            command.replace('\'', "'\\''")
        ))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

async fn directory_has_entries(path: &std::path::Path) -> Result<bool, anyhow::Error> {
    if tokio::fs::metadata(path).await.is_err() {
        return Ok(false);
    }

    let mut entries = tokio::fs::read_dir(path).await?;
    Ok(entries.next_entry().await?.is_some())
}

async fn clear_directory(path: &std::path::Path) -> Result<(), anyhow::Error> {
    if tokio::fs::metadata(path).await.is_err() {
        return Ok(());
    }

    let mut entries = tokio::fs::read_dir(path).await?;
    while let Some(entry) = entries.next_entry().await? {
        let entry_path = entry.path();
        let file_type = entry.file_type().await?;
        if file_type.is_dir() {
            tokio::fs::remove_dir_all(entry_path).await?;
        } else {
            tokio::fs::remove_file(entry_path).await?;
        }
    }

    Ok(())
}

async fn run_checked_setup_command(
    server: &crate::server::Server,
    command: String,
    working_dir: &str,
    timeout_seconds: u64,
) -> Result<(), anyhow::Error> {
    run_checked_setup_command_with_env(server, command, working_dir, timeout_seconds, &[]).await
}

async fn run_checked_setup_command_with_env(
    server: &crate::server::Server,
    command: String,
    working_dir: &str,
    timeout_seconds: u64,
    extra_env: &[String],
) -> Result<(), anyhow::Error> {
    let output = crate::server::helper::exec_web_hosting_user_command_with_env(
        server,
        &command,
        Some(working_dir),
        Some(timeout_seconds),
        extra_env,
    )
    .await?;

    if output.exit_code == Some(0) && !output.timed_out {
        return Ok(());
    }

    let message = if output.timed_out {
        "setup command timed out".to_string()
    } else {
        format!("setup command exited with {:?}", output.exit_code)
    };

    Err(anyhow::anyhow!(message))
}

fn required_payload_value(value: Option<&str>, label: &str) -> Result<String, anyhow::Error> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("WordPress {} is required", label))
}

fn site_url(payload: &WebHostingPayload) -> String {
    let raw = payload
        .primary_domain
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("localhost");

    if raw.starts_with("http://") || raw.starts_with("https://") {
        return raw.trim_end_matches('/').to_string();
    }

    let scheme = if payload.ssl_mode.as_deref() == Some("off") {
        "http"
    } else {
        "https"
    };

    format!("{}://{}", scheme, raw.trim_end_matches('/'))
}

fn default_admin_email(payload: &WebHostingPayload) -> String {
    let host = site_url(payload)
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("example.com")
        .trim_start_matches("www.")
        .to_string();

    if host.contains('.') {
        format!("admin@{}", host)
    } else {
        "admin@example.com".to_string()
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_env_value(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn strip_wordpress_prefix(path: &std::path::Path) -> Result<std::path::PathBuf, anyhow::Error> {
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Normal(prefix)) if prefix == "wordpress" => {}
        _ => return Err(anyhow::anyhow!("unexpected WordPress archive layout")),
    }

    let mut relative = std::path::PathBuf::new();
    for component in components {
        match component {
            std::path::Component::Normal(segment) => relative.push(segment),
            std::path::Component::CurDir => {}
            _ => return Err(anyhow::anyhow!("unsafe path in WordPress archive")),
        }
    }

    Ok(relative)
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn js_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('$', "\\$")
}

fn php_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}
