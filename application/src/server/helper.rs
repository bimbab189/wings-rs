use anyhow::Context;
use futures_util::StreamExt;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use utoipa::ToSchema;

const MANIFEST_PATH: &str = "/.jexactyl/helper-agent.json";
const DEFAULT_TIMEOUT_MS: u64 = 1500;
const DEFAULT_API_BASE_PATH: &str = "/jexactyl-helper/v1";
const HOST_HELPER_PLUGIN_PATH: &str = "/etc/pterodactyl/helper_plugin.jar";
const HOST_HELPER_FABRIC_PATH: &str = "/etc/pterodactyl/helper_fabric.jar";
const HOST_HELPER_LEGACY_FABRIC_PATH: &str = "/etc/pterodactyl/helper_legacyfabric.jar";
const HOST_HELPER_FORGE_PATH: &str = "/etc/pterodactyl/helper_forge.jar";
pub const WEB_HOSTING_MAX_COMMAND_OUTPUT_BYTES: usize = 96 * 1024;
pub const WEB_HOSTING_DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 120;
pub const WEB_HOSTING_MAX_COMMAND_TIMEOUT_SECONDS: u64 = 600;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HelperAgentManifest {
    #[serde(default)]
    pub version: u8,
    pub variant: String,
    pub port: u16,
    pub token: String,
    #[serde(default = "default_api_base_path")]
    pub api_base_path: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub capabilities: Value,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HelperStatus {
    pub connected: bool,
    pub variant: String,
    pub port: u16,
    pub api_base_path: String,
    #[schema(value_type = Object)]
    pub capabilities: Value,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HelperInstallResponse {
    pub variant: String,
    pub source_path: String,
    pub target_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerCommandOutput {
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub truncated: bool,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

fn default_api_base_path() -> String {
    DEFAULT_API_BASE_PATH.to_string()
}

pub async fn read_manifest(
    server: &crate::server::Server,
) -> Result<HelperAgentManifest, anyhow::Error> {
    let contents = server
        .filesystem
        .async_read_to_string(MANIFEST_PATH, 64 * 1024)
        .await
        .context("failed to read helper manifest")?;

    let manifest: HelperAgentManifest =
        serde_json::from_str(&contents).context("failed to decode helper manifest")?;

    Ok(manifest)
}

pub async fn resolve_container_ip(server: &crate::server::Server) -> Result<String, anyhow::Error> {
    let docker_id = server
        .docker_container_id()
        .await
        .context("server container is not attached")?;

    let details = server
        .app_state
        .docker
        .inspect_container(&docker_id, None)
        .await
        .context("failed to inspect container")?;

    let preferred_network = server.app_state.config.load().docker.network.name.clone();

    if let Some(networks) = details
        .network_settings
        .and_then(|settings| settings.networks)
    {
        if let Some(network) = networks.get(&preferred_network)
            && let Some(ip) = &network.ip_address
            && !ip.is_empty()
        {
            return Ok(ip.clone());
        }

        for network in networks.values() {
            if let Some(ip) = &network.ip_address
                && !ip.is_empty()
            {
                return Ok(ip.clone());
            }
        }
    }

    Err(anyhow::anyhow!("container IP address is unavailable"))
}

pub async fn exec_container_shell_command(
    server: &crate::server::Server,
    command: &str,
    working_dir: &str,
    timeout: std::time::Duration,
    max_output_bytes: usize,
) -> Result<ContainerCommandOutput, anyhow::Error> {
    exec_container_shell_command_with_env(
        server,
        command,
        working_dir,
        timeout,
        max_output_bytes,
        &[],
    )
    .await
}

pub async fn exec_container_shell_command_with_env(
    server: &crate::server::Server,
    command: &str,
    working_dir: &str,
    timeout: std::time::Duration,
    max_output_bytes: usize,
    extra_env: &[String],
) -> Result<ContainerCommandOutput, anyhow::Error> {
    if server.state.get_state() == crate::server::state::ServerState::Offline {
        return Err(anyhow::anyhow!("server is offline"));
    }

    if command.trim().is_empty() {
        return Err(anyhow::anyhow!("command cannot be empty"));
    }

    let docker_id = server
        .docker_container_id()
        .await
        .context("server container is not attached")?;

    let mut environment = vec!["HOME=/home/container".to_string()];
    environment.extend(extra_env.iter().cloned());
    let environment = environment.iter().map(String::as_str).collect::<Vec<_>>();

    let exec = server
        .app_state
        .docker
        .create_exec(
            &docker_id,
            bollard::exec::CreateExecOptions {
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(vec!["sh", "-lc", command]),
                working_dir: Some(working_dir),
                env: Some(environment),
                ..Default::default()
            },
        )
        .await
        .context("failed to create container exec")?;

    let started = server
        .app_state
        .docker
        .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
        .await
        .context("failed to start container exec")?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut truncated = false;
    let mut timed_out = false;

    if let bollard::exec::StartExecResults::Attached { mut output, .. } = started {
        let read_output = async {
            while let Some(item) = output.next().await {
                match item.context("failed to read container exec output")? {
                    bollard::container::LogOutput::StdOut { message }
                    | bollard::container::LogOutput::Console { message } => {
                        append_limited(&mut stdout, &message, max_output_bytes, &mut truncated);
                    }
                    bollard::container::LogOutput::StdErr { message } => {
                        append_limited(&mut stderr, &message, max_output_bytes, &mut truncated);
                    }
                    _ => {}
                }
            }

            Ok::<(), anyhow::Error>(())
        };

        match tokio::time::timeout(timeout, read_output).await {
            Ok(result) => result?,
            Err(_) => timed_out = true,
        }
    }

    let inspect = server
        .app_state
        .docker
        .inspect_exec(&exec.id)
        .await
        .context("failed to inspect container exec")?;

    Ok(ContainerCommandOutput {
        exit_code: inspect.exit_code,
        stdout: String::from_utf8_lossy(&stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
        timed_out,
        truncated,
    })
}

pub async fn is_web_hosting_server(server: &crate::server::Server) -> bool {
    let configuration = server.configuration.read().await;

    if configuration
        .labels
        .get("com.jexactyl.webhosting")
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
    {
        return true;
    }

    let image = configuration
        .container
        .image
        .to_string()
        .trim_end_matches('~')
        .to_ascii_lowercase();

    image.starts_with("jexactyl/webhosting-php:") || image.contains("/webhosting-php:")
}

pub fn web_hosting_working_directory(raw: Option<&str>) -> Result<String, anyhow::Error> {
    let raw = raw.unwrap_or("/").trim();
    let relative = raw
        .strip_prefix("/home/container")
        .unwrap_or(raw)
        .trim_start_matches('/');
    let path = safe_container_path(std::path::Path::new("/home/container"), relative)?;

    Ok(path.to_string_lossy().to_string())
}

pub async fn exec_web_hosting_user_command(
    server: &crate::server::Server,
    command: &str,
    working_dir: Option<&str>,
    timeout_seconds: Option<u64>,
) -> Result<ContainerCommandOutput, anyhow::Error> {
    exec_web_hosting_user_command_with_env(server, command, working_dir, timeout_seconds, &[]).await
}

pub async fn exec_web_hosting_user_command_with_env(
    server: &crate::server::Server,
    command: &str,
    working_dir: Option<&str>,
    timeout_seconds: Option<u64>,
    extra_env: &[String],
) -> Result<ContainerCommandOutput, anyhow::Error> {
    validate_web_hosting_user_command(command)?;

    let working_dir = web_hosting_working_directory(working_dir)?;
    let timeout_seconds = timeout_seconds
        .unwrap_or(WEB_HOSTING_DEFAULT_COMMAND_TIMEOUT_SECONDS)
        .clamp(1, WEB_HOSTING_MAX_COMMAND_TIMEOUT_SECONDS);
    let bounded_command = format!(
        "timeout -s TERM {}s sh -lc {}",
        timeout_seconds,
        shell_single_quote(command)
    );
    let mut output = exec_container_shell_command_with_env(
        server,
        &bounded_command,
        &working_dir,
        std::time::Duration::from_secs(timeout_seconds.saturating_add(5)),
        WEB_HOSTING_MAX_COMMAND_OUTPUT_BYTES,
        extra_env,
    )
    .await?;

    if output.exit_code == Some(124) {
        output.timed_out = true;
    }

    Ok(output)
}

pub fn validate_web_hosting_user_command(command: &str) -> Result<(), anyhow::Error> {
    let command = command.trim();

    if command.is_empty() {
        return Err(anyhow::anyhow!("command cannot be empty"));
    }

    if command.len() > 2000 {
        return Err(anyhow::anyhow!("command is too long"));
    }

    if command.contains('\0') || command.contains('\n') || command.contains('\r') {
        return Err(anyhow::anyhow!("command contains an invalid character"));
    }

    if has_background_operator(command) {
        return Err(anyhow::anyhow!(
            "background commands are not allowed on web hosting"
        ));
    }

    let lower = command.to_ascii_lowercase();
    for fragment in ["/dev/tcp", "/dev/udp", "/proc/sys", "/sys/fs/cgroup"] {
        if lower.contains(fragment) {
            return Err(anyhow::anyhow!(
                "system and network escape paths are not allowed on web hosting"
            ));
        }
    }

    for fragment in [
        "| sh", "|sh", "| bash", "|bash", "| ash", "|ash", "| zsh", "|zsh", "| fish", "|fish",
        "curl ",
    ] {
        if lower.contains(fragment) {
            return Err(anyhow::anyhow!(
                "piping downloaded content into a shell is not allowed"
            ));
        }
    }

    if (lower.contains("curl ") || lower.contains("wget ")) && lower.contains('|') {
        return Err(anyhow::anyhow!(
            "piping downloaded content into another command is not allowed"
        ));
    }

    for token in command_tokens(command) {
        let command_name = token_basename(&token).to_ascii_lowercase();
        if is_denied_web_hosting_command(&command_name) {
            return Err(anyhow::anyhow!(
                "{} is not allowed on web hosting plans",
                command_name
            ));
        }
    }

    Ok(())
}

fn safe_container_path(
    base: &std::path::Path,
    relative: &str,
) -> Result<std::path::PathBuf, anyhow::Error> {
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

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn has_background_operator(command: &str) -> bool {
    let chars = command.chars().collect::<Vec<_>>();

    for (index, ch) in chars.iter().enumerate() {
        if *ch != '&' {
            continue;
        }

        let previous = index.checked_sub(1).and_then(|i| chars.get(i)).copied();
        let next = chars.get(index + 1).copied();

        if previous != Some('&') && previous != Some('>') && next != Some('&') {
            return true;
        }
    }

    false
}

fn command_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            continue;
        }

        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                current.push(ch);
            }

            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }

        if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '(' | ')' | '<' | '>') {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }

        current.push(ch);
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn token_basename(token: &str) -> &str {
    token
        .trim_matches(|ch| ch == '\'' || ch == '"')
        .trim_start_matches("./")
        .rsplit('/')
        .next()
        .unwrap_or(token)
}

fn is_denied_web_hosting_command(command_name: &str) -> bool {
    matches!(
        command_name,
        "apk"
            | "apt"
            | "apt-get"
            | "aptitude"
            | "ash"
            | "bash"
            | "chroot"
            | "containerd"
            | "cron"
            | "crond"
            | "daemonize"
            | "dnf"
            | "docker"
            | "dockerd"
            | "eval"
            | "exec"
            | "fish"
            | "iptables"
            | "mount"
            | "nc"
            | "ncat"
            | "netcat"
            | "nft"
            | "nohup"
            | "nsenter"
            | "pacman"
            | "pivot_root"
            | "proot"
            | "runc"
            | "screen"
            | "service"
            | "setcap"
            | "setsid"
            | "sh"
            | "socat"
            | "ssh"
            | "sshd"
            | "su"
            | "sudo"
            | "supervisord"
            | "systemctl"
            | "tmux"
            | "umount"
            | "unshare"
            | "yum"
            | "zsh"
            | "zypper"
    )
}

fn append_limited(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
    max_output_bytes: usize,
    truncated: &mut bool,
) {
    if buffer.len() >= max_output_bytes {
        *truncated = true;
        return;
    }

    let available = max_output_bytes.saturating_sub(buffer.len());
    let write_len = chunk.len().min(available);
    buffer.extend_from_slice(&chunk[..write_len]);

    if write_len < chunk.len() {
        *truncated = true;
    }
}

fn build_client(manifest: &HelperAgentManifest) -> Result<reqwest::Client, anyhow::Error> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", manifest.token))
            .context("invalid helper authorization token")?,
    );

    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_millis(
            manifest.timeout_ms.max(250),
        ))
        .build()
        .context("failed to build helper HTTP client")?)
}

