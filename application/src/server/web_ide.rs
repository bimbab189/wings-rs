use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};
use anyhow::Context;
use base64::Engine;
use bollard::container::{
    Config as ContainerConfig, CreateContainerOptions, ListContainersOptions,
    RemoveContainerOptions,
};
use bollard::models::{HostConfig, Mount, MountTypeEnum};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};
use yrs::{GetString, ReadTxn, Text, Transact};
use yrs_axum::{AwarenessRef, broadcast::BroadcastGroup};

const COPILOT_MEMORY_MOUNT: &str =
    "/run/jexactyl-webide/user-data/User/globalStorage/github.copilot-chat/memory-tool/memories";
const ENCRYPTED_STATE_FILE: &str = "state.enc";
const PENDING_STATE_PREFIX: &str = ".pending-";
const ENCRYPTED_STATE_MAGIC: &[u8] = b"JXWEBIDE-STATE\0";
const ENCRYPTED_STATE_VERSION: u8 = 1;
const ENCRYPTED_BROWSER_STATE_FILE: &str = "browser-state.enc";
const ENCRYPTED_BROWSER_STATE_MAGIC: &[u8] = b"JXWEBIDE-BROWSER\0";
const ENCRYPTED_BROWSER_STATE_VERSION: u8 = 1;
const USER_STATE_DIRECTORY: &str = "users";
const ENCRYPTED_USER_PROFILE_FILE: &str = "profile.enc";
const ENCRYPTED_USER_BROWSER_STATE_FILE: &str = "browser-state.enc";
const USER_GLOBAL_BROWSER_DATABASE: &str = "jexactyl-webide-user-global";
const USER_GLOBAL_SHARED_BROWSER_DATABASE: &str = "jexactyl-webide-user-global-shared";
const ENCRYPTED_USER_PROFILE_MAGIC: &[u8] = b"JXWEBIDE-USER-PROFILE\0";
const ENCRYPTED_USER_PROFILE_VERSION: u8 = 1;
const MAX_BROWSER_STATE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct WebIdeManager {
    inner: Arc<WebIdeManagerInner>,
}

struct WebIdeManagerInner {
    config: Arc<crate::config::Config>,
    docker: Arc<bollard::Docker>,
    sessions: RwLock<HashMap<uuid::Uuid, WebIdeSession>>,
    session_launch_lock: Mutex<()>,
    consumed_jtis: RwLock<HashMap<String, Instant>>,
    collaboration_rooms: RwLock<HashMap<(uuid::Uuid, String), Arc<CollaborationRoom>>>,
    browser_state_lock: Mutex<()>,
    user_profile_lock: Mutex<()>,
    user_theme_events: RwLock<HashMap<uuid::Uuid, tokio::sync::broadcast::Sender<String>>>,
}

pub struct CollaborationRoom {
    pub group: Arc<BroadcastGroup>,
    pub oversized: Arc<AtomicBool>,
    pub limit_notifier: Arc<tokio::sync::Notify>,
    pub awareness_clients: Arc<StdMutex<HashMap<u64, uuid::Uuid>>>,
    _save_subscription: yrs::Subscription,
}

#[derive(Clone)]
pub struct WebIdeSession {
    pub uuid: uuid::Uuid,
    pub server_uuid: uuid::Uuid,
    pub user_uuid: uuid::Uuid,
    pub display_name: String,
    pub can_use_console: bool,
    pub permissions: crate::server::permissions::Permissions,
    pub container_id: String,
    pub socket_path: PathBuf,
    pub persistent_state_path: PathBuf,
    pub persistent_user_state_path: PathBuf,
    pub cookie_hash: Option<[u8; 32]>,
    pub extension_token_hash: [u8; 32],
    /// Random capability used only by the trusted browser workbench storage
    /// bridge. It is injected into the no-store root document and is never
    /// exposed to the extension host or persisted.
    pub browser_storage_token: String,
    /// Aborts the host-side, session-scoped Unix terminal listener. The
    /// listener is the only path used by native VS Code terminal tools; it is
    /// never published on a TCP port or attached to another IDE session.
    pub terminal_socket_task: Option<tokio::task::AbortHandle>,
    /// Aborts the session-scoped addon-tool Unix listener. It forwards only
    /// bounded, panel-authorized mod/plugin operations and is never exposed
    /// on a TCP port.
    pub addon_tools_socket_task: Option<tokio::task::AbortHandle>,
    /// Panel policy snapshot checked on every local addon-tool request.
    pub copilot_addon_tools_enabled: bool,
    pub agent_requests: Arc<tokio::sync::Semaphore>,
    pub created_at: Instant,
    pub last_interaction: Instant,
    /// Consecutive transient failures while re-validating this session with
    /// the panel. Explicit 4xx revocations still stop immediately; this only
    /// prevents a single timeout or panel 5xx from killing an active IDE.
    pub panel_authorization_failures: u8,
    /// Maximum time without a browser/extension presence lease before the
    /// sidecar is reaped. This is separate from the longer idle timeout.
    pub presence_timeout: Duration,
    pub idle_timeout: Duration,
    pub maximum_lifetime: Duration,
}

pub struct StartWebIdeSession {
    pub session_uuid: uuid::Uuid,
    pub user_uuid: uuid::Uuid,
    /// Panel-calculated aggregate allowance. None means unlimited for this
    /// user; Wings still enforces its node-wide hard cap below.
    pub user_session_limit: Option<usize>,
    /// Encrypted state is single-writer for a server/user pair. This is kept
    /// separate from the node-wide max_sessions_per_server setting.
    pub user_server_session_limit: Option<usize>,
    pub display_name: String,
    pub can_use_console: bool,
    pub copilot_addon_tools_enabled: bool,
    pub permissions: crate::server::permissions::Permissions,
    pub has_file_denylist: Option<bool>,
    pub presence_timeout: Duration,
    pub idle_timeout: Duration,
    pub maximum_lifetime: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelEventStatus {
    Accepted,
    Rejected,
    Unavailable,
}

fn panel_status_is_rejection(status: Option<reqwest::StatusCode>) -> bool {
    status.is_some_and(|status| {
        status.is_client_error()
            && status != reqwest::StatusCode::REQUEST_TIMEOUT
            && status != reqwest::StatusCode::TOO_MANY_REQUESTS
    })
}

#[derive(Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct PersistentBrowserState {
    #[serde(default)]
    secrets: HashMap<String, String>,
    #[serde(default)]
    databases: HashMap<String, HashMap<String, String>>,
}

pub enum BrowserStateOperation {
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
        insert: Vec<(String, String)>,
        delete: Vec<String>,
    },
    StorageClear {
        database: String,
    },
}

pub enum BrowserStateResult {
    Empty,
    Secret(Option<String>),
    Keys(Vec<String>),
    Entries {
        entries: Vec<(String, String)>,
        present: bool,
    },
}

