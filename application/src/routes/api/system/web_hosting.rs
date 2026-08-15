use super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

pub(crate) mod get {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::GetState,
    };
    use serde::Serialize;
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    struct Response {
        enabled: bool,
        version: String,
        vhost_dir: String,
        acme_root: String,
        webroot_base: String,
        state_dir: String,
        reload_helper: String,
        bind_address: String,
        http_port: u16,
        https_port: u16,
        standby_bind_address: String,
        standby_http_port: u16,
        standby_https_port: u16,
        vhost_dir_writable: bool,
        reload_helper_present: bool,
        tools: ToolStatus,
        actions: Vec<&'static str>,
        app_templates: Vec<AppTemplate>,
    }

    #[derive(ToSchema, Serialize)]
    struct ToolStatus {
        openresty: bool,
        clamdscan: bool,
        clamscan: bool,
        restic: bool,
        borg: bool,
        git: bool,
        tar: bool,
        php: bool,
        composer: bool,
        wp_cli: bool,
        node: bool,
        npm: bool,
    }

    #[derive(ToSchema, Serialize)]
    struct AppTemplate {
        id: &'static str,
        name: &'static str,
        description: &'static str,
    }

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = inline(Response)),
    ))]
    pub async fn route(state: GetState) -> ApiResponseResult {
        response(state)
    }

    #[utoipa::path(get, path = "/capabilities", responses(
        (status = OK, body = inline(Response)),
    ))]
    pub async fn capabilities(state: GetState) -> ApiResponseResult {
        response(state)
    }

    fn response(state: GetState) -> ApiResponseResult {
        let config = &state.config.web_hosting;

        ApiResponse::new_serialized(Response {
            enabled: config.enabled,
            version: crate::full_version(),
            vhost_dir: config.vhost_dir.clone(),
            acme_root: config.acme_root.clone(),
            webroot_base: config.webroot_base.clone(),
            state_dir: config.state_dir.clone(),
            reload_helper: config.reload_helper.clone(),
            bind_address: config.bind_address.clone(),
            http_port: config.http_port,
            https_port: config.https_port,
            standby_bind_address: config.standby_bind_address.clone(),
            standby_http_port: config.standby_http_port,
            standby_https_port: config.standby_https_port,
            vhost_dir_writable: writable_directory(&config.vhost_dir),
            reload_helper_present: std::path::Path::new(&config.reload_helper).exists(),
            tools: ToolStatus {
                openresty: command_exists("openresty")
                    || std::path::Path::new("/usr/local/openresty/nginx/sbin/nginx").exists(),
                clamdscan: command_exists("clamdscan"),
                clamscan: command_exists("clamscan"),
                restic: command_exists("restic"),
                borg: command_exists("borg"),
                git: command_exists("git"),
                tar: command_exists("tar"),
                php: command_exists("php"),
                composer: command_exists("composer"),
                wp_cli: command_exists("wp"),
                node: command_exists("node"),
                npm: command_exists("npm"),
            },
            actions: vec![
                "sync-vhost",
                "fix-permissions",
                "scan",
                "quarantine",
                "repair-wordpress",
                "backup",
                "restore",
                "deploy",
                "stage",
                "push-staging",
                "redis-provision",
                "audit",
                "cache-purge",
                "runtime-info",
                "run-command",
                "install-app",
            ],
            app_templates: vec![
                AppTemplate {
                    id: "wordpress",
                    name: "WordPress",
                    description: "Provision a database and complete WordPress setup with an admin account.",
                },
                AppTemplate {
                    id: "laravel",
                    name: "Laravel",
                    description: "Create a Laravel app with SQLite and a ready public entrypoint.",
                },
                AppTemplate {
                    id: "react",
                    name: "React / Vite",
                    description: "Install dependencies and publish a production Vite build.",
                },
                AppTemplate {
                    id: "adminer",
                    name: "Adminer",
                    description: "Install the Adminer PHP database management app.",
                },
                AppTemplate {
                    id: "php",
                    name: "PHP starter",
                    description: "Create a PHP landing page in the document root.",
                },
                AppTemplate {
                    id: "static",
                    name: "Static site",
                    description: "Create a static HTML starter.",
                },
            ],
        })
        .ok()
    }

    fn command_exists(command: &str) -> bool {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "command -v {} >/dev/null 2>&1",
                shell_escape(command)
            ))
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn writable_directory(path: &str) -> bool {
        let path = std::path::Path::new(path);
        if !path.is_dir() {
            return false;
        }

        let probe = path.join(format!(".wings-webhosting-probe-{}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(_) => {
                let _ = std::fs::remove_file(probe);
                true
            }
            Err(_) => false,
        }
    }

    fn shell_escape(value: &str) -> String {
        value.replace('\'', "'\\''")
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .routes(routes!(get::capabilities))
        .with_state(state.clone())
}