fn build_base_url(ip: &str, manifest: &HelperAgentManifest) -> String {
    format!(
        "http://{}:{}{}",
        ip,
        manifest.port,
        manifest.api_base_path.trim_end_matches('/')
    )
}

pub async fn get_status(server: &crate::server::Server) -> Result<HelperStatus, anyhow::Error> {
    let manifest = read_manifest(server).await?;
    let ip = resolve_container_ip(server).await?;
    let client = build_client(&manifest)?;
    let url = format!("{}/status", build_base_url(&ip, &manifest));

    let response: Value = client
        .get(url)
        .send()
        .await
        .context("helper status request failed")?
        .error_for_status()
        .context("helper status request returned an error")?
        .json()
        .await
        .context("failed to decode helper status response")?;

    Ok(HelperStatus {
        connected: response
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        variant: manifest.variant,
        port: manifest.port,
        api_base_path: manifest.api_base_path,
        capabilities: response
            .get("capabilities")
            .cloned()
            .unwrap_or(manifest.capabilities),
    })
}

pub async fn get_player_snapshot(
    server: &crate::server::Server,
    player: &str,
) -> Result<Value, anyhow::Error> {
    let manifest = read_manifest(server).await?;
    let ip = resolve_container_ip(server).await?;
    let client = build_client(&manifest)?;
    let url = format!(
        "{}/players/{}/snapshot",
        build_base_url(&ip, &manifest),
        utf8_percent_encode(player, NON_ALPHANUMERIC)
    );

    client
        .get(url)
        .send()
        .await
        .context("helper snapshot request failed")?
        .error_for_status()
        .context("helper snapshot request returned an error")?
        .json()
        .await
        .context("failed to decode helper snapshot response")
}