impl WebIdeManager {
    pub fn new(config: Arc<crate::config::Config>, docker: Arc<bollard::Docker>) -> Self {
        Self {
            inner: Arc::new(WebIdeManagerInner {
                config,
                docker,
                sessions: RwLock::new(HashMap::new()),
                session_launch_lock: Mutex::new(()),
                consumed_jtis: RwLock::new(HashMap::new()),
                collaboration_rooms: RwLock::new(HashMap::new()),
                browser_state_lock: Mutex::new(()),
                user_profile_lock: Mutex::new(()),
                user_theme_events: RwLock::new(HashMap::new()),
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.config.web_ide.enabled && self.validate_security_configuration().is_ok()
    }

    pub async fn start(
        &self,
        server: &crate::server::Server,
        request: StartWebIdeSession,
    ) -> Result<bool, anyhow::Error> {
        if !self.enabled() {
            self.validate_security_configuration()?;
            anyhow::bail!("web IDE is disabled");
        }
        let _launch_guard = self.inner.session_launch_lock.lock().await;

        let stale_duplicate = {
            let sessions = self.inner.sessions.read().await;
            if sessions.contains_key(&request.session_uuid) {
                return Ok(false);
            }
            sessions
                .values()
                .find(|session| {
                    session.server_uuid == server.uuid && session.user_uuid == request.user_uuid
                })
                .and_then(|session| {
                    let presence_expired = session.cookie_hash.is_some()
                        && session.last_interaction.elapsed() > session.presence_timeout;
                    let launch_expired = session.cookie_hash.is_none()
                        && session.created_at.elapsed() > Duration::from_secs(120);
                    (presence_expired || launch_expired).then_some(session.uuid)
                })
        };
        if let Some(session_uuid) = stale_duplicate {
            // A panel row can be missing/revoked after a node restart. Wings
            // still owns the authoritative live session, so release it only
            // when its browser lease is already expired, then retry the normal
            // capacity checks below.
            self.stop(session_uuid, "stale_presence_replaced").await;
        }

        let sessions = self.inner.sessions.read().await;
        if sessions.len() >= self.inner.config.web_ide.max_sessions {
            anyhow::bail!("node Web IDE session limit reached");
        }
        if sessions
            .values()
            .filter(|session| session.server_uuid == server.uuid)
            .count()
            >= self.inner.config.web_ide.max_sessions_per_server
        {
            anyhow::bail!("server Web IDE session limit reached");
        }
        let user_server_count = sessions
            .values()
            .filter(|session| {
                session.server_uuid == server.uuid && session.user_uuid == request.user_uuid
            })
            .count();
        let user_server_limit_reached = request
            .user_server_session_limit
            .map(|limit| user_server_count >= limit)
            // Payloads from an older panel did not carry an explicit limit;
            // retain their original duplicate-session guard while rolling out.
            .unwrap_or(user_server_count > 0);
        if user_server_limit_reached {
            anyhow::bail!("this user already has an active Web IDE session for the server");
        }
        if request.user_session_limit.is_some_and(|limit| {
            sessions
                .values()
                .filter(|session| session.user_uuid == request.user_uuid)
                .count()
                >= limit
        }) {
            anyhow::bail!("user Web IDE session limit reached");
        }
        drop(sessions);

        // Sidecars do not inherit game-container mounts, networking, devices,
        // or privileges, so those egg/container features do not affect IDE
        // isolation. A file denylist is different: a raw workspace bind mount
        // cannot enforce per-path filtering. New panels send this fact from
        // their canonical egg model so startup never waits on a game-container
        // configuration lock. During rolling upgrades, older payloads are
        // accepted only when Wings can verify its local configuration quickly;
        // a held lock fails closed.
        let has_file_denylist = match request.has_file_denylist {
            Some(value) => value,
            None => tokio::time::timeout(Duration::from_secs(2), async {
                !server
                    .configuration
                    .read()
                    .await
                    .egg
                    .file_denylist
                    .is_empty()
            })
            .await
            .unwrap_or(true),
        };
        if has_file_denylist {
            anyhow::bail!("server is not eligible for Web IDE because its egg has a file denylist");
        }

        let (uid, gid) = if self.inner.config.system.user.rootless.enabled {
            (
                self.inner.config.system.user.rootless.container_uid,
                self.inner.config.system.user.rootless.container_gid,
            )
        } else {
            (
                self.inner.config.system.user.uid,
                self.inner.config.system.user.gid,
            )
        };
        if uid == 0 || gid == 0 {
            anyhow::bail!("Web IDE requires a non-zero container uid and gid");
        }

        let runtime_root = PathBuf::from(&self.inner.config.web_ide.runtime_directory);
        tokio::fs::create_dir_all(&runtime_root)
            .await
            .context("failed to create Web IDE runtime root")?;
        let runtime_path = runtime_root.join(request.session_uuid.to_string());
        if let Ok(metadata) = tokio::fs::symlink_metadata(&runtime_path).await
            && (metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            anyhow::bail!("Web IDE runtime session path is not a real directory");
        }
        tokio::fs::create_dir_all(&runtime_path)
            .await
            .context("failed to create Web IDE runtime directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&runtime_root, std::fs::Permissions::from_mode(0o700))
                .await?;
            tokio::fs::set_permissions(&runtime_path, std::fs::Permissions::from_mode(0o700))
                .await?;
        }

        // The code-server process and its plaintext state stay session-scoped.
        // Wings restores the authenticated server+panel-user archive into this
        // runtime and encrypts it again after the sidecar stops. The session
        // settings file remains on the runtime mount so another session cannot
        // inherit this tab's endpoint or extension credential.
        let persistent_path = self
            .prepare_persistent_directory(server.uuid, request.user_uuid, uid, gid)
            .await?;
        let persistent_user_state_path = self
            .prepare_user_persistent_directory(request.user_uuid)
            .await?;
        let runtime_user_data_root = runtime_path.join("user-data");
        let runtime_user_data_placeholder = runtime_user_data_root.join("User");
        let runtime_extensions_path = runtime_path.join("extensions");
        let runtime_memory_path = runtime_path.join("memory");
        let runtime_home_placeholder = runtime_path.join("home");
        let legacy_memory_path = persistent_memory_path(
            Path::new(&self.inner.config.web_ide.memory_directory),
            server.uuid,
            request.user_uuid,
        );
        self.restore_encrypted_state(
            &persistent_path,
            &runtime_user_data_placeholder,
            &runtime_extensions_path,
            &runtime_memory_path,
            &runtime_home_placeholder,
            &legacy_memory_path,
            uid,
            gid,
        )
        .await?;
        tokio::fs::create_dir_all(&runtime_user_data_placeholder).await?;
        tokio::fs::create_dir_all(&runtime_extensions_path).await?;
        tokio::fs::create_dir_all(&runtime_memory_path).await?;
        tokio::fs::create_dir_all(&runtime_home_placeholder).await?;
        let lock_root = runtime_user_data_placeholder.clone();
        tokio::task::spawn_blocking(move || remove_workspace_storage_locks(&lock_root)).await??;
        let settings_path = runtime_user_data_placeholder.join("settings.json");
        self.restore_user_profile(&persistent_user_state_path, &settings_path, uid, gid)
            .await?;
        self.seed_security_note(&runtime_memory_path, uid, gid)
            .await?;
        let home = runtime_home_placeholder.clone();
        let global_storage = runtime_user_data_placeholder.join("globalStorage");
        let copilot_storage = global_storage.join("github.copilot-chat");
        let memory_mount_parent = copilot_storage.join("memory-tool");
        tokio::fs::create_dir_all(&memory_mount_parent).await?;
        let endpoint = format!(
            "{}/api/servers/{}/web-ide/s/{}",
            self.inner
                .config
                .web_ide
                .public_url
                .trim_end_matches('/')
                .replacen("https://", "wss://", 1),
            server.uuid,
            request.session_uuid
        );
        let public_network_enabled = self.inner.config.web_ide.allow_public_network;
        let allowed_network_domains: Vec<&str> = if public_network_enabled {
            vec!["*"]
        } else {
            Vec::new()
        };
        let denied_network_domains: Vec<&str> = if public_network_enabled {
            Vec::new()
        } else {
            vec!["*"]
        };
        // A separate, random, session-lifetime credential lets the trusted
        // workspace extension reach Wings from the sidecar. It is
        // not the browser cookie or launch JWT, cannot outlive this sidecar,
        // and is removed with the runtime directory.
        let extension_token = random_url_token(32);
        let extension_token_hash = Sha256::digest(extension_token.as_bytes()).into();
        let browser_storage_token = random_url_token(32);
        let managed_settings = serde_json::json!({
            "jexactyl.webIde.endpoint": endpoint,
            "jexactyl.webIde.extensionToken": extension_token,
            "jexactyl.webIde.displayName": request.display_name.clone(),
            "jexactyl.webIde.canUseConsole": request.can_use_console,
            "jexactyl.webIde.addonToolsEnabled": request.copilot_addon_tools_enabled,
            // This is a fixed path inside the session-only runtime bind. It
            // is not a host path and is usable only by the non-root sidecar
            // process that owns this Web IDE session.
            "jexactyl.webIde.addonToolsSocket": "/run/jexactyl-webide/addon-tools.sock",
            "extensions.autoCheckUpdates": false,
            "extensions.autoUpdate": false,
            "security.workspace.trust.enabled": false,
            // Explicitly remove every built-in process profile. The
            // first-party extension contributes the only remaining
            // profile, `Jexactyl Server`; null values remove VS Code's
            // default Bash/zsh/fish/tmux profiles without relying on a
            // user-editable shell path.
            "terminal.integrated.profiles.linux": {
                "bash": null,
                "zsh": null,
                "fish": null,
                "tmux": null,
                "pwsh": null
            },
            "terminal.integrated.defaultProfile.linux": "Jexactyl Server",
            "terminal.integrated.automationProfile.linux": null,
            "terminal.integrated.agentHostProfile.linux": null,
            // VS Code's native Execute tools require a process-backed PTY.
            // This fixed executable is a WebSocket connector to the same
            // permission-checked Wings terminal, never an OS shell.
            "chat.tools.terminal.terminalProfile.linux": {
                "path": "/opt/jexactyl/bin/jexactyl-terminal"
            },
            "terminal.integrated.shellIntegration.enabled": false,
            "chat.tools.terminal.outputLocation": "terminal",
            // Public egress is an explicit, temporary node setting. The
            // firewall still denies node/private destinations, while the
            // pinned Copilot build has browser/fetch tools removed.
            "chat.agent.networkFilter": !public_network_enabled,
            "chat.agent.allowedNetworkDomains": allowed_network_domains,
            "chat.agent.deniedNetworkDomains": denied_network_domains,
            // MCP is intentionally unavailable in a customer sidecar.
            // This also guarantees that an MCP supplied by a workspace
            // cannot be discovered or started.
            "chat.mcp.access": "none",
            "chat.mcp.gallery.enabled": false,
            "chat.mcp.autostart": "never",
            "chat.mcp.apps.enabled": false,
            // Expose the same native VS Code tool groups as the pinned
            // Copilot build, including subagents, memory, notebooks,
            // extension install, and workspace setup helpers.
            "github.copilot.chat.executionSubagent.enabled": true,
            "github.copilot.chat.searchSubagent.enabled": true,
            "github.copilot.chat.exploreAgent.enabled": true,
            "github.copilot.chat.skillTool.enabled": true,
            "github.copilot.chat.switchAgent.enabled": true,
            "github.copilot.chat.installExtensionSkill.enabled": false,
            "github.copilot.chat.projectSetupInfoSkill.enabled": true,
            "github.copilot.chat.newWorkspaceCreation.enabled": true,
            "github.copilot.chat.getChangedFilesTool.enabled": true,
            "github.copilot.chat.gpt55GetChangedFilesTool.enabled": true,
            "github.copilot.chat.gpt55ReadFileTool.enabled": true,
            "github.copilot.chat.codesearch.enabled": true,
            "github.copilot.chat.setupTests.enabled": true,
            "github.copilot.chat.tools.viewImage.enabled": true,
            "github.copilot.chat.workspace.enableCodeSearch": true,
            "github.copilot.chat.githubMcpServer.enabled": false,
            "github.copilot.chat.cli.mcp.enabled": false,
            "github.copilot.chat.backgroundAgent.enabled": false,
            "github.copilot.chat.cloudAgent.enabled": false,
            "github.copilot.chat.tools.defaultToolsGrouped": false,
            "workbench.startupEditor": "none",
            "telemetry.telemetryLevel": "off",
            "remote.autoForwardPorts": false
        });
        write_managed_settings(&settings_path, &managed_settings).await?;
        #[cfg(unix)]
        {
            for path in [
                &runtime_path,
                &runtime_user_data_root,
                &runtime_user_data_placeholder,
                &runtime_extensions_path,
                &runtime_memory_path,
                &home,
                &global_storage,
                &copilot_storage,
                &memory_mount_parent,
                &settings_path,
            ] {
                std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
            }
        }

        let socket_path = runtime_path.join("ide.sock");
        let workspace = self.inner.config.data_path(server.uuid);
        let container_name = format!("jexide-{}", random_url_token(10));
        let labels = HashMap::from([
            ("com.jexactyl.webide".to_string(), "true".to_string()),
            (
                "com.jexactyl.webide.session".to_string(),
                request.session_uuid.to_string(),
            ),
            (
                "com.jexactyl.webide.server".to_string(),
                server.uuid.to_string(),
            ),
            (
                "com.jexactyl.webide.user".to_string(),
                request.user_uuid.to_string(),
            ),
        ]);

        let memory = (self.inner.config.web_ide.memory_mib * 1024 * 1024) as i64;
        let cpu_quota = (self.inner.config.web_ide.cpu_percent as i64) * 1000;
        let container = match self
            .inner
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: container_name,
                    ..Default::default()
                }),
                ContainerConfig {
                    image: Some(self.inner.config.web_ide.image.clone()),
                    user: Some(format!("{uid}:{gid}")),
                    working_dir: Some("/home/container".to_string()),
                    env: Some(vec![
                        "HOME=/run/jexactyl-webide/home".to_string(),
                        "SHELL=/opt/jexactyl/bin/jexactyl-terminal".to_string(),
                    ]),
                    cmd: Some(vec![
                        "/opt/code-server/bin/code-server".to_string(),
                        "--socket".to_string(),
                        "/run/jexactyl-webide/ide.sock".to_string(),
                        "--socket-mode".to_string(),
                        "0600".to_string(),
                        "--auth".to_string(),
                        "none".to_string(),
                        "--disable-proxy".to_string(),
                        "--disable-telemetry".to_string(),
                        "--disable-update-check".to_string(),
                        "--disable-workspace-trust".to_string(),
                        "--user-data-dir".to_string(),
                        "/run/jexactyl-webide/user-data".to_string(),
                        "--extensions-dir".to_string(),
                        "/run/jexactyl-webide/extensions".to_string(),
                        "/home/container".to_string(),
                    ]),
                    labels: Some(labels),
                    host_config: Some(HostConfig {
                        // `bridge` is Docker's isolated default bridge, not
                        // the shared Pterodactyl customer network. It is only
                        // selected after the node firewall verification flag
                        // has been set in config.yml.
                        network_mode: Some(if public_network_enabled {
                            "bridge".to_string()
                        } else {
                            "none".to_string()
                        }),
                        readonly_rootfs: Some(true),
                        cap_drop: Some(vec!["ALL".to_string()]),
                        security_opt: Some(vec!["no-new-privileges:true".to_string()]),
                        memory: Some(memory),
                        memory_swap: Some(memory),
                        cpu_period: Some(100_000),
                        cpu_quota: Some(cpu_quota),
                        pids_limit: Some(self.inner.config.web_ide.pid_limit),
                        mounts: Some(vec![
                            Mount {
                                typ: Some(MountTypeEnum::BIND),
                                source: Some(workspace.to_string_lossy().into_owned()),
                                target: Some("/home/container".to_string()),
                                read_only: Some(false),
                                ..Default::default()
                            },
                            Mount {
                                typ: Some(MountTypeEnum::BIND),
                                source: Some(runtime_path.to_string_lossy().into_owned()),
                                target: Some("/run/jexactyl-webide".to_string()),
                                read_only: Some(false),
                                ..Default::default()
                            },
                            // Memory is restored into the session runtime and
                            // overlaid at Copilot's canonical path. It is
                            // encrypted together with User, extensions and
                            // HOME when the session stops.
                            Mount {
                                typ: Some(MountTypeEnum::BIND),
                                source: Some(runtime_memory_path.to_string_lossy().into_owned()),
                                target: Some(COPILOT_MEMORY_MOUNT.to_string()),
                                read_only: Some(false),
                                ..Default::default()
                            },
                            Mount {
                                typ: Some(MountTypeEnum::BIND),
                                source: Some(settings_path.to_string_lossy().into_owned()),
                                target: Some(
                                    "/run/jexactyl-webide/user-data/User/settings.json".to_string(),
                                ),
                                read_only: Some(false),
                                ..Default::default()
                            },
                        ]),
                        tmpfs: Some(HashMap::from([(
                            "/tmp".to_string(),
                            format!(
                                "rw,noexec,nosuid,nodev,size={}M",
                                self.inner.config.web_ide.tmpfs_mib
                            ),
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(container) => container,
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&runtime_path).await;
                return Err(error).context("failed to create Web IDE container");
            }
        };

        if let Err(error) = self
            .inner
            .docker
            .start_container::<String>(&container.id, None)
            .await
        {
            let _ = self
                .inner
                .docker
                .remove_container(
                    &container.id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
            let _ = tokio::fs::remove_dir_all(&runtime_path).await;
            anyhow::bail!("failed to start Web IDE container: {error}");
        }

        for _ in 0..100 {
            if tokio::fs::metadata(&socket_path).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if tokio::fs::metadata(&socket_path).await.is_err() {
            self.remove_container(&container.id).await;
            let _ = tokio::fs::remove_dir_all(&runtime_path).await;
            anyhow::bail!("Web IDE did not create its Unix socket");
        }

        self.inner.sessions.write().await.insert(
            request.session_uuid,
            WebIdeSession {
                uuid: request.session_uuid,
                server_uuid: server.uuid,
                user_uuid: request.user_uuid,
                display_name: request.display_name,
                can_use_console: request.can_use_console,
                permissions: request.permissions,
                container_id: container.id,
                socket_path,
                persistent_state_path: persistent_path,
                persistent_user_state_path,
                cookie_hash: None,
                extension_token_hash,
                browser_storage_token,
                terminal_socket_task: None,
                addon_tools_socket_task: None,
                copilot_addon_tools_enabled: request.copilot_addon_tools_enabled,
                agent_requests: Arc::new(tokio::sync::Semaphore::new(2)),
                created_at: Instant::now(),
                last_interaction: Instant::now(),
                panel_authorization_failures: 0,
                presence_timeout: request.presence_timeout,
                idle_timeout: request.idle_timeout,
                maximum_lifetime: request.maximum_lifetime,
            },
        );

        tracing::info!(
            server = %server.uuid,
            session = %request.session_uuid,
            user = %request.user_uuid,
            "started Web IDE session"
        );
        Ok(true)
    }

    pub async fn consume_launch(
        &self,
        session_uuid: uuid::Uuid,
        server_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
        jti: &str,
        permissions: crate::server::permissions::Permissions,
        can_use_console: bool,
        expiration: Instant,
    ) -> Result<String, anyhow::Error> {
        {
            let sessions = self.inner.sessions.read().await;
            let session = sessions
                .get(&session_uuid)
                .context("unknown Web IDE session")?;
            if session.server_uuid != server_uuid
                || session.user_uuid != user_uuid
                || session.cookie_hash.is_some()
            {
                anyhow::bail!("invalid or already consumed Web IDE launch");
            }
        }

        let mut consumed = self.inner.consumed_jtis.write().await;
        if consumed.contains_key(jti) {
            anyhow::bail!("Web IDE launch token was already used");
        }
        consumed.insert(jti.to_string(), expiration);
        drop(consumed);

        let cookie = random_url_token(32);
        let hash = Sha256::digest(cookie.as_bytes()).into();
        let mut sessions = self.inner.sessions.write().await;
        let session = sessions
            .get_mut(&session_uuid)
            .context("unknown Web IDE session")?;
        // The signed one-time JWT is authoritative. Refreshing the snapshot at
        // exchange time also makes rolling upgrades compatible with older
        // panel start payloads that did not yet include `permissions`.
        session.permissions = permissions;
        session.can_use_console = can_use_console;
        session.cookie_hash = Some(hash);
        drop(sessions);
        if self
            .report_panel_event(server_uuid, session_uuid, "opened", None)
            .await
            != PanelEventStatus::Accepted
        {
            self.stop(session_uuid, "panel_authorization_failed").await;
            anyhow::bail!("panel did not authorize the Web IDE session");
        }
        Ok(cookie)
    }

    pub(crate) async fn attach_terminal_socket_task(
        &self,
        session_uuid: uuid::Uuid,
        task: tokio::task::AbortHandle,
    ) -> bool {
        let mut sessions = self.inner.sessions.write().await;
        let Some(session) = sessions.get_mut(&session_uuid) else {
            task.abort();
            return false;
        };
        if let Some(previous) = session.terminal_socket_task.replace(task) {
            previous.abort();
        }
        true
    }

    pub(crate) async fn attach_addon_tools_socket_task(
        &self,
        session_uuid: uuid::Uuid,
        task: tokio::task::AbortHandle,
    ) -> bool {
        let mut sessions = self.inner.sessions.write().await;
        let Some(session) = sessions.get_mut(&session_uuid) else {
            task.abort();
            return false;
        };
        if let Some(previous) = session.addon_tools_socket_task.replace(task) {
            previous.abort();
        }
        true
    }

    /// Authorizes the private Unix terminal socket. Possession of the socket
    /// is session-scoped by its 0700 parent and 0600 ownership; this check also
    /// makes revocation and permission changes fail closed at command time.
    pub(crate) async fn authenticate_local_terminal(
        &self,
        session_uuid: uuid::Uuid,
        server_uuid: uuid::Uuid,
        interaction: bool,
    ) -> Option<WebIdeSession> {
        let mut sessions = self.inner.sessions.write().await;
        let session = sessions.get_mut(&session_uuid)?;
        if session.server_uuid != server_uuid || !session.can_use_console {
            return None;
        }
        if interaction {
            session.last_interaction = Instant::now();
        }
        Some(session.clone())
    }

    /// Authorizes the private addon-tools Unix socket. Filesystem ownership
    /// binds the socket to one sidecar; this check additionally enforces the
    /// exact server/session and current lifetime/policy at request time.
    pub(crate) async fn authenticate_local_addon_tools(
        &self,
        session_uuid: uuid::Uuid,
        server_uuid: uuid::Uuid,
        interaction: bool,
    ) -> Option<WebIdeSession> {
        let mut sessions = self.inner.sessions.write().await;
        let session = sessions.get_mut(&session_uuid)?;
        if session.server_uuid != server_uuid || !session.copilot_addon_tools_enabled {
            return None;
        }
        if session.created_at.elapsed() > session.maximum_lifetime
            || (session.cookie_hash.is_some()
                && session.last_interaction.elapsed() > session.presence_timeout)
            || session.last_interaction.elapsed() > session.idle_timeout
        {
            return None;
        }
        if interaction {
            session.last_interaction = Instant::now();
        }
        Some(session.clone())
    }

    pub async fn authenticate_cookie(
        &self,
        session_uuid: uuid::Uuid,
        server_uuid: uuid::Uuid,
        cookie: &str,
        interaction: bool,
    ) -> Option<WebIdeSession> {
        let candidate: [u8; 32] = Sha256::digest(cookie.as_bytes()).into();
        let mut sessions = self.inner.sessions.write().await;
        let session = sessions.get_mut(&session_uuid)?;
        if session.server_uuid != server_uuid
            || session
                .cookie_hash
                .as_ref()
                .is_none_or(|stored| !constant_time_eq::constant_time_eq(stored, &candidate))
            || session.created_at.elapsed() > session.maximum_lifetime
            || (session.cookie_hash.is_some()
                && session.last_interaction.elapsed() > session.presence_timeout)
            || session.last_interaction.elapsed() > session.idle_timeout
        {
            return None;
        }
        if interaction {
            session.last_interaction = Instant::now();
        }
        Some(session.clone())
    }

    pub async fn authenticate_extension(
        &self,
        session_uuid: uuid::Uuid,
        server_uuid: uuid::Uuid,
        token: &str,
        interaction: bool,
    ) -> Option<WebIdeSession> {
        let candidate: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let mut sessions = self.inner.sessions.write().await;
        let session = sessions.get_mut(&session_uuid)?;
        if session.server_uuid != server_uuid
            || !constant_time_eq::constant_time_eq(&session.extension_token_hash, &candidate)
            || session.created_at.elapsed() > session.maximum_lifetime
            || (session.cookie_hash.is_some()
                && session.last_interaction.elapsed() > session.presence_timeout)
            || session.last_interaction.elapsed() > session.idle_timeout
        {
            return None;
        }
        if interaction {
            session.last_interaction = Instant::now();
        }
        Some(session.clone())
    }

    /// Renew the browser lease from the authenticated panel-to-Wings API.
    /// This is deliberately separate from the browser cookie/extension
    /// endpoints: the panel already authenticates this route with the node's
    /// bearer token and must not receive a session credential.
    pub async fn touch_presence(
        &self,
        session_uuid: uuid::Uuid,
        server_uuid: uuid::Uuid,
    ) -> Result<(), anyhow::Error> {
        let mut sessions = self.inner.sessions.write().await;
        let session = sessions
            .get_mut(&session_uuid)
            .ok_or_else(|| anyhow::anyhow!("Web IDE session not found"))?;
        if session.server_uuid != server_uuid
            || session.created_at.elapsed() > session.maximum_lifetime
            || session.last_interaction.elapsed() > session.presence_timeout
            || session.last_interaction.elapsed() > session.idle_timeout
        {
            anyhow::bail!("Web IDE session is no longer active");
        }
        session.last_interaction = Instant::now();
        Ok(())
    }

    /// Durable storage for browser-only VS Code state. Browser builds keep
    /// extension SecretStorage and the Chat session index in IndexedDB, which
    /// is tied to a changing session URL in this deployment. Mirror those
    /// records into an authenticated AES-GCM file scoped to the exact
    /// server/user pair. The file is never mounted into the sidecar.
    pub async fn browser_state(
        &self,
        session: &WebIdeSession,
        operation: BrowserStateOperation,
    ) -> Result<BrowserStateResult, anyhow::Error> {
        self.browser_state_scoped(session, operation, false).await
    }

    /// Durable state shared by every server opened by the same panel user.
    /// Only VS Code's global browser databases and SecretStorage use this
    /// path; workspace databases and chat transcripts remain server-scoped.
    pub async fn user_browser_state(
        &self,
        session: &WebIdeSession,
        operation: BrowserStateOperation,
    ) -> Result<BrowserStateResult, anyhow::Error> {
        self.browser_state_scoped(session, operation, true).await
    }

    /// Return the user's portable color theme. Settings are authoritative
    /// once present; the browser-state fallback migrates themes selected by
    /// older builds that only mirrored VS Code's `colorThemeData` record.
    pub async fn user_theme(
        &self,
        session: &WebIdeSession,
    ) -> Result<Option<String>, anyhow::Error> {
        let _guard = self.inner.user_profile_lock.lock().await;
        let key = self.encryption_key().await?;
        let profile_path = session
            .persistent_user_state_path
            .join(ENCRYPTED_USER_PROFILE_FILE);
        let user_uuid = session.user_uuid;
        let max = self.inner.config.web_ide.max_persistent_state_bytes;
        let theme = tokio::task::spawn_blocking(move || {
            let value = match fs::read(&profile_path) {
                Ok(encrypted) => decrypt_user_profile_value(&encrypted, &key, user_uuid, max)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    serde_json::json!({})
                }
                Err(error) => return Err(error.into()),
            };
            Ok::<_, anyhow::Error>(
                value
                    .get("workbench.colorTheme")
                    .and_then(serde_json::Value::as_str)
                    .filter(|theme| valid_user_theme(theme))
                    .map(str::to_owned),
            )
        })
        .await??;
        drop(_guard);
        if theme.is_some() {
            return Ok(theme);
        }

        let BrowserStateResult::Entries { entries, .. } = self
            .user_browser_state(
                session,
                BrowserStateOperation::StorageSnapshot {
                    database: USER_GLOBAL_BROWSER_DATABASE.to_owned(),
                },
            )
            .await?
        else {
            return Ok(None);
        };
        Ok(entries
            .into_iter()
            .find_map(|(key, value)| (key == "colorThemeData").then_some(value))
            .and_then(|value| browser_theme_name(&value)))
    }

    /// Persist one portable theme for this panel user. The encrypted profile
    /// is shared across that user's servers, while chat/workspace state stays
    /// in its existing server-scoped archive.
    pub async fn set_user_theme(
        &self,
        session: &WebIdeSession,
        theme: String,
    ) -> Result<(), anyhow::Error> {
        if !valid_user_theme(&theme) {
            anyhow::bail!("Web IDE theme is invalid");
        }
        let _guard = self.inner.user_profile_lock.lock().await;
        let key = self.encryption_key().await?;
        let persistent_path = session.persistent_user_state_path.clone();
        let user_uuid = session.user_uuid;
        let published_theme = theme.clone();
        let max = self.inner.config.web_ide.max_persistent_state_bytes;
        tokio::task::spawn_blocking(move || {
            let profile_path = persistent_path.join(ENCRYPTED_USER_PROFILE_FILE);
            let mut value = match fs::read(&profile_path) {
                Ok(encrypted) => decrypt_user_profile_value(&encrypted, &key, user_uuid, max)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    serde_json::json!({})
                }
                Err(error) => return Err(error.into()),
            };
            value
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("Web IDE user profile must be an object"))?
                .insert(
                    "workbench.colorTheme".to_owned(),
                    serde_json::Value::String(theme),
                );
            write_encrypted_user_profile_value(&persistent_path, &key, user_uuid, max, &value)
        })
        .await??;
        drop(_guard);
        let sender = {
            let mut events = self.inner.user_theme_events.write().await;
            events
                .entry(user_uuid)
                .or_insert_with(|| tokio::sync::broadcast::channel(32).0)
                .clone()
        };
        let _ = sender.send(published_theme);
        Ok(())
    }

    pub(crate) async fn subscribe_user_theme(
        &self,
        user_uuid: uuid::Uuid,
    ) -> tokio::sync::broadcast::Receiver<String> {
        let mut events = self.inner.user_theme_events.write().await;
        events
            .entry(user_uuid)
            .or_insert_with(|| tokio::sync::broadcast::channel(32).0)
            .subscribe()
    }

    async fn browser_state_scoped(
        &self,
        session: &WebIdeSession,
        operation: BrowserStateOperation,
        user_scoped: bool,
    ) -> Result<BrowserStateResult, anyhow::Error> {
        validate_browser_state_scope(user_scoped, &operation)?;
        let operation = if user_scoped {
            match operation {
                BrowserStateOperation::StorageSnapshot { database } => {
                    BrowserStateOperation::StorageSnapshot {
                        database: canonical_user_browser_database_name(&database).to_owned(),
                    }
                }
                BrowserStateOperation::StorageUpdate {
                    database,
                    insert,
                    delete,
                } => BrowserStateOperation::StorageUpdate {
                    database: canonical_user_browser_database_name(&database).to_owned(),
                    insert,
                    delete,
                },
                BrowserStateOperation::StorageClear { database } => {
                    BrowserStateOperation::StorageClear {
                        database: canonical_user_browser_database_name(&database).to_owned(),
                    }
                }
                operation => operation,
            }
        } else {
            operation
        };
        let _guard = self.inner.browser_state_lock.lock().await;
        let key = self.encryption_key().await?;
        let path = if user_scoped {
            session
                .persistent_user_state_path
                .join(ENCRYPTED_USER_BROWSER_STATE_FILE)
        } else {
            session
                .persistent_state_path
                .join(ENCRYPTED_BROWSER_STATE_FILE)
        };
        let server_uuid = session.server_uuid;
        let user_uuid = session.user_uuid;
        let persistent_root = PathBuf::from(&self.inner.config.web_ide.persistent_data_directory);
        let (mut state, migrated) = tokio::task::spawn_blocking(move || {
            if user_scoped {
                read_persistent_user_browser_state(&path, &persistent_root, &key, user_uuid)
            } else {
                read_persistent_browser_state(&path, &key, server_uuid, user_uuid)
                    .map(|state| (state, false))
            }
        })
        .await??;

        let mut changed = migrated;
        let result = match operation {
            BrowserStateOperation::SecretGet { key } => {
                validate_browser_key(&key, 1024)?;
                BrowserStateResult::Secret(state.secrets.get(&key).cloned())
            }
            BrowserStateOperation::SecretSet { key, value } => {
                validate_browser_key(&key, 1024)?;
                if value.len() > 256 * 1024 {
                    anyhow::bail!("browser secret exceeds the configured limit");
                }
                if !state.secrets.contains_key(&key) && state.secrets.len() >= 512 {
                    anyhow::bail!("browser secret count exceeds the configured limit");
                }
                state.secrets.insert(key, value);
                changed = true;
                BrowserStateResult::Empty
            }
            BrowserStateOperation::SecretDelete { key } => {
                validate_browser_key(&key, 1024)?;
                changed = state.secrets.remove(&key).is_some();
                BrowserStateResult::Empty
            }
            BrowserStateOperation::SecretKeys => {
                let mut keys: Vec<_> = state.secrets.keys().cloned().collect();
                keys.sort_unstable();
                BrowserStateResult::Keys(keys)
            }
            BrowserStateOperation::StorageSnapshot { database } => {
                validate_browser_key(&database, 512)?;
                let present = state.databases.contains_key(&database);
                let mut entries: Vec<_> = state
                    .databases
                    .get(&database)
                    .into_iter()
                    .flat_map(|values| values.iter())
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
                BrowserStateResult::Entries { entries, present }
            }
            BrowserStateOperation::StorageUpdate {
                database,
                insert,
                delete,
            } => {
                validate_browser_key(&database, 512)?;
                if insert.len().saturating_add(delete.len()) > 4096 {
                    anyhow::bail!("browser storage update contains too many entries");
                }
                if !state.databases.contains_key(&database) && state.databases.len() >= 128 {
                    anyhow::bail!("browser storage database count exceeds the configured limit");
                }
                let values = state.databases.entry(database).or_default();
                for (key, value) in insert {
                    validate_browser_key(&key, 2048)?;
                    if value.len() > 1024 * 1024 {
                        anyhow::bail!("browser storage value exceeds the configured limit");
                    }
                    values.insert(key, value);
                }
                for key in delete {
                    validate_browser_key(&key, 2048)?;
                    values.remove(&key);
                }
                if values.len() > 16_384 {
                    anyhow::bail!("browser storage entry count exceeds the configured limit");
                }
                changed = true;
                BrowserStateResult::Empty
            }
            BrowserStateOperation::StorageClear { database } => {
                validate_browser_key(&database, 512)?;
                state.databases.insert(database, HashMap::new());
                changed = true;
                BrowserStateResult::Empty
            }
        };

        if changed {
            let path = if user_scoped {
                session
                    .persistent_user_state_path
                    .join(ENCRYPTED_USER_BROWSER_STATE_FILE)
            } else {
                session
                    .persistent_state_path
                    .join(ENCRYPTED_BROWSER_STATE_FILE)
            };
            let key = self.encryption_key().await?;
            tokio::task::spawn_blocking(move || {
                if user_scoped {
                    write_persistent_user_browser_state(&path, &key, user_uuid, &state)
                } else {
                    write_persistent_browser_state(&path, &key, server_uuid, user_uuid, &state)
                }
            })
            .await??;
        }

        Ok(result)
    }

    pub async fn stop(&self, session_uuid: uuid::Uuid, reason: &str) {
        // Drop the write guard before cleanup and before checking for remaining
        // sessions. Keeping the temporary guard alive for the entire `if let`
        // body deadlocks when the code below acquires the read guard.
        let session = {
            let mut sessions = self.inner.sessions.write().await;
            sessions.remove(&session_uuid)
        };
        if let Some(session) = session {
            if let Some(task) = &session.terminal_socket_task {
                task.abort();
            }
            if let Some(task) = &session.addon_tools_socket_task {
                task.abort();
            }
            self.remove_container(&session.container_id).await;
            if let Some(runtime) = session.socket_path.parent() {
                let persisted = match self
                    .persist_encrypted_state(
                        runtime,
                        &session.persistent_state_path,
                        session.server_uuid,
                        session.user_uuid,
                    )
                    .await
                {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::error!(
                            server = %session.server_uuid,
                            session = %session_uuid,
                            user = %session.user_uuid,
                            error = %error,
                            "failed to encrypt Web IDE state; retaining the stopped runtime for recovery"
                        );
                        if let Err(marker_error) = self
                            .write_pending_state_marker(
                                &session.persistent_state_path,
                                session_uuid,
                            )
                            .await
                        {
                            tracing::error!(
                                server = %session.server_uuid,
                                session = %session_uuid,
                                user = %session.user_uuid,
                                error = %marker_error,
                                "failed to record pending Web IDE state recovery"
                            );
                        }
                        false
                    }
                };
                if persisted {
                    let _ = tokio::fs::remove_file(
                        session
                            .persistent_state_path
                            .join(format!("{PENDING_STATE_PREFIX}{session_uuid}")),
                    )
                    .await;
                    let _ = tokio::fs::remove_dir_all(runtime).await;
                }
            }
            tracing::info!(
                server = %session.server_uuid,
                session = %session_uuid,
                user = %session.user_uuid,
                reason,
                "stopped Web IDE session"
            );
            let _ = self
                .report_panel_event(session.server_uuid, session_uuid, "closed", Some(reason))
                .await;
            let has_server_sessions = self
                .inner
                .sessions
                .read()
                .await
                .values()
                .any(|candidate| candidate.server_uuid == session.server_uuid);
            if !has_server_sessions {
                self.inner
                    .collaboration_rooms
                    .write()
                    .await
                    .retain(|(server_uuid, _), _| *server_uuid != session.server_uuid);
            }
        }
    }

    pub async fn collaboration_room(
        &self,
        server: &crate::server::Server,
        encoded_path: &str,
    ) -> Result<Arc<CollaborationRoom>, anyhow::Error> {
        let requested = decode_collaboration_path(encoded_path)?;
        let canonical = server.filesystem.async_canonicalize(&requested).await?;
        let metadata = server.filesystem.async_metadata(&canonical).await?;
        if !metadata.is_file()
            || metadata.len() as usize > self.inner.config.web_ide.max_collaboration_document_bytes
        {
            anyhow::bail!("collaboration supports only bounded regular text files");
        }
        let canonical_key = canonical.to_string_lossy().to_string();
        let key = (server.uuid, canonical_key.clone());
        if let Some(room) = self.inner.collaboration_rooms.read().await.get(&key) {
            if room.oversized.load(Ordering::Acquire) {
                anyhow::bail!("collaboration document exceeded the configured limit");
            }
            return Ok(Arc::clone(room));
        }
        if self.inner.collaboration_rooms.read().await.len()
            >= self.inner.config.web_ide.max_collaboration_rooms
        {
            anyhow::bail!("maximum collaboration room count reached");
        }

        let content = server
            .filesystem
            .async_read_to_string(
                &canonical,
                self.inner.config.web_ide.max_collaboration_document_bytes,
            )
            .await?;
        if content.len() > self.inner.config.web_ide.max_collaboration_document_bytes {
            anyhow::bail!("collaboration document exceeds the configured limit");
        }
        let doc = yrs::Doc::new();
        {
            let text = doc.get_or_insert_text("content");
            let mut transaction = doc.transact_mut();
            text.insert(&mut transaction, 0, &content);
        }

        let awareness: AwarenessRef = Arc::new(RwLock::new(yrs::sync::Awareness::new(doc)));
        let group = Arc::new(BroadcastGroup::new(Arc::clone(&awareness), 128).await);
        let (save_sender, mut save_receiver) = tokio::sync::mpsc::unbounded_channel::<()>();
        let oversized = Arc::new(AtomicBool::new(false));
        let limit_notifier = Arc::new(tokio::sync::Notify::new());
        let oversized_for_observer = Arc::clone(&oversized);
        let limit_notifier_for_observer = Arc::clone(&limit_notifier);
        let max_bytes = self.inner.config.web_ide.max_collaboration_document_bytes;
        let save_subscription = awareness
            .read()
            .await
            .doc()
            .observe_update_v1(move |transaction, _event| {
                let exceeds_limit = transaction
                    .get_text("content")
                    .is_some_and(|text| text.get_string(transaction).len() > max_bytes);
                if exceeds_limit {
                    oversized_for_observer.store(true, Ordering::Release);
                    limit_notifier_for_observer.notify_waiters();
                    return;
                }
                let _ = save_sender.send(());
            })
            .map_err(|error| {
                anyhow::anyhow!("failed to observe collaboration document: {error:?}")
            })?;
        let room = Arc::new(CollaborationRoom {
            group,
            oversized,
            limit_notifier,
            awareness_clients: Arc::new(StdMutex::new(HashMap::new())),
            _save_subscription: save_subscription,
        });

        let server_for_writer = server.clone();
        let path_for_writer = canonical.clone();
        let awareness_for_writer = Arc::clone(&awareness);
        tokio::spawn(async move {
            while save_receiver.recv().await.is_some() {
                tokio::time::sleep(Duration::from_millis(250)).await;
                while save_receiver.try_recv().is_ok() {}
                let content = {
                    let awareness = awareness_for_writer.read().await;
                    let transaction = awareness.doc().transact();
                    transaction
                        .get_text("content")
                        .expect("collaboration rooms always contain a content text")
                        .get_string(&transaction)
                };
                if content.len() > max_bytes {
                    tracing::warn!(server = %server_for_writer.uuid, file = %path_for_writer.display(), "refusing oversized collaborative document write");
                    continue;
                }
                let parent = path_for_writer
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new(""));
                let temporary =
                    parent.join(format!(".jexactyl-collab-{}.tmp", random_url_token(8)));
                let result = async {
                    server_for_writer
                        .filesystem
                        .async_write(&temporary, content.as_bytes())
                        .await?;
                    server_for_writer
                        .filesystem
                        .async_rename(&temporary, &server_for_writer.filesystem, &path_for_writer)
                        .await?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    let _ = server_for_writer
                        .filesystem
                        .async_remove_file(&temporary)
                        .await;
                    tracing::error!(server = %server_for_writer.uuid, file = %path_for_writer.display(), error = %error, "failed to persist collaborative document");
                }
            }
        });

        let mut rooms = self.inner.collaboration_rooms.write().await;
        if let Some(existing) = rooms.get(&key) {
            return Ok(Arc::clone(existing));
        }
        if rooms.len() >= self.inner.config.web_ide.max_collaboration_rooms {
            anyhow::bail!("maximum collaboration room count reached");
        }
        rooms.insert(key, Arc::clone(&room));
        Ok(room)
    }

    pub async fn stop_for_server(
        &self,
        session_uuid: uuid::Uuid,
        server_uuid: uuid::Uuid,
        reason: &str,
    ) {
        let matches = self
            .inner
            .sessions
            .read()
            .await
            .get(&session_uuid)
            .is_some_and(|session| session.server_uuid == server_uuid);
        if matches {
            self.stop(session_uuid, reason).await;
        }
    }

    pub async fn revoke_user(&self, server_uuid: uuid::Uuid, user_uuid: uuid::Uuid, reason: &str) {
        let ids: Vec<_> = self
            .inner
            .sessions
            .read()
            .await
            .values()
            .filter(|session| session.server_uuid == server_uuid && session.user_uuid == user_uuid)
            .map(|session| session.uuid)
            .collect();
        for id in ids {
            self.stop(id, reason).await;
        }
    }

    pub async fn revoke_server(&self, server_uuid: uuid::Uuid, reason: &str) {
        let ids: Vec<_> = self
            .inner
            .sessions
            .read()
            .await
            .values()
            .filter(|session| session.server_uuid == server_uuid)
            .map(|session| session.uuid)
            .collect();
        for id in ids {
            self.stop(id, reason).await;
        }
    }

    /// Remove the persistent user memories for a server that is being
    /// destroyed. This is deliberately only reachable with the server UUID
    /// from Wings' authenticated server lifecycle, never from a customer API.
    pub async fn remove_memory_for_server(&self, server_uuid: uuid::Uuid) {
        self.remove_server_state_directory(
            Path::new(&self.inner.config.web_ide.memory_directory),
            server_uuid,
            "memory",
            || self.validate_memory_directory(),
        )
        .await;
        self.remove_server_state_directory(
            Path::new(&self.inner.config.web_ide.persistent_data_directory),
            server_uuid,
            "persistent user data",
            || self.validate_persistent_data_directory(),
        )
        .await;
    }

    async fn remove_server_state_directory<F>(
        &self,
        root: &Path,
        server_uuid: uuid::Uuid,
        label: &str,
        validate: F,
    ) where
        F: Fn() -> Result<(), anyhow::Error>,
    {
        if let Err(error) = validate() {
            tracing::error!(error = %error, state = label, "refusing Web IDE state cleanup for unsafe directory");
            return;
        }
        let path = root.join(server_uuid.to_string());
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                tracing::error!(server = %server_uuid, state = label, "refusing to remove symlinked Web IDE state directory");
            }
            Ok(metadata) if metadata.is_dir() => {
                if let Err(error) = tokio::fs::remove_dir_all(&path).await {
                    tracing::warn!(server = %server_uuid, state = label, error = %error, "failed to remove Web IDE state directory");
                }
            }
            Ok(_) => {
                tracing::warn!(server = %server_uuid, state = label, "refusing to remove non-directory Web IDE state path");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(server = %server_uuid, state = label, error = %error, "failed to inspect Web IDE state directory");
            }
        }
    }

