#![recursion_limit = "512"]

use std::{fmt::Debug, sync::OnceLock};

pub mod bins;
pub mod commands;
pub mod config;
pub mod deserialize;
pub mod io;
pub mod models;
pub mod net;
pub mod payload;
pub mod remote;
pub mod response;
pub mod routes;
pub mod server;
pub mod ssh;
pub mod stats;
pub mod utils;

pub use payload::Payload;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_COMMIT: &str = match option_env!("CARGO_GIT_COMMIT") {
    Some(value) => value,
    None => "unknown",
};
pub const GIT_BRANCH: &str = match option_env!("CARGO_GIT_BRANCH") {
    Some(value) => value,
    None => "unknown",
};
pub const TARGET: &str = match option_env!("CARGO_TARGET") {
    Some(value) => value,
    None => "unknown-unknown",
};

pub static CLAP_COMMAND: OnceLock<clap::Command> = OnceLock::new();

#[cfg(unix)]
pub const DEFAULT_CONFIG_PATH: &str = "/etc/pterodactyl/config.yml";
#[cfg(windows)]
pub const DEFAULT_CONFIG_PATH: &str = "C:\\ProgramData\\Calagopus-Wings\\config.yml";

/// 32 KiB - used for general IO
pub const BUFFER_SIZE: usize = 32 * 1024;
/// 4 MiB - used for transfers
pub const TRANSFER_BUFFER_SIZE: usize = 4 * 1024 * 1024;

pub fn full_version() -> String {
    if GIT_BRANCH == "unknown" {
        VERSION.to_string()
    } else {
        format!("{VERSION}:{GIT_COMMIT}@{GIT_BRANCH}")
    }
}

pub fn spawn_blocking_handled<
    F: FnOnce() -> Result<(), E> + Send + 'static,
    E: Debug + Send + 'static,
>(
    f: F,
) {
    tokio::spawn(async move {
        match tokio::task::spawn_blocking(f).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                tracing::error!("spawned blocking task failed: {:?}", err);
            }
            Err(err) => {
                tracing::error!("spawned blocking task panicked: {:?}", err);
            }
        }
    });
}

pub fn spawn_blocking_signalled<
    F: FnOnce() -> Result<(), E> + Send + 'static,
    E: Debug + Send + 'static,
>(
    signal: crate::io::fallible_reader::FallibleSignal,
    f: F,
) {
    tokio::spawn(async move {
        match tokio::task::spawn_blocking(f).await {
            Ok(Ok(())) => signal.succeed(),
            Ok(Err(err)) => {
                tracing::error!("spawned blocking task failed: {:?}", err);
                signal.fail(format!("{err:?}"));
            }
            Err(err) => {
                tracing::error!("spawned blocking task panicked: {:?}", err);
                signal.fail(format!("task panicked: {err:?}"));
            }
        }
    });
}

pub fn spawn_handled<
    F: std::future::Future<Output = Result<(), E>> + Send + 'static,
    E: Debug + Send + 'static,
>(
    f: F,
) {
    tokio::spawn(async move {
        match f.await {
            Ok(_) => {}
            Err(err) => {
                tracing::error!("spawned async task failed: {:?}", err);
            }
        }
    });
}

#[inline(always)]
#[cold]
fn cold_path() {}

#[inline(always)]
pub fn likely(b: bool) -> bool {
    if b {
        true
    } else {
        cold_path();
        false
    }
}

#[inline(always)]
pub fn unlikely(b: bool) -> bool {
    if b {
        cold_path();
        true
    } else {
        false
    }
}