pub async fn get_attribute_catalog(server: &crate::server::Server) -> Result<Value, anyhow::Error> {
    let manifest = read_manifest(server).await?;
    let ip = resolve_container_ip(server).await?;
    let client = build_client(&manifest)?;
    let url = format!("{}/attributes", build_base_url(&ip, &manifest));

    client
        .get(url)
        .send()
        .await
        .context("helper attribute catalog request failed")?
        .error_for_status()
        .context("helper attribute catalog request returned an error")?
        .json()
        .await
        .context("failed to decode helper attribute catalog response")
}

fn normalize_helper_variant(variant: &str) -> &str {
    match variant {
        "fabric-legacy" => "legacyfabric",
        _ => variant,
    }
}

fn helper_paths(variant: &str) -> Result<(&'static str, PathBuf, PathBuf), anyhow::Error> {
    match normalize_helper_variant(variant) {
        "spigot" => Ok((
            "spigot",
            PathBuf::from(HOST_HELPER_PLUGIN_PATH),
            PathBuf::from("/plugins/JexactylHelper.jar"),
        )),
        "fabric" => Ok((
            "fabric",
            PathBuf::from(HOST_HELPER_FABRIC_PATH),
            PathBuf::from("/mods/JexactylHelper-Fabric.jar"),
        )),
        "legacyfabric" => Ok((
            "legacyfabric",
            PathBuf::from(HOST_HELPER_LEGACY_FABRIC_PATH),
            PathBuf::from("/mods/JexactylHelper-Fabric-Legacy.jar"),
        )),
        "forge" => Ok((
            "forge",
            PathBuf::from(HOST_HELPER_FORGE_PATH),
            PathBuf::from("/mods/JexactylHelper-Forge.jar"),
        )),
        _ => Err(anyhow::anyhow!("unsupported helper variant")),
    }
}