    pub async fn reconcile_orphans(&self) {
        let mut deferred_runtimes = HashSet::new();
        let filters = HashMap::from([(
            "label".to_string(),
            vec!["com.jexactyl.webide=true".to_string()],
        )]);
        if let Ok(containers) = self
            .inner
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters,
                ..Default::default()
            }))
            .await
        {
            for container in containers {
                let session_uuid = container
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("com.jexactyl.webide.session"))
                    .and_then(|value| uuid::Uuid::parse_str(value).ok());
                let server_uuid = container
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("com.jexactyl.webide.server"))
                    .and_then(|value| uuid::Uuid::parse_str(value).ok());
                let user_uuid = container
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("com.jexactyl.webide.user"))
                    .and_then(|value| uuid::Uuid::parse_str(value).ok());

                // A daemon restart clears the in-memory session map, but the
                // sidecar runtime may still contain the user's plaintext IDE
                // state. Encrypt it before removing the orphaned container so
                // settings, credentials, chat history, extensions, and memory
                // survive the restart. Labels are daemon-authored and bind the
                // archive to the same server/user scope through AES-GCM AAD.
                if let (Some(session_uuid), Some(server_uuid), Some(user_uuid)) =
                    (session_uuid, server_uuid, user_uuid)
                {
                    let (uid, gid) = if self.inner.config.system.user.rootless.enabled {
                        (
                            self.inner.config.system.user.rootless.container_uid,
                            self.inner.config.system.user.rootless.container_gid,
                        )
                    } else {
                        (
                            self.inner.config.system.user.uid,
                            self.inner.config.system.user.gid,
                        )
                    };
                    let runtime = PathBuf::from(&self.inner.config.web_ide.runtime_directory)
                        .join(session_uuid.to_string());
                    let persistent = match self
                        .prepare_persistent_directory(server_uuid, user_uuid, uid, gid)
                        .await
                    {
                        Ok(path) => path,
                        Err(error) => {
                            deferred_runtimes.insert(session_uuid);
                            tracing::error!(
                                server = %server_uuid,
                                session = %session_uuid,
                                user = %user_uuid,
                                error = %error,
                                "refusing to remove orphaned Web IDE container before preparing encrypted state"
                            );
                            continue;
                        }
                    };
                    if let Err(error) = self
                        .persist_encrypted_state(&runtime, &persistent, server_uuid, user_uuid)
                        .await
                    {
                        deferred_runtimes.insert(session_uuid);
                        tracing::error!(
                            server = %server_uuid,
                            session = %session_uuid,
                            user = %user_uuid,
                            error = %error,
                            "failed to encrypt orphaned Web IDE state; orphan cleanup skipped"
                        );
                        continue;
                    }
                    let _ = self
                        .report_panel_event(
                            server_uuid,
                            session_uuid,
                            "closed",
                            Some("daemon_restarted"),
                        )
                        .await;
                } else if let Some(session_uuid) = session_uuid {
                    deferred_runtimes.insert(session_uuid);
                    tracing::error!(
                        session = %session_uuid,
                        "refusing to remove orphaned Web IDE container without complete session labels"
                    );
                }
                let can_remove = session_uuid
                    .is_some_and(|session_uuid| !deferred_runtimes.contains(&session_uuid));
                if can_remove {
                    if let Some(id) = container.id {
                        self.remove_container(&id).await;
                    }
                } else if session_uuid.is_none() {
                    tracing::error!(
                        "refusing to remove orphaned Web IDE container without a session label"
                    );
                }
            }
        }
        if let Err(error) = self.validate_runtime_directory() {
            tracing::error!(error = %error, "refusing unsafe Web IDE runtime directory cleanup");
            return;
        }
        let runtime = PathBuf::from(&self.inner.config.web_ide.runtime_directory);
        if let Ok(mut entries) = tokio::fs::read_dir(&runtime).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry
                    .file_name()
                    .to_str()
                    .and_then(|name| uuid::Uuid::parse_str(name).ok())
                    .is_none()
                {
                    continue;
                }
                if entry
                    .file_name()
                    .to_str()
                    .and_then(|name| uuid::Uuid::parse_str(name).ok())
                    .is_some_and(|session_uuid| deferred_runtimes.contains(&session_uuid))
                {
                    continue;
                }
                let Some(session_uuid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| uuid::Uuid::parse_str(name).ok())
                else {
                    continue;
                };
                if self.has_pending_state(session_uuid).await.unwrap_or(true) {
                    // A previous stop could not encrypt this runtime. Keep it
                    // for the next authenticated session to recover; deleting
                    // it during daemon startup would be irreversible data loss.
                    continue;
                }
                match tokio::fs::symlink_metadata(entry.path()).await {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        let _ = tokio::fs::remove_file(entry.path()).await;
                    }
                    Ok(metadata) if metadata.is_dir() => {
                        let _ = tokio::fs::remove_dir_all(entry.path()).await;
                    }
                    Ok(_) => {
                        let _ = tokio::fs::remove_file(entry.path()).await;
                    }
                    Err(_) => {}
                }
            }
        }
        if tokio::fs::create_dir_all(&runtime).await.is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = tokio::fs::set_permissions(runtime, std::fs::Permissions::from_mode(0o700))
                    .await;
            }
        }
    }

    async fn has_pending_state(&self, session_uuid: uuid::Uuid) -> Result<bool, anyhow::Error> {
        self.validate_persistent_data_directory()?;
        let root = PathBuf::from(&self.inner.config.web_ide.persistent_data_directory);
        let mut servers = tokio::fs::read_dir(&root).await?;
        while let Some(server_entry) = servers.next_entry().await? {
            let server_metadata = tokio::fs::symlink_metadata(server_entry.path()).await?;
            if server_metadata.file_type().is_symlink() || !server_metadata.is_dir() {
                continue;
            }
            let mut users = tokio::fs::read_dir(server_entry.path()).await?;
            while let Some(user_entry) = users.next_entry().await? {
                let user_metadata = tokio::fs::symlink_metadata(user_entry.path()).await?;
                if user_metadata.file_type().is_symlink() || !user_metadata.is_dir() {
                    continue;
                }
                let marker = user_entry
                    .path()
                    .join(format!("{PENDING_STATE_PREFIX}{session_uuid}"));
                if tokio::fs::symlink_metadata(marker).await.is_ok() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    async fn report_panel_event(
        &self,
        server_uuid: uuid::Uuid,
        session_uuid: uuid::Uuid,
        event: &str,
        reason: Option<&str>,
    ) -> PanelEventStatus {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            self.inner.config.client.send_web_ide_session_event(
                server_uuid,
                session_uuid,
                event,
                reason,
            ),
        )
        .await;

        match result {
            Ok(Ok(())) => PanelEventStatus::Accepted,
            Ok(Err(error)) => {
                let status = error
                    .downcast_ref::<reqwest::Error>()
                    .and_then(reqwest::Error::status);
                let rejected = panel_status_is_rejection(status);
                tracing::warn!(
                    server = %server_uuid,
                    session = %session_uuid,
                    event,
                    error = %error,
                    "failed to report Web IDE session state to panel"
                );
                if rejected {
                    PanelEventStatus::Rejected
                } else {
                    PanelEventStatus::Unavailable
                }
            }
            Err(_) => {
                tracing::warn!(
                    server = %server_uuid,
                    session = %session_uuid,
                    event,
                    "timed out reporting Web IDE session state to panel"
                );
                PanelEventStatus::Unavailable
            }
        }
    }

    pub fn start_reaper(&self) {
        let manager = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let candidates: Vec<_> = manager
                    .inner
                    .sessions
                    .read()
                    .await
                    .values()
                    .cloned()
                    .collect();
                let mut authorization_candidates = Vec::new();
                for session in candidates {
                    if (session.cookie_hash.is_none()
                        && session.created_at.elapsed() > Duration::from_secs(120))
                        || session.created_at.elapsed() > session.maximum_lifetime
                        || (session.cookie_hash.is_some()
                            && session.last_interaction.elapsed() > session.presence_timeout)
                        || session.last_interaction.elapsed() > session.idle_timeout
                    {
                        let reason = if session.cookie_hash.is_none() {
                            "launch_expired"
                        } else if session.last_interaction.elapsed() > session.presence_timeout {
                            "browser_presence_expired"
                        } else {
                            "idle_or_lifetime_expired"
                        };
                        manager.stop(session.uuid, reason).await;
                        continue;
                    }
                    let running = manager
                        .inner
                        .docker
                        .inspect_container(&session.container_id, None)
                        .await
                        .ok()
                        .and_then(|container| container.state)
                        .and_then(|state| state.running)
                        .unwrap_or(false);
                    if !running {
                        manager.stop(session.uuid, "sidecar_exited").await;
                        continue;
                    }
                    authorization_candidates.push(session);
                }
                let authorization_results = futures_util::future::join_all(
                    authorization_candidates.iter().map(|session| {
                        let manager = manager.clone();
                        async move {
                            (
                                session.uuid,
                                manager
                                    .report_panel_event(
                                        session.server_uuid,
                                        session.uuid,
                                        if session.last_interaction.elapsed()
                                            <= session.presence_timeout
                                        {
                                            "presence"
                                        } else {
                                            "heartbeat"
                                        },
                                        None,
                                    )
                                    .await,
                            )
                        }
                    }),
                )
                .await;
                for (session_uuid, status) in authorization_results {
                    let stop_reason = match status {
                        PanelEventStatus::Accepted => {
                            if let Some(session) =
                                manager.inner.sessions.write().await.get_mut(&session_uuid)
                            {
                                session.panel_authorization_failures = 0;
                            }
                            None
                        }
                        PanelEventStatus::Rejected => Some("panel_authorization_revoked"),
                        PanelEventStatus::Unavailable => {
                            let failures = {
                                let mut sessions = manager.inner.sessions.write().await;
                                sessions.get_mut(&session_uuid).map(|session| {
                                    session.panel_authorization_failures =
                                        session.panel_authorization_failures.saturating_add(1);
                                    session.panel_authorization_failures
                                })
                            };
                            match failures {
                                Some(failures) if failures >= 3 => {
                                    Some("panel_authorization_unavailable")
                                }
                                Some(failures) => {
                                    tracing::warn!(
                                        session = %session_uuid,
                                        failures,
                                        "retaining Web IDE during transient panel authorization failure"
                                    );
                                    None
                                }
                                None => None,
                            }
                        }
                    };
                    if let Some(reason) = stop_reason {
                        manager.stop(session_uuid, reason).await;
                    }
                }
                manager
                    .inner
                    .consumed_jtis
                    .write()
                    .await
                    .retain(|_, expiry| *expiry > Instant::now());
            }
        });
    }

    async fn remove_container(&self, id: &str) {
        let _ = self
            .inner
            .docker
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
    }

    fn validate_security_configuration(&self) -> Result<(), anyhow::Error> {
        if !self.inner.config.web_ide.enabled {
            anyhow::bail!("web IDE is disabled");
        }
        let public_url = reqwest::Url::parse(&self.inner.config.web_ide.public_url)
            .context("web_ide.public_url is not a valid URL")?;
        if public_url.scheme() != "https"
            || public_url.host_str().is_none()
            || !public_url.username().is_empty()
            || public_url.password().is_some()
            || public_url.query().is_some()
            || public_url.fragment().is_some()
            || !matches!(public_url.path(), "" | "/")
        {
            anyhow::bail!("web_ide.public_url must be an exact HTTPS origin");
        }
        let image = &self.inner.config.web_ide.image;
        let digest = image
            .strip_prefix("sha256:")
            .or_else(|| {
                image
                    .split_once("@sha256:")
                    .filter(|(name, _)| !name.is_empty())
                    .map(|(_, digest)| digest)
            })
            .context("web_ide.image must use an immutable image ID or repository digest")?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("web_ide.image has an invalid sha256 digest");
        }
        if !self
            .inner
            .config
            .web_ide
            .terminal_network_isolation_verified
            || self.inner.config.docker.network.driver != "bridge"
            || self.inner.config.docker.network.mode != self.inner.config.docker.network.name
        {
            anyhow::bail!("Web IDE terminal network isolation is not verified");
        }
        if self.inner.config.web_ide.allow_public_network
            && !self.inner.config.web_ide.public_network_isolation_verified
        {
            anyhow::bail!("Web IDE public egress isolation is not verified");
        }
        let config = &self.inner.config.web_ide;
        if !(256..=8192).contains(&config.memory_mib)
            || !(1..=800).contains(&config.cpu_percent)
            || !(32..=4096).contains(&config.pid_limit)
            || !(32..=2048).contains(&config.tmpfs_mib)
            || !(1..=64 * 1024 * 1024).contains(&config.max_request_bytes)
            || !(1..=8 * 1024 * 1024).contains(&config.max_collaboration_document_bytes)
            || !(1..=2048).contains(&config.max_collaboration_rooms)
            || !(1..=1024).contains(&config.max_sessions)
            || !(1..=config.max_sessions).contains(&config.max_sessions_per_server)
        {
            anyhow::bail!("invalid Web IDE resource or aggregate limits");
        }
        self.validate_runtime_directory()?;
        self.validate_memory_directory()?;
        self.validate_persistent_data_directory()
    }

    async fn prepare_persistent_directory(
        &self,
        server_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
        _uid: u32,
        _gid: u32,
    ) -> Result<PathBuf, anyhow::Error> {
        self.validate_persistent_data_directory()?;
        let root = PathBuf::from(&self.inner.config.web_ide.persistent_data_directory);
        tokio::fs::create_dir_all(&root)
            .await
            .context("failed to create Web IDE persistent data root")?;
        let server_path = root.join(server_uuid.to_string());
        let user_path = persistent_memory_path(&root, server_uuid, user_uuid);
        tokio::fs::create_dir_all(&user_path)
            .await
            .context("failed to create Web IDE encrypted state directory")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for (path, owner_uid, owner_gid) in [
                (root.as_path(), 0, 0),
                (server_path.as_path(), 0, 0),
                (user_path.as_path(), 0, 0),
            ] {
                let metadata = tokio::fs::symlink_metadata(path).await?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    anyhow::bail!("Web IDE persistent data path is not a real directory");
                }
                tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
                std::os::unix::fs::chown(path, Some(owner_uid), Some(owner_gid))?;
            }
        }

        let state_path = user_path.join(ENCRYPTED_STATE_FILE);
        if let Ok(metadata) = tokio::fs::symlink_metadata(&state_path).await {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!("Web IDE encrypted state is not a regular file");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
                    anyhow::bail!("Web IDE encrypted state must be root-owned mode 0600");
                }
            }
        }

        Ok(user_path)
    }

    async fn prepare_user_persistent_directory(
        &self,
        user_uuid: uuid::Uuid,
    ) -> Result<PathBuf, anyhow::Error> {
        self.validate_persistent_data_directory()?;
        let root = PathBuf::from(&self.inner.config.web_ide.persistent_data_directory);
        let users_root = root.join(USER_STATE_DIRECTORY);
        let user_path = users_root.join(user_uuid.to_string());
        tokio::fs::create_dir_all(&user_path)
            .await
            .context("failed to create Web IDE user profile directory")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [root.as_path(), users_root.as_path(), user_path.as_path()] {
                let metadata = tokio::fs::symlink_metadata(path).await?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    anyhow::bail!("Web IDE user profile path is not a real directory");
                }
                tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
                std::os::unix::fs::chown(path, Some(0), Some(0))?;
            }
        }

        for file_name in [
            ENCRYPTED_USER_PROFILE_FILE,
            ENCRYPTED_USER_BROWSER_STATE_FILE,
        ] {
            let path = user_path.join(file_name);
            if let Ok(metadata) = tokio::fs::symlink_metadata(&path).await {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    anyhow::bail!("Web IDE user profile state is not a regular file");
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
                        anyhow::bail!("Web IDE user profile state must be root-owned mode 0600");
                    }
                }
            }
        }

        Ok(user_path)
    }

    async fn restore_user_profile(
        &self,
        persistent_path: &Path,
        settings_path: &Path,
        uid: u32,
        gid: u32,
    ) -> Result<(), anyhow::Error> {
        let key = self.encryption_key().await?;
        let user_uuid = user_profile_scope(persistent_path)?;
        let profile_path = persistent_path.join(ENCRYPTED_USER_PROFILE_FILE);
        let settings_path = settings_path.to_path_buf();
        match tokio::fs::read(&profile_path).await {
            Ok(ciphertext) => {
                let max = self.inner.config.web_ide.max_persistent_state_bytes;
                tokio::task::spawn_blocking(move || {
                    decrypt_user_profile(
                        &ciphertext,
                        &key,
                        user_uuid,
                        &settings_path,
                        max,
                        uid,
                        gid,
                    )
                })
                .await??;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    async fn persist_user_profile(
        &self,
        runtime: &Path,
        persistent_path: &Path,
        user_uuid: uuid::Uuid,
    ) -> Result<(), anyhow::Error> {
        // Multiple servers owned by the same user may stop concurrently.
        // Serialize the read/merge/encrypt replacement so neither stop can
        // discard settings written by the other.
        let _guard = self.inner.user_profile_lock.lock().await;
        let key = self.encryption_key().await?;
        let runtime = runtime.to_path_buf();
        let persistent_path = persistent_path.to_path_buf();
        let max = self.inner.config.web_ide.max_persistent_state_bytes;
        tokio::task::spawn_blocking(move || {
            encrypt_user_profile(&runtime, &persistent_path, &key, user_uuid, max)
        })
        .await??;
        Ok(())
    }

    async fn encryption_key(&self) -> Result<[u8; 32], anyhow::Error> {
        let path = PathBuf::from(&self.inner.config.web_ide.encryption_key_file);
        let metadata = tokio::fs::symlink_metadata(&path).await;
        if matches!(metadata, Err(ref error) if error.kind() == std::io::ErrorKind::NotFound) {
            let mut key = [0u8; 32];
            rand::rng().fill_bytes(&mut key);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let created = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&path)
                }
                #[cfg(not(unix))]
                {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                }
            };
            match created {
                Ok(mut file) => {
                    file.write_all(&key)?;
                    file.sync_all()?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }

        let metadata = tokio::fs::symlink_metadata(&path).await?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != 32 {
            anyhow::bail!("web_ide.encryption_key_file must contain exactly 32 bytes");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
                anyhow::bail!("web_ide.encryption_key_file must be root-owned mode 0600");
            }
        }
        let bytes = tokio::fs::read(path).await?;
        bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("web_ide.encryption_key_file has an invalid length"))
    }

    async fn restore_encrypted_state(
        &self,
        persistent_path: &Path,
        user_data: &Path,
        extensions: &Path,
        memory: &Path,
        home: &Path,
        legacy_memory: &Path,
        uid: u32,
        gid: u32,
    ) -> Result<(), anyhow::Error> {
        let key = self.encryption_key().await?;
        let state_path = persistent_path.join(ENCRYPTED_STATE_FILE);
        let (server_uuid, user_uuid) = persistent_scope(persistent_path)?;
        self.recover_pending_state(persistent_path, server_uuid, user_uuid)
            .await?;
        match tokio::fs::read(&state_path).await {
            Ok(ciphertext) => {
                let max = self.inner.config.web_ide.max_persistent_state_bytes;
                if ciphertext.len() > max.saturating_add(128) {
                    anyhow::bail!("Web IDE encrypted state exceeds the configured limit");
                }
                let user_data = user_data.to_path_buf();
                let extensions = extensions.to_path_buf();
                let memory = memory.to_path_buf();
                let home = home.to_path_buf();
                tokio::task::spawn_blocking(move || {
                    decrypt_state_archive(
                        &ciphertext,
                        &key,
                        server_uuid,
                        user_uuid,
                        &user_data,
                        &extensions,
                        &memory,
                        &home,
                        max,
                        uid,
                        gid,
                    )
                })
                .await??;
                cleanup_legacy_state(
                    persistent_path,
                    &persistent_memory_path(
                        Path::new(&self.inner.config.web_ide.memory_directory),
                        server_uuid,
                        user_uuid,
                    ),
                )
                .await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                migrate_legacy_state(
                    persistent_path,
                    user_data,
                    extensions,
                    memory,
                    home,
                    legacy_memory,
                )
                .await?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    async fn persist_encrypted_state(
        &self,
        runtime: &Path,
        persistent_path: &Path,
        server_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
    ) -> Result<(), anyhow::Error> {
        let persistent_user_state_path = self.prepare_user_persistent_directory(user_uuid).await?;
        self.persist_user_profile(runtime, &persistent_user_state_path, user_uuid)
            .await?;
        let key = self.encryption_key().await?;
        let runtime = runtime.to_path_buf();
        let persistent_path = persistent_path.to_path_buf();
        let max = self.inner.config.web_ide.max_persistent_state_bytes;
        let legacy_memory = persistent_memory_path(
            Path::new(&self.inner.config.web_ide.memory_directory),
            server_uuid,
            user_uuid,
        );
        tokio::task::spawn_blocking(move || {
            encrypt_state_archive(
                &runtime,
                &persistent_path,
                &legacy_memory,
                &key,
                server_uuid,
                user_uuid,
                max,
            )
        })
        .await??;
        Ok(())
    }

    async fn write_pending_state_marker(
        &self,
        persistent_path: &Path,
        session_uuid: uuid::Uuid,
    ) -> Result<(), anyhow::Error> {
        let marker = persistent_path.join(format!("{PENDING_STATE_PREFIX}{session_uuid}"));
        match tokio::fs::symlink_metadata(&marker).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                anyhow::bail!("Web IDE pending state marker is not a regular file");
            }
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .await?;
        file.write_all(session_uuid.to_string().as_bytes()).await?;
        file.flush().await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600)).await?;
            std::os::unix::fs::chown(&marker, Some(0), Some(0))?;
        }
        Ok(())
    }

    async fn recover_pending_state(
        &self,
        persistent_path: &Path,
        server_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
    ) -> Result<(), anyhow::Error> {
        let mut entries = tokio::fs::read_dir(persistent_path).await?;
        let mut pending = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(value) = name.strip_prefix(PENDING_STATE_PREFIX) else {
                continue;
            };
            let Ok(session_uuid) = uuid::Uuid::parse_str(value) else {
                anyhow::bail!("Web IDE pending state marker has an invalid session UUID");
            };
            let metadata = tokio::fs::symlink_metadata(entry.path()).await?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!("Web IDE pending state marker is not a regular file");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
                    anyhow::bail!("Web IDE pending state marker must be root-owned mode 0600");
                }
            }
            let marker_contents = tokio::fs::read_to_string(entry.path()).await?;
            if marker_contents.trim() != session_uuid.to_string() {
                anyhow::bail!("Web IDE pending state marker contents do not match its name");
            }
            pending.push((session_uuid, entry.path()));
        }
        if pending.len() > 1 {
            anyhow::bail!("multiple pending Web IDE state runtimes require operator review");
        }
        let Some((session_uuid, marker)) = pending.pop() else {
            return Ok(());
        };
        let runtime_root = PathBuf::from(&self.inner.config.web_ide.runtime_directory);
        self.validate_runtime_directory()?;
        let runtime = runtime_root.join(session_uuid.to_string());
        let metadata = tokio::fs::symlink_metadata(&runtime).await?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!("pending Web IDE runtime is not a real directory");
        }
        self.persist_encrypted_state(&runtime, persistent_path, server_uuid, user_uuid)
            .await?;
        tokio::fs::remove_dir_all(&runtime).await?;
        tokio::fs::remove_file(marker).await?;
        Ok(())
    }

    async fn seed_security_note(
        &self,
        memory_path: &Path,
        uid: u32,
        gid: u32,
    ) -> Result<(), anyhow::Error> {
        let note = memory_path.join("webide-security.md");
        match tokio::fs::symlink_metadata(&note).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                anyhow::bail!("Web IDE memory seed is not a regular file");
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&note)
                    .await?;
                let security_note: &[u8] = if self.inner.config.web_ide.allow_public_network {
                    b"# Jexactyl Web IDE security decisions\n\n\
Client-side fetch was evaluated, but the built-in web fetch tool delegates to\n\
an internal VS Code workbench fetcher. This deployment cannot guarantee that\n\
requests originate from the user's own browser, so the tool remains disabled.\n\
The temporary public-egress mode allows extension and GitHub traffic through\n\
Docker's isolated default bridge; the node firewall denies node services,\n\
Docker gateways, private/reserved ranges, and cloud metadata addresses.\n\
Browser automation and all MCP servers are disabled as well.\n"
                } else {
                    b"# Jexactyl Web IDE security decisions\n\n\
Client-side fetch was evaluated, but the built-in web fetch tool delegates to\n\
an internal VS Code workbench fetcher. This deployment cannot guarantee that\n\
requests originate from the user's own browser, so the tool is disabled. The\n\
sidecar is deliberately network-isolated and does not relay arbitrary URLs\n\
through the hosting node. Browser automation and all MCP servers are disabled\n\
as well.\n"
                };
                file.write_all(security_note).await?;
                file.flush().await?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    tokio::fs::set_permissions(&note, std::fs::Permissions::from_mode(0o600))
                        .await?;
                    std::os::unix::fs::chown(&note, Some(uid), Some(gid))?;
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn validate_memory_directory(&self) -> Result<(), anyhow::Error> {
        let memory = std::path::Path::new(&self.inner.config.web_ide.memory_directory);
        let component_count = memory
            .components()
            .filter(|component| matches!(component, std::path::Component::Normal(_)))
            .count();
        if !memory.is_absolute()
            || component_count < 3
            || memory.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
        {
            anyhow::bail!("web_ide.memory_directory must be a dedicated absolute path");
        }

        let protected = [
            &self.inner.config.system.data_directory,
            &self.inner.config.system.archive_directory,
            &self.inner.config.system.backup_directory,
            &self.inner.config.system.vmount_directory,
            &self.inner.config.system.log_directory,
            &self.inner.config.system.tmp_directory,
            &self.inner.config.web_ide.runtime_directory,
        ];
        for protected in protected {
            let protected = std::path::Path::new(protected);
            if memory == protected || memory.starts_with(protected) || protected.starts_with(memory)
            {
                anyhow::bail!("web_ide.memory_directory overlaps a protected Wings directory");
            }
        }

        if let Ok(metadata) = std::fs::symlink_metadata(memory)
            && (metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            anyhow::bail!("web_ide.memory_directory must be a real directory, not a symlink");
        }
        Ok(())
    }

    fn validate_persistent_data_directory(&self) -> Result<(), anyhow::Error> {
        let persistent = Path::new(&self.inner.config.web_ide.persistent_data_directory);
        let component_count = persistent
            .components()
            .filter(|component| matches!(component, std::path::Component::Normal(_)))
            .count();
        if !persistent.is_absolute()
            || component_count < 3
            || persistent.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
        {
            anyhow::bail!("web_ide.persistent_data_directory must be a dedicated absolute path");
        }

        let protected = [
            &self.inner.config.system.data_directory,
            &self.inner.config.system.archive_directory,
            &self.inner.config.system.backup_directory,
            &self.inner.config.system.vmount_directory,
            &self.inner.config.system.log_directory,
            &self.inner.config.system.tmp_directory,
            &self.inner.config.web_ide.runtime_directory,
            &self.inner.config.web_ide.memory_directory,
        ];
        for protected in protected {
            let protected = Path::new(protected);
            if persistent == protected
                || persistent.starts_with(protected)
                || protected.starts_with(persistent)
            {
                anyhow::bail!(
                    "web_ide.persistent_data_directory overlaps a protected Wings directory"
                );
            }
        }

        if let Ok(metadata) = std::fs::symlink_metadata(persistent)
            && (metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            anyhow::bail!(
                "web_ide.persistent_data_directory must be a real directory, not a symlink"
            );
        }
        Ok(())
    }

    fn validate_runtime_directory(&self) -> Result<(), anyhow::Error> {
        let runtime = std::path::Path::new(&self.inner.config.web_ide.runtime_directory);
        let component_count = runtime
            .components()
            .filter(|component| matches!(component, std::path::Component::Normal(_)))
            .count();
        if !runtime.is_absolute()
            || component_count < 3
            || runtime.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
        {
            anyhow::bail!("web_ide.runtime_directory must be a dedicated absolute path");
        }

        let protected = [
            &self.inner.config.system.data_directory,
            &self.inner.config.system.archive_directory,
            &self.inner.config.system.backup_directory,
            &self.inner.config.system.vmount_directory,
            &self.inner.config.system.log_directory,
            &self.inner.config.system.tmp_directory,
        ];
        for protected in protected {
            let protected = std::path::Path::new(protected);
            if runtime == protected
                || runtime.starts_with(protected)
                || protected.starts_with(runtime)
            {
                anyhow::bail!("web_ide.runtime_directory overlaps a protected Wings directory");
            }
        }

        if let Ok(metadata) = std::fs::symlink_metadata(runtime)
            && (metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            anyhow::bail!("web_ide.runtime_directory must be a real directory, not a symlink");
        }
        Ok(())
    }
}

async fn write_managed_settings(
    path: &Path,
    managed: &serde_json::Value,
) -> Result<(), anyhow::Error> {
    let mut settings = match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({})),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error.into()),
    };
    if let (Some(existing), Some(managed)) = (settings.as_object_mut(), managed.as_object()) {
        for (key, value) in managed {
            existing.insert(key.clone(), value.clone());
        }
    } else {
        anyhow::bail!("managed Web IDE settings must be a JSON object");
    }

    let temporary = path.with_extension(format!("json.{}", uuid::Uuid::new_v4()));
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(&settings)?).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
    }
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(())
}

fn state_aad(server_uuid: uuid::Uuid, user_uuid: uuid::Uuid) -> Vec<u8> {
    format!("jexactyl-webide-state:v{ENCRYPTED_STATE_VERSION}:{server_uuid}:{user_uuid}")
        .into_bytes()
}

fn browser_state_aad(server_uuid: uuid::Uuid, user_uuid: uuid::Uuid) -> Vec<u8> {
    format!(
        "jexactyl-webide-browser-state:v{ENCRYPTED_BROWSER_STATE_VERSION}:{server_uuid}:{user_uuid}"
    )
    .into_bytes()
}

fn user_profile_aad(user_uuid: uuid::Uuid) -> Vec<u8> {
    format!("jexactyl-webide-user-profile:v{ENCRYPTED_USER_PROFILE_VERSION}:{user_uuid}")
        .into_bytes()
}