pub async fn install_host_artifact(
    server: &crate::server::Server,
    variant: &str,
) -> Result<HelperInstallResponse, anyhow::Error> {
    let (normalized_variant, source_path, target_path) = helper_paths(variant)?;

    let source_metadata = tokio::fs::metadata(&source_path)
        .await
        .with_context(|| format!("failed to access helper artifact {}", source_path.display()))?;

    if !source_metadata.is_file() {
        return Err(anyhow::anyhow!(
            "helper artifact {} is not a file",
            source_path.display()
        ));
    }

    let parent = target_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("target file has no parent"))?;
    let file_name = target_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("target file has no file name"))?;

    let (root, filesystem) = server.filesystem.resolve_writable_fs(server, parent).await;
    let destination = root.join(file_name);
    let existing_metadata = filesystem.async_metadata(&destination).await;

    if filesystem.is_primary_server_fs()
        && server
            .filesystem
            .is_ignored(
                &destination,
                existing_metadata
                    .as_ref()
                    .is_ok_and(|m| m.file_type.is_dir()),
            )
            .await
    {
        return Err(anyhow::anyhow!("target path is ignored"));
    }

    if filesystem.is_primary_server_fs() && server.filesystem.is_ignored(parent, true).await {
        return Err(anyhow::anyhow!("target parent directory is ignored"));
    }

    filesystem
        .async_create_dir_all(&parent)
        .await
        .with_context(|| format!("failed to create target directory {}", parent.display()))?;

    let old_size = existing_metadata
        .map(|metadata| metadata.size as i64)
        .unwrap_or(0);
    let new_size = source_metadata.len() as i64;

    if filesystem.is_primary_server_fs()
        && !server
            .filesystem
            .async_allocate_in_path(parent, new_size - old_size, false)
            .await
    {
        return Err(anyhow::anyhow!(
            "failed to allocate space for helper artifact"
        ));
    }

    let bytes = tokio::fs::read(&source_path)
        .await
        .with_context(|| format!("failed to read helper artifact {}", source_path.display()))?;

    let mut file = filesystem
        .async_create_file(&destination)
        .await
        .with_context(|| format!("failed to create target file {}", destination.display()))?;
    file.write_all(&bytes)
        .await
        .with_context(|| format!("failed to write helper artifact {}", destination.display()))?;
    file.shutdown()
        .await
        .with_context(|| format!("failed to flush helper artifact {}", destination.display()))?;
    filesystem
        .async_chown(&destination)
        .await
        .with_context(|| format!("failed to chown helper artifact {}", destination.display()))?;

    Ok(HelperInstallResponse {
        variant: normalized_variant.to_string(),
        source_path: source_path.display().to_string(),
        target_path: target_path.display().to_string(),
    })
}