fn user_browser_state_aad(user_uuid: uuid::Uuid) -> Vec<u8> {
    format!("jexactyl-webide-user-browser-state:v{ENCRYPTED_BROWSER_STATE_VERSION}:{user_uuid}")
        .into_bytes()
}

fn validate_browser_key(value: &str, maximum: usize) -> Result<(), anyhow::Error> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        anyhow::bail!("browser storage key is invalid");
    }
    Ok(())
}

fn is_global_browser_database_name(database: &str) -> bool {
    database.ends_with("-global")
        || database.ends_with("-global-shared")
        || database.contains("-global-")
}

fn canonical_user_browser_database_name(database: &str) -> &'static str {
    if database.ends_with("-global-shared") || database.contains("-global-shared-") {
        USER_GLOBAL_SHARED_BROWSER_DATABASE
    } else {
        USER_GLOBAL_BROWSER_DATABASE
    }
}

fn browser_theme_name(value: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()
        .and_then(|value| {
            value
                .get("settingsId")
                .or_else(|| value.get("label"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .filter(|theme| valid_user_theme(theme))
}

fn browser_theme_is_non_default(value: &str) -> bool {
    browser_theme_name(value)
        .is_some_and(|theme| !matches!(theme.as_str(), "Default" | "Light 2026"))
}

fn valid_user_theme(theme: &str) -> bool {
    !theme.is_empty() && theme.len() <= 256 && !theme.chars().any(char::is_control)
}

/// Older Web IDE builds accidentally included the server scope in VS Code's
/// global IndexedDB name. Consolidate those records once into two stable
/// user-only buckets. The fullest database supplies the base record set;
/// explicit, non-default appearance choices win over a generated light-theme
/// default so an old session cannot erase a user's chosen theme during the
/// migration.
fn canonicalize_user_browser_databases(state: &mut PersistentBrowserState) -> bool {
    let legacy: Vec<_> = state
        .databases
        .iter()
        .filter(|(database, _)| is_global_browser_database_name(database))
        .map(|(database, values)| (database.clone(), values.clone()))
        .collect();
    if legacy.is_empty() {
        return false;
    }

    let mut changed = false;
    for (shared, canonical) in [
        (false, USER_GLOBAL_BROWSER_DATABASE),
        (true, USER_GLOBAL_SHARED_BROWSER_DATABASE),
    ] {
        let mut candidates: Vec<_> = legacy
            .iter()
            .filter(|(database, _)| {
                let is_shared =
                    database.ends_with("-global-shared") || database.contains("-global-shared-");
                is_shared == shared
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }
        candidates.sort_by(|left, right| {
            left.1
                .len()
                .cmp(&right.1.len())
                .then_with(|| left.0.cmp(&right.0))
        });
        let mut merged = candidates
            .last()
            .map(|(_, values)| (*values).clone())
            .unwrap_or_default();
        for (_, values) in &candidates {
            for (key, value) in values.iter() {
                merged.entry(key.clone()).or_insert_with(|| value.clone());
                if key == "colorThemeData" && browser_theme_is_non_default(value) {
                    merged.insert(key.clone(), value.clone());
                }
            }
        }
        if state.databases.get(canonical) != Some(&merged) {
            state.databases.insert(canonical.to_owned(), merged);
            changed = true;
        }
    }
    for (database, _) in legacy {
        if database != USER_GLOBAL_BROWSER_DATABASE
            && database != USER_GLOBAL_SHARED_BROWSER_DATABASE
        {
            changed |= state.databases.remove(&database).is_some();
        }
    }
    changed
}

fn validate_browser_state_scope(
    user_scoped: bool,
    operation: &BrowserStateOperation,
) -> Result<(), anyhow::Error> {
    match operation {
        BrowserStateOperation::SecretGet { .. }
        | BrowserStateOperation::SecretSet { .. }
        | BrowserStateOperation::SecretDelete { .. }
        | BrowserStateOperation::SecretKeys
            if !user_scoped =>
        {
            anyhow::bail!("browser secrets are only available in the user vault");
        }
        BrowserStateOperation::StorageSnapshot { database }
        | BrowserStateOperation::StorageUpdate { database, .. }
        | BrowserStateOperation::StorageClear { database } => {
            if is_global_browser_database_name(database) != user_scoped {
                anyhow::bail!("browser storage database is outside this scope");
            }
        }
        _ => {}
    }
    Ok(())
}

fn read_persistent_browser_state(
    path: &Path,
    key: &[u8; 32],
    server_uuid: uuid::Uuid,
    user_uuid: uuid::Uuid,
) -> Result<PersistentBrowserState, anyhow::Error> {
    read_persistent_browser_state_with_aad(path, key, &browser_state_aad(server_uuid, user_uuid))
}

fn read_persistent_browser_state_with_aad(
    path: &Path,
    key: &[u8; 32],
    aad: &[u8],
) -> Result<PersistentBrowserState, anyhow::Error> {
    let encrypted = match fs::read(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PersistentBrowserState::default());
        }
        Err(error) => return Err(error.into()),
    };
    let header_len = ENCRYPTED_BROWSER_STATE_MAGIC.len() + 1 + 12;
    if encrypted.len() <= header_len
        || encrypted.len() > MAX_BROWSER_STATE_BYTES.saturating_add(128)
        || &encrypted[..ENCRYPTED_BROWSER_STATE_MAGIC.len()] != ENCRYPTED_BROWSER_STATE_MAGIC
        || encrypted[ENCRYPTED_BROWSER_STATE_MAGIC.len()] != ENCRYPTED_BROWSER_STATE_VERSION
    {
        anyhow::bail!("Web IDE browser state has an unsupported format");
    }
    let nonce =
        aes_gcm::Nonce::from_slice(&encrypted[ENCRYPTED_BROWSER_STATE_MAGIC.len() + 1..header_len]);
    let cipher = Aes256Gcm::new_from_slice(key).expect("AES-256 key length is fixed");
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &encrypted[header_len..],
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("Web IDE browser state authentication failed"))?;
    if plaintext.len() > MAX_BROWSER_STATE_BYTES {
        anyhow::bail!("Web IDE browser state exceeds the configured limit");
    }
    let state: PersistentBrowserState = serde_json::from_slice(&plaintext)?;
    if state.secrets.len() > 512 || state.databases.len() > 128 {
        anyhow::bail!("Web IDE browser state contains too many records");
    }
    for (key, value) in &state.secrets {
        validate_browser_key(key, 1024)?;
        if value.len() > 256 * 1024 {
            anyhow::bail!("Web IDE browser state contains an oversized secret");
        }
    }
    for (database, values) in &state.databases {
        validate_browser_key(database, 512)?;
        if values.len() > 16_384 {
            anyhow::bail!("Web IDE browser state contains too many database records");
        }
        for (key, value) in values {
            validate_browser_key(key, 2048)?;
            if value.len() > 1024 * 1024 {
                anyhow::bail!("Web IDE browser state contains an oversized value");
            }
        }
    }
    Ok(state)
}

fn read_persistent_user_browser_state(
    path: &Path,
    persistent_root: &Path,
    key: &[u8; 32],
    user_uuid: uuid::Uuid,
) -> Result<(PersistentBrowserState, bool), anyhow::Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!("Web IDE user browser state is not a regular file");
            }
            let mut state = read_persistent_browser_state_with_aad(
                path,
                key,
                &user_browser_state_aad(user_uuid),
            )?;
            let changed = canonicalize_user_browser_databases(&mut state);
            return Ok((state, changed));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    // Migrate the global secrets and global UI databases from the previous
    // server/user archives once. Workspace databases are intentionally not
    // copied, so chat history and open files stay attached to their server.
    let mut migrated_state = PersistentBrowserState::default();
    let mut migrated = false;
    let entries = match fs::read_dir(persistent_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((migrated_state, false));
        }
        Err(error) => return Err(error.into()),
    };
    for server_entry in entries {
        let server_entry = server_entry?;
        let server_uuid = match uuid::Uuid::parse_str(&server_entry.file_name().to_string_lossy()) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let server_metadata = fs::symlink_metadata(server_entry.path())?;
        if server_metadata.file_type().is_symlink() || !server_metadata.is_dir() {
            continue;
        }
        let legacy_path = server_entry
            .path()
            .join(user_uuid.to_string())
            .join(ENCRYPTED_BROWSER_STATE_FILE);
        let legacy_metadata = match fs::symlink_metadata(&legacy_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        if legacy_metadata.file_type().is_symlink() || !legacy_metadata.is_file() {
            continue;
        }
        let legacy = match read_persistent_browser_state_with_aad(
            &legacy_path,
            key,
            &browser_state_aad(server_uuid, user_uuid),
        ) {
            Ok(state) => state,
            Err(_) => continue,
        };
        for (secret, value) in legacy.secrets {
            migrated_state.secrets.entry(secret).or_insert(value);
        }
        for (database, values) in legacy.databases {
            if !is_global_browser_database_name(&database) {
                continue;
            }
            migrated_state
                .databases
                .entry(database)
                .or_default()
                .extend(values);
        }
        migrated = true;
    }
    migrated |= canonicalize_user_browser_databases(&mut migrated_state);
    Ok((migrated_state, migrated))
}

fn write_persistent_browser_state(
    path: &Path,
    key: &[u8; 32],
    server_uuid: uuid::Uuid,
    user_uuid: uuid::Uuid,
    state: &PersistentBrowserState,
) -> Result<(), anyhow::Error> {
    write_persistent_browser_state_with_aad(
        path,
        key,
        &browser_state_aad(server_uuid, user_uuid),
        state,
    )
}

fn write_persistent_browser_state_with_aad(
    path: &Path,
    key: &[u8; 32],
    aad: &[u8],
    state: &PersistentBrowserState,
) -> Result<(), anyhow::Error> {
    let plaintext = serde_json::to_vec(state)?;
    if plaintext.len() > MAX_BROWSER_STATE_BYTES {
        anyhow::bail!("Web IDE browser state exceeds the configured limit");
    }
    let cipher = Aes256Gcm::new_from_slice(key).expect("AES-256 key length is fixed");
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            aes_gcm::Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("failed to encrypt Web IDE browser state"))?;
    let mut output = Vec::with_capacity(
        ENCRYPTED_BROWSER_STATE_MAGIC.len() + 1 + nonce_bytes.len() + ciphertext.len(),
    );
    output.extend_from_slice(ENCRYPTED_BROWSER_STATE_MAGIC);
    output.push(ENCRYPTED_BROWSER_STATE_VERSION);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Web IDE browser state path has no parent"))?;
    let temporary = parent.join(format!(
        ".{ENCRYPTED_BROWSER_STATE_FILE}.{}",
        uuid::Uuid::new_v4()
    ));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&output)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, chown};
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
            chown(&temporary, Some(0), Some(0))?;
        }
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn write_persistent_user_browser_state(
    path: &Path,
    key: &[u8; 32],
    user_uuid: uuid::Uuid,
    state: &PersistentBrowserState,
) -> Result<(), anyhow::Error> {
    write_persistent_browser_state_with_aad(path, key, &user_browser_state_aad(user_uuid), state)
}

fn encrypt_user_profile(
    runtime: &Path,
    persistent_path: &Path,
    key: &[u8; 32],
    user_uuid: uuid::Uuid,
    max_bytes: usize,
) -> Result<(), anyhow::Error> {
    let settings_path = runtime.join("user-data/User/settings.json");
    if fs::symlink_metadata(&settings_path).is_ok() {
        sanitize_settings_for_archive(&settings_path)?;
    }
    let current = match fs::read(&settings_path) {
        Ok(bytes) => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .context("Web IDE user profile settings are not valid JSON")?;
            if !value.is_object() {
                anyhow::bail!("Web IDE user profile settings must be a JSON object");
            }
            value
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error.into()),
    };
    let state_path = persistent_path.join(ENCRYPTED_USER_PROFILE_FILE);
    let mut settings = match fs::read(&state_path) {
        Ok(encrypted) => decrypt_user_profile_value(&encrypted, key, user_uuid, max_bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error.into()),
    };
    let settings_object = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Web IDE user profile settings must be a JSON object"))?;
    for (name, value) in current
        .as_object()
        .expect("current Web IDE settings were validated as an object")
    {
        settings_object.insert(name.clone(), value.clone());
    }
    write_encrypted_user_profile_value(persistent_path, key, user_uuid, max_bytes, &settings)
}

fn write_encrypted_user_profile_value(
    persistent_path: &Path,
    key: &[u8; 32],
    user_uuid: uuid::Uuid,
    max_bytes: usize,
    value: &serde_json::Value,
) -> Result<(), anyhow::Error> {
    if !value.is_object() {
        anyhow::bail!("Web IDE user profile settings must be a JSON object");
    }
    let settings = serde_json::to_vec(value)?;
    if settings.len() > max_bytes {
        anyhow::bail!("Web IDE user profile exceeds the configured size limit");
    }
    let cipher = Aes256Gcm::new_from_slice(key).expect("AES-256 key length is fixed");
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            aes_gcm::Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: &settings,
                aad: &user_profile_aad(user_uuid),
            },
        )
        .map_err(|_| anyhow::anyhow!("failed to encrypt Web IDE user profile"))?;
    let mut output = Vec::with_capacity(
        ENCRYPTED_USER_PROFILE_MAGIC.len() + 1 + nonce_bytes.len() + ciphertext.len(),
    );
    output.extend_from_slice(ENCRYPTED_USER_PROFILE_MAGIC);
    output.push(ENCRYPTED_USER_PROFILE_VERSION);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    if output.len() > max_bytes.saturating_add(128) {
        anyhow::bail!("Web IDE encrypted user profile exceeds the configured size limit");
    }
    let state_path = persistent_path.join(ENCRYPTED_USER_PROFILE_FILE);
    let temporary = persistent_path.join(format!(
        ".{ENCRYPTED_USER_PROFILE_FILE}.{}",
        uuid::Uuid::new_v4()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&output)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, chown};
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        chown(&temporary, Some(0), Some(0))?;
    }
    if let Err(error) = fs::rename(&temporary, &state_path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn decrypt_user_profile_value(
    encrypted: &[u8],
    key: &[u8; 32],
    user_uuid: uuid::Uuid,
    max_bytes: usize,
) -> Result<serde_json::Value, anyhow::Error> {
    let header_len = ENCRYPTED_USER_PROFILE_MAGIC.len() + 1 + 12;
    if encrypted.len() <= header_len
        || encrypted.len() > max_bytes.saturating_add(128)
        || &encrypted[..ENCRYPTED_USER_PROFILE_MAGIC.len()] != ENCRYPTED_USER_PROFILE_MAGIC
        || encrypted[ENCRYPTED_USER_PROFILE_MAGIC.len()] != ENCRYPTED_USER_PROFILE_VERSION
    {
        anyhow::bail!("Web IDE encrypted user profile has an unsupported format");
    }
    let nonce =
        aes_gcm::Nonce::from_slice(&encrypted[ENCRYPTED_USER_PROFILE_MAGIC.len() + 1..header_len]);
    let cipher = Aes256Gcm::new_from_slice(key).expect("AES-256 key length is fixed");
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &encrypted[header_len..],
                aad: &user_profile_aad(user_uuid),
            },
        )
        .map_err(|_| anyhow::anyhow!("Web IDE encrypted user profile authentication failed"))?;
    if plaintext.len() > max_bytes {
        anyhow::bail!("Web IDE decrypted user profile exceeds the configured size limit");
    }
    let value: serde_json::Value = serde_json::from_slice(&plaintext)
        .context("Web IDE encrypted user profile settings are not valid JSON")?;
    if !value.is_object() {
        anyhow::bail!("Web IDE encrypted user profile settings must be a JSON object");
    }
    Ok(value)
}

fn decrypt_user_profile(
    encrypted: &[u8],
    key: &[u8; 32],
    user_uuid: uuid::Uuid,
    settings_path: &Path,
    max_bytes: usize,
    uid: u32,
    gid: u32,
) -> Result<(), anyhow::Error> {
    let value = decrypt_user_profile_value(encrypted, key, user_uuid, max_bytes)?;
    let parent = settings_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Web IDE settings path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".settings.json.{}", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(&value)?)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        std::os::unix::fs::chown(&temporary, Some(uid), Some(gid))?;
    }
    if let Err(error) = fs::rename(&temporary, settings_path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn user_profile_scope(path: &Path) -> Result<uuid::Uuid, anyhow::Error> {
    let user_uuid = path
        .file_name()
        .and_then(|value| uuid::Uuid::parse_str(value.to_str()?).ok())
        .ok_or_else(|| anyhow::anyhow!("invalid Web IDE persistent user profile path"))?;
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid Web IDE persistent user profile root"))?;
    if parent != USER_STATE_DIRECTORY {
        anyhow::bail!("Web IDE user profile path is outside the users directory");
    }
    Ok(user_uuid)
}

fn append_tree(
    builder: &mut tar::Builder<Vec<u8>>,
    source: &Path,
    archive_path: &Path,
    total_bytes: &mut usize,
    max_bytes: usize,
) -> Result<(), anyhow::Error> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("Web IDE state contains a non-directory root");
    }
    let mut directory = tar::Header::new_gnu();
    directory.set_entry_type(tar::EntryType::Directory);
    directory.set_mode(0o700);
    directory.set_size(0);
    append_archive_entry(builder, &mut directory, archive_path, std::io::empty())?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let child_archive_path = archive_path.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!("Web IDE state contains a symlink: {}", path.display());
        }
        if metadata.is_dir() {
            append_tree(builder, &path, &child_archive_path, total_bytes, max_bytes)?;
        } else if metadata.is_file() {
            // vscode.lock only coordinates processes within one live
            // code-server runtime. Restoring it into a new sidecar causes VS
            // Code to fork workspaceStorage into `<id>-1`, splitting Copilot
            // responses, chat indexes, open editors, and extension databases.
            if entry.file_name() == "vscode.lock" {
                continue;
            }
            let size = usize::try_from(metadata.len())
                .map_err(|_| anyhow::anyhow!("Web IDE state file is too large"))?;
            *total_bytes = total_bytes
                .checked_add(size)
                .ok_or_else(|| anyhow::anyhow!("Web IDE state size overflow"))?;
            if *total_bytes > max_bytes {
                anyhow::bail!("Web IDE state exceeds the configured size limit");
            }
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(metadata.permissions().mode() & 0o777);
            header.set_size(metadata.len());
            let mut file = fs::File::open(path)?;
            append_archive_entry(builder, &mut header, &child_archive_path, &mut file)?;
        } else {
            anyhow::bail!("Web IDE state contains a special file");
        }
    }
    Ok(())
}

fn remove_workspace_storage_locks(user_data: &Path) -> Result<(), anyhow::Error> {
    let workspace_storage = user_data.join("workspaceStorage");
    let entries = match fs::read_dir(&workspace_storage) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!("Web IDE workspace storage contains a symlink");
        }
        if !metadata.is_dir() {
            continue;
        }
        let lock = entry.path().join("vscode.lock");
        match fs::symlink_metadata(&lock) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                anyhow::bail!("Web IDE workspace lock is not a regular file");
            }
            Ok(_) => fs::remove_file(lock)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn sanitize_settings_for_archive(path: &Path) -> Result<(), anyhow::Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("Web IDE settings state is not a regular file");
    }
    let bytes = fs::read(path)?;
    let mut settings = serde_json::from_slice::<serde_json::Value>(&bytes)
        .context("Web IDE settings state is not valid JSON")?;
    let Some(settings) = settings.as_object_mut() else {
        anyhow::bail!("Web IDE settings state must be a JSON object");
    };
    // Keep harmless user choices such as the selected BYOK provider/model,
    // but never archive credentials or session routing material. Unknown
    // future Jexactyl settings are dropped by default so adding a managed
    // secret cannot accidentally make it durable.
    settings.retain(|key, _| {
        !key.starts_with("jexactyl.webIde.")
            || matches!(
                key.as_str(),
                "jexactyl.webIde.byokProvider" | "jexactyl.webIde.byokModel"
            )
    });
    let temporary = path.with_extension(format!("json.{}", uuid::Uuid::new_v4()));
    let output = serde_json::to_vec_pretty(&settings)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&output)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn append_archive_entry<R: Read>(
    builder: &mut tar::Builder<Vec<u8>>,
    header: &mut tar::Header,
    archive_path: &Path,
    data: R,
) -> Result<(), anyhow::Error> {
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;

    #[cfg(unix)]
    let path_bytes = archive_path.as_os_str().as_bytes();
    #[cfg(not(unix))]
    let path_bytes = archive_path.to_string_lossy().as_bytes();

    // tar's GNU helper has a boundary bug when a long path truncates exactly
    // at a separator. PAX stores the complete path in a separate authenticated
    // header and handles arbitrary VS Code cache/extension names safely.
    if path_bytes.len() > 100 {
        builder.append_pax_extensions([("path", path_bytes)])?;
        header.set_path("jexactyl-state-entry")?;
        header.set_cksum();
        builder.append(header, data)?;
    } else {
        builder.append_data(header, archive_path, data)?;
    }
    Ok(())
}

fn encrypt_state_archive(
    runtime: &Path,
    persistent_path: &Path,
    legacy_memory: &Path,
    key: &[u8; 32],
    server_uuid: uuid::Uuid,
    user_uuid: uuid::Uuid,
    max_bytes: usize,
) -> Result<(), anyhow::Error> {
    let metadata = fs::symlink_metadata(runtime)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("Web IDE runtime state is not a real directory");
    }
    // The endpoint and extension credential are regenerated for every
    // session. Preserve user settings (theme, editor preferences, etc.) but
    // never carry those session-bound values into the encrypted archive.
    sanitize_settings_for_archive(&runtime.join("user-data/User/settings.json"))?;
    let mut archive = tar::Builder::new(Vec::new());
    let mut total_bytes = 0usize;
    for (directory, archive_path) in [
        (
            runtime.join("user-data").join("User"),
            PathBuf::from("user-data/User"),
        ),
        (runtime.join("extensions"), PathBuf::from("extensions")),
        (runtime.join("memory"), PathBuf::from("memory")),
        (runtime.join("home"), PathBuf::from("home")),
    ] {
        append_tree(
            &mut archive,
            &directory,
            &archive_path,
            &mut total_bytes,
            max_bytes,
        )?;
    }
    let plaintext = archive.into_inner()?;
    if plaintext.len() > max_bytes {
        anyhow::bail!("Web IDE state archive exceeds the configured size limit");
    }
    let cipher = Aes256Gcm::new_from_slice(key).expect("AES-256 key length is fixed");
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = aes_gcm::Nonce::from_slice(&nonce_bytes);
    let aad = state_aad(server_uuid, user_uuid);
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("failed to encrypt Web IDE state"))?;

    let state_path = persistent_path.join(ENCRYPTED_STATE_FILE);
    let temporary =
        persistent_path.join(format!(".{ENCRYPTED_STATE_FILE}.{}", uuid::Uuid::new_v4()));
    let mut output = Vec::with_capacity(ENCRYPTED_STATE_MAGIC.len() + 1 + 12 + ciphertext.len());
    output.extend_from_slice(ENCRYPTED_STATE_MAGIC);
    output.push(ENCRYPTED_STATE_VERSION);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    if output.len() > max_bytes.saturating_add(128) {
        anyhow::bail!("Web IDE encrypted state exceeds the configured size limit");
    }
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&output)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, chown};
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
            chown(&temporary, Some(0), Some(0))?;
        }
    }
    if let Err(error) = fs::rename(&temporary, &state_path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }

    // Older releases stored plaintext directories at these paths. Remove them
    // only after the authenticated archive has been durably replaced.
    for path in [
        persistent_path.join("User"),
        persistent_path.join("extensions"),
        persistent_path.join("memory"),
        persistent_path.join("home"),
        legacy_memory.to_path_buf(),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!("refusing to remove symlinked legacy Web IDE state");
            }
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
            Ok(_) => anyhow::bail!("refusing to remove non-directory legacy Web IDE state"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn decrypt_state_archive(
    encrypted: &[u8],
    key: &[u8; 32],
    server_uuid: uuid::Uuid,
    user_uuid: uuid::Uuid,
    user_data: &Path,
    extensions: &Path,
    memory: &Path,
    home: &Path,
    max_bytes: usize,
    uid: u32,
    gid: u32,
) -> Result<(), anyhow::Error> {
    let header_len = ENCRYPTED_STATE_MAGIC.len() + 1 + 12;
    if encrypted.len() <= header_len
        || &encrypted[..ENCRYPTED_STATE_MAGIC.len()] != ENCRYPTED_STATE_MAGIC
        || encrypted[ENCRYPTED_STATE_MAGIC.len()] != ENCRYPTED_STATE_VERSION
    {
        anyhow::bail!("Web IDE encrypted state has an unsupported format");
    }
    let nonce = aes_gcm::Nonce::from_slice(&encrypted[ENCRYPTED_STATE_MAGIC.len() + 1..header_len]);
    let cipher = Aes256Gcm::new_from_slice(key).expect("AES-256 key length is fixed");
    let aad = state_aad(server_uuid, user_uuid);
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &encrypted[header_len..],
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("Web IDE encrypted state authentication failed"))?;
    if plaintext.len() > max_bytes {
        anyhow::bail!("Web IDE decrypted state exceeds the configured size limit");
    }
    extract_state_archive(
        &plaintext, user_data, extensions, memory, home, max_bytes, uid, gid,
    )
}

fn extract_state_archive(
    archive_bytes: &[u8],
    user_data: &Path,
    extensions: &Path,
    memory: &Path,
    home: &Path,
    max_bytes: usize,
    uid: u32,
    gid: u32,
) -> Result<(), anyhow::Error> {
    let mut archive = tar::Archive::new(std::io::Cursor::new(archive_bytes));
    let mut total_bytes = 0usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();
        let components: Vec<_> = entry_path.components().collect();
        if components.is_empty()
            || components
                .iter()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            anyhow::bail!("Web IDE encrypted state contains an unsafe path");
        }
        let (base, prefix_len) = if components.len() >= 2
            && components[0].as_os_str() == "user-data"
            && components[1].as_os_str() == "User"
        {
            (user_data, 2)
        } else if components[0].as_os_str() == "extensions" {
            (extensions, 1)
        } else if components[0].as_os_str() == "memory" {
            (memory, 1)
        } else if components[0].as_os_str() == "home" {
            (home, 1)
        } else {
            anyhow::bail!("Web IDE encrypted state contains an unknown root");
        };
        let relative =
            components[prefix_len..]
                .iter()
                .fold(PathBuf::new(), |mut path, component| {
                    path.push(component.as_os_str());
                    path
                });
        let target = if relative.as_os_str().is_empty() {
            base.to_path_buf()
        } else {
            base.join(relative)
        };
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if !entry_type.is_file() {
            anyhow::bail!("Web IDE encrypted state contains a link or special file");
        }
        let size = usize::try_from(entry.size())
            .map_err(|_| anyhow::anyhow!("Web IDE state file is too large"))?;
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("Web IDE state size overflow"))?;
        if total_bytes > max_bytes {
            anyhow::bail!("Web IDE encrypted state exceeds the configured size limit");
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Ok(metadata) = fs::symlink_metadata(&target)
            && metadata.file_type().is_symlink()
        {
            anyhow::bail!("Web IDE encrypted state would overwrite a symlink");
        }
        let mut content = Vec::with_capacity(size);
        entry.read_to_end(&mut content)?;
        if content.len() != size {
            anyhow::bail!("Web IDE encrypted state entry size mismatch");
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&target)?;
        file.write_all(&content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = entry.header().mode().unwrap_or(0o600) & 0o777;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode.max(0o600)))?;
        }
    }
    for path in [user_data, extensions, memory, home] {
        if path.exists() {
            chown_tree(path, uid, gid)?;
        }
    }
    Ok(())
}

fn chown_tree(path: &Path, uid: u32, gid: u32) -> Result<(), anyhow::Error> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("Web IDE state contains a symlink");
    }
    #[cfg(unix)]
    std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

async fn migrate_legacy_state(
    persistent_path: &Path,
    user_data: &Path,
    extensions: &Path,
    memory: &Path,
    home: &Path,
    legacy_memory: &Path,
) -> Result<(), anyhow::Error> {
    for (source, destination) in [
        (persistent_path.join("User"), user_data.to_path_buf()),
        (persistent_path.join("extensions"), extensions.to_path_buf()),
        (persistent_path.join("memory"), memory.to_path_buf()),
        (persistent_path.join("home"), home.to_path_buf()),
        (legacy_memory.to_path_buf(), memory.to_path_buf()),
    ] {
        let metadata = match tokio::fs::symlink_metadata(&source).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!("refusing to migrate unsafe legacy Web IDE state");
        }
        if tokio::fs::try_exists(&destination).await? {
            anyhow::bail!("duplicate legacy Web IDE state roots");
        }
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::rename(source, destination).await?;
    }
    Ok(())
}

async fn cleanup_legacy_state(
    persistent_path: &Path,
    legacy_memory: &Path,
) -> Result<(), anyhow::Error> {
    for path in [
        persistent_path.join("User"),
        persistent_path.join("extensions"),
        persistent_path.join("memory"),
        persistent_path.join("home"),
        legacy_memory.to_path_buf(),
    ] {
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!("refusing to remove symlinked legacy Web IDE state");
            }
            Ok(metadata) if metadata.is_dir() => tokio::fs::remove_dir_all(path).await?,
            Ok(_) => anyhow::bail!("refusing to remove non-directory legacy Web IDE state"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn persistent_scope(path: &Path) -> Result<(uuid::Uuid, uuid::Uuid), anyhow::Error> {
    let server_uuid = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| uuid::Uuid::parse_str(value.to_str()?).ok())
        .ok_or_else(|| anyhow::anyhow!("invalid Web IDE persistent server path"))?;
    let user_uuid = path
        .file_name()
        .and_then(|value| uuid::Uuid::parse_str(value.to_str()?).ok())
        .ok_or_else(|| anyhow::anyhow!("invalid Web IDE persistent user path"))?;
    Ok((server_uuid, user_uuid))
}

fn persistent_memory_path(root: &Path, server_uuid: uuid::Uuid, user_uuid: uuid::Uuid) -> PathBuf {
    root.join(server_uuid.to_string())
        .join(user_uuid.to_string())
}

fn random_url_token(bytes: usize) -> String {
    let mut value = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut value);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value)
}

fn decode_collaboration_path(encoded: &str) -> Result<PathBuf, anyhow::Error> {
    if encoded.len() > 2048 {
        anyhow::bail!("collaboration room path is too long");
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .context("invalid collaboration room path")?;
    let requested = std::str::from_utf8(&raw).context("collaboration path is not UTF-8")?;
    let requested = std::path::Path::new(requested);
    if requested.as_os_str().is_empty()
        || requested.is_absolute()
        || requested
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        anyhow::bail!("invalid collaboration file path");
    }
    Ok(requested.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_authorization_statuses_fail_closed_without_killing_on_transient_errors() {
        assert!(panel_status_is_rejection(Some(
            reqwest::StatusCode::FORBIDDEN
        )));
        assert!(panel_status_is_rejection(Some(
            reqwest::StatusCode::NOT_FOUND
        )));
        assert!(!panel_status_is_rejection(Some(
            reqwest::StatusCode::REQUEST_TIMEOUT
        )));
        assert!(!panel_status_is_rejection(Some(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        )));
        assert!(!panel_status_is_rejection(Some(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        )));
        assert!(!panel_status_is_rejection(None));
    }

    fn room(path: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(path)
    }

    #[test]
    fn collaboration_path_accepts_only_relative_normal_components() {
        assert_eq!(
            decode_collaboration_path(&room(b"src/main.rs")).unwrap(),
            PathBuf::from("src/main.rs")
        );
        for rejected in [
            b"".as_slice(),
            b"../secret",
            b"src/../secret",
            b"/etc/passwd",
            b"./file",
        ] {
            assert!(decode_collaboration_path(&room(rejected)).is_err());
        }
        assert!(decode_collaboration_path("not!base64").is_err());
        assert!(decode_collaboration_path(&room(&[0xff, 0xfe])).is_err());
    }

    #[test]
    fn persistent_memory_path_is_scoped_to_both_server_and_user() {
        let root = Path::new("/var/lib/pterodactyl/webide-memory");
        let server_a = uuid::Uuid::from_u128(1);
        let server_b = uuid::Uuid::from_u128(2);
        let user_a = uuid::Uuid::from_u128(3);
        let user_b = uuid::Uuid::from_u128(4);
        assert_ne!(
            persistent_memory_path(root, server_a, user_a),
            persistent_memory_path(root, server_a, user_b)
        );
        assert_ne!(
            persistent_memory_path(root, server_a, user_a),
            persistent_memory_path(root, server_b, user_a)
        );
        assert!(persistent_memory_path(root, server_a, user_a).starts_with(root));
    }

    #[test]
    fn encrypted_state_round_trip_is_authenticated_and_scoped() {
        let root =
            std::env::temp_dir().join(format!("jexactyl-webide-test-{}", uuid::Uuid::new_v4()));
        let runtime = root.join("runtime");
        let persistent = root.join("persistent");
        let restored = root.join("restored");
        for path in [
            runtime.join("user-data/User/globalStorage"),
            runtime.join("user-data/User/workspaceStorage/5bf2cc5e"),
            runtime.join("extensions/example"),
            runtime.join("memory"),
            runtime.join("home"),
            persistent.clone(),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(
            runtime.join("user-data/User/globalStorage/state.json"),
            br#"{"theme":"light","github":"secret"}"#,
        )
        .unwrap();
        fs::write(
            runtime.join("user-data/User/workspaceStorage/5bf2cc5e/vscode.lock"),
            b"stale-process-lock",
        )
        .unwrap();
        fs::write(
            runtime.join("user-data/User/settings.json"),
            br#"{"workbench.colorTheme":"Jexactyl Light","jexactyl.webIde.endpoint":"https://node.invalid/session","jexactyl.webIde.extensionToken":"session-secret","jexactyl.webIde.byokProvider":"openai","jexactyl.webIde.byokModel":"gpt-4.1","jexactyl.webIde.futureSecret":"must-not-persist"}"#,
        )
        .unwrap();
        fs::write(
            runtime.join("extensions/example/package.json"),
            b"extension",
        )
        .unwrap();
        fs::write(runtime.join("memory/notes.md"), b"memory").unwrap();
        fs::write(runtime.join("home/.gitconfig"), b"[user]\nname = test\n").unwrap();
        let long_name = "l".repeat(180);
        let long_path = runtime
            .join("user-data/User/globalStorage/CachedConfigurations/defaults")
            .join(&long_name);
        fs::create_dir_all(long_path.parent().unwrap()).unwrap();
        fs::write(&long_path, b"long-path-state").unwrap();
        let server = uuid::Uuid::from_u128(10);
        let user = uuid::Uuid::from_u128(20);
        let key = [7u8; 32];
        encrypt_state_archive(
            &runtime,
            &persistent,
            &root.join("legacy-memory"),
            &key,
            server,
            user,
            1024 * 1024,
        )
        .unwrap();
        let encrypted = fs::read(persistent.join(ENCRYPTED_STATE_FILE)).unwrap();
        assert!(encrypted.starts_with(ENCRYPTED_STATE_MAGIC));
        assert!(!encrypted.windows(5).any(|window| window == b"theme"));
        assert!(
            !encrypted
                .windows(14)
                .any(|window| window == b"session-secret")
        );
        assert!(
            !encrypted
                .windows(12)
                .any(|window| window == b"futureSecret")
        );
        decrypt_state_archive(
            &encrypted,
            &key,
            server,
            user,
            &restored.join("user-data/User"),
            &restored.join("extensions"),
            &restored.join("memory"),
            &restored.join("home"),
            1024 * 1024,
            0,
            0,
        )
        .unwrap();
        assert_eq!(
            fs::read(restored.join("user-data/User/globalStorage/state.json")).unwrap(),
            br#"{"theme":"light","github":"secret"}"#
        );
        assert_eq!(
            fs::read(restored.join("extensions/example/package.json")).unwrap(),
            b"extension"
        );
        assert!(
            !restored
                .join("user-data/User/workspaceStorage/5bf2cc5e/vscode.lock")
                .exists()
        );
        let restored_settings: serde_json::Value = serde_json::from_slice(
            &fs::read(restored.join("user-data/User/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(restored_settings["workbench.colorTheme"], "Jexactyl Light");
        assert!(restored_settings.get("jexactyl.webIde.endpoint").is_none());
        assert!(
            restored_settings
                .get("jexactyl.webIde.extensionToken")
                .is_none()
        );
        assert_eq!(restored_settings["jexactyl.webIde.byokProvider"], "openai");
        assert_eq!(restored_settings["jexactyl.webIde.byokModel"], "gpt-4.1");
        assert!(
            restored_settings
                .get("jexactyl.webIde.futureSecret")
                .is_none()
        );
        assert_eq!(
            fs::read(
                restored
                    .join("user-data/User/globalStorage/CachedConfigurations/defaults")
                    .join(long_name)
            )
            .unwrap(),
            b"long-path-state"
        );
        let mut tampered = encrypted;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(
            decrypt_state_archive(
                &tampered,
                &key,
                server,
                user,
                &root.join("tampered/user-data/User"),
                &root.join("tampered/extensions"),
                &root.join("tampered/memory"),
                &root.join("tampered/home"),
                1024 * 1024,
                0,
                0,
            )
            .is_err()
        );
        assert!(
            decrypt_state_archive(
                &fs::read(persistent.join(ENCRYPTED_STATE_FILE)).unwrap(),
                &key,
                server,
                uuid::Uuid::from_u128(21),
                &root.join("wrong-user/user-data/User"),
                &root.join("wrong-user/extensions"),
                &root.join("wrong-user/memory"),
                &root.join("wrong-user/home"),
                1024 * 1024,
                0,
                0,
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn encrypted_browser_state_round_trip_is_authenticated_and_scoped() {
        let root = std::env::temp_dir().join(format!(
            "jexactyl-webide-browser-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(ENCRYPTED_BROWSER_STATE_FILE);
        let key = [23u8; 32];
        let server = uuid::Uuid::from_u128(30);
        let user = uuid::Uuid::from_u128(40);
        let mut state = PersistentBrowserState::default();
        state.secrets.insert(
            "github.authentication".to_owned(),
            "oauth-secret".to_owned(),
        );
        state.databases.insert(
            "vscode-web-state-db-global".to_owned(),
            HashMap::from([(
                "chat.ChatSessionStore.index".to_owned(),
                r#"[{"sessionId":"one"}]"#.to_owned(),
            )]),
        );

        write_persistent_browser_state(&path, &key, server, user, &state).unwrap();
        let encrypted = fs::read(&path).unwrap();
        assert!(encrypted.starts_with(ENCRYPTED_BROWSER_STATE_MAGIC));
        assert!(
            !encrypted
                .windows(12)
                .any(|window| window == b"oauth-secret")
        );
        assert_eq!(
            read_persistent_browser_state(&path, &key, server, user).unwrap(),
            state
        );
        assert!(
            read_persistent_browser_state(&path, &key, server, uuid::Uuid::from_u128(41)).is_err()
        );

        let mut tampered = encrypted;
        *tampered.last_mut().unwrap() ^= 1;
        fs::write(&path, tampered).unwrap();
        assert!(read_persistent_browser_state(&path, &key, server, user).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn encrypted_user_profile_is_portable_but_user_scoped() {
        let root = std::env::temp_dir().join(format!(
            "jexactyl-webide-user-profile-test-{}",
            uuid::Uuid::new_v4()
        ));
        let runtime = root.join("runtime");
        let settings = runtime.join("user-data/User/settings.json");
        let persistent = root.join("users/one");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::create_dir_all(&persistent).unwrap();
        fs::write(
            &settings,
            br#"{"workbench.colorTheme":"Jexactyl Dark","jexactyl.webIde.endpoint":"https://stale","editor.fontSize":14}"#,
        )
        .unwrap();
        let key = [71u8; 32];
        let user = uuid::Uuid::from_u128(51);
        encrypt_user_profile(&runtime, &persistent, &key, user, 1024 * 1024).unwrap();
        let encrypted = fs::read(persistent.join(ENCRYPTED_USER_PROFILE_FILE)).unwrap();
        assert!(encrypted.starts_with(ENCRYPTED_USER_PROFILE_MAGIC));
        assert!(!encrypted.windows(6).any(|window| window == b"stale"));

        let restored = root.join("restored/user-data/User/settings.json");
        decrypt_user_profile(&encrypted, &key, user, &restored, 1024 * 1024, 0, 0).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&restored).unwrap()).unwrap();
        assert_eq!(value["workbench.colorTheme"], "Jexactyl Dark");
        assert!(value.get("jexactyl.webIde.endpoint").is_none());

        // A second server that never changed the theme must not erase the
        // explicit choice made by the first server when it stops later.
        fs::write(
            &settings,
            br#"{"editor.fontSize":16,"jexactyl.webIde.endpoint":"https://new-session"}"#,
        )
        .unwrap();
        encrypt_user_profile(&runtime, &persistent, &key, user, 1024 * 1024).unwrap();
        let merged = decrypt_user_profile_value(
            &fs::read(persistent.join(ENCRYPTED_USER_PROFILE_FILE)).unwrap(),
            &key,
            user,
            1024 * 1024,
        )
        .unwrap();
        assert_eq!(merged["workbench.colorTheme"], "Jexactyl Dark");
        assert_eq!(merged["editor.fontSize"], 16);
        assert!(merged.get("jexactyl.webIde.endpoint").is_none());
        assert!(
            decrypt_user_profile(
                &encrypted,
                &key,
                uuid::Uuid::from_u128(52),
                &root.join("wrong-user/settings.json"),
                1024 * 1024,
                0,
                0,
            )
            .is_err()
        );
        let mut tampered = encrypted;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(
            decrypt_user_profile(
                &tampered,
                &key,
                user,
                &root.join("tampered/settings.json"),
                1024 * 1024,
                0,
                0,
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_global_browser_state_migrates_without_workspace_records() {
        let root = std::env::temp_dir().join(format!(
            "jexactyl-webide-browser-migration-test-{}",
            uuid::Uuid::new_v4()
        ));
        let server = uuid::Uuid::from_u128(61);
        let user = uuid::Uuid::from_u128(62);
        let legacy = root
            .join(server.to_string())
            .join(user.to_string())
            .join(ENCRYPTED_BROWSER_STATE_FILE);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let key = [73u8; 32];
        let mut state = PersistentBrowserState::default();
        state.secrets.insert("github.auth".into(), "token".into());
        state
            .databases
            .insert("vscode-web-state-db-global".into(), HashMap::new());
        state
            .databases
            .insert("vscode-web-state-db-workspace".into(), HashMap::new());
        write_persistent_browser_state(&legacy, &key, server, user, &state).unwrap();
        let target = root
            .join(USER_STATE_DIRECTORY)
            .join(user.to_string())
            .join(ENCRYPTED_USER_BROWSER_STATE_FILE);
        let (migrated, changed) =
            read_persistent_user_browser_state(&target, &root, &key, user).unwrap();
        assert!(changed);
        assert_eq!(
            migrated.secrets.get("github.auth").map(String::as_str),
            Some("token")
        );
        assert!(
            migrated
                .databases
                .contains_key(USER_GLOBAL_BROWSER_DATABASE)
        );
        assert!(
            !migrated
                .databases
                .contains_key("vscode-web-state-db-workspace")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn split_user_global_databases_collapse_and_keep_explicit_theme() {
        let light = r#"{"settingsId":"Light 2026","label":"Light 2026"}"#.to_owned();
        let dark = r#"{"settingsId":"Dark 2026","label":"Dark 2026"}"#.to_owned();
        let mut state = PersistentBrowserState::default();
        state.databases.insert(
            "vscode-web-state-db-user-global".into(),
            HashMap::from([
                ("colorThemeData".into(), light),
                ("GitHub.copilot-chat".into(), "new-account-state".into()),
            ]),
        );
        state.databases.insert(
            "vscode-web-state-db-old-server-global".into(),
            HashMap::from([("colorThemeData".into(), dark.clone())]),
        );

        assert!(canonicalize_user_browser_databases(&mut state));
        assert_eq!(state.databases.len(), 1);
        let global = &state.databases[USER_GLOBAL_BROWSER_DATABASE];
        assert_eq!(global["colorThemeData"], dark);
        assert_eq!(global["GitHub.copilot-chat"], "new-account-state");
        assert!(!canonicalize_user_browser_databases(&mut state));
    }

    #[test]
    fn browser_state_scope_rejects_cross_server_chat_database_access() {
        let workspace = BrowserStateOperation::StorageSnapshot {
            database: "vscode-web-state-db-server-scope-workspace".into(),
        };
        let global = BrowserStateOperation::StorageSnapshot {
            database: "vscode-web-state-db-user-scope-global".into(),
        };
        let secret = BrowserStateOperation::SecretKeys;
        assert!(validate_browser_state_scope(false, &workspace).is_ok());
        assert!(validate_browser_state_scope(false, &global).is_err());
        assert!(validate_browser_state_scope(true, &global).is_ok());
        assert!(validate_browser_state_scope(true, &workspace).is_err());
        assert!(validate_browser_state_scope(false, &secret).is_err());
        assert!(validate_browser_state_scope(true, &secret).is_ok());
    }
}
