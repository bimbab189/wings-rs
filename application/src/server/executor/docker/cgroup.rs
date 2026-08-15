#[cfg(target_os = "linux")]
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

#[cfg(target_os = "linux")]
const ROOT: &str = "/sys/fs/cgroup";

#[cfg(target_os = "linux")]
pub fn is_unified() -> bool {
    static UNIFIED: OnceLock<bool> = OnceLock::new();

    *UNIFIED.get_or_init(|| Path::new(ROOT).join("cgroup.controllers").exists())
}

#[cfg(not(target_os = "linux"))]
pub fn is_unified() -> bool {
    false
}

/// The CPU limit a quota/period pair expresses, in percent of a single core.
/// Zero when unlimited.
pub fn limit_percent(quota_us: i64, period_us: i64) -> u32 {
    if quota_us <= 0 || period_us <= 0 {
        return 0;
    }

    (quota_us * 100 / period_us) as u32
}

/// The CFS burst value to write for a quota, in microseconds. The kernel enforces
/// `burst <= quota`, so the multiple is clamped to at most 1.
pub fn burst_us(quota_us: i64, multiple: f64) -> u64 {
    if quota_us <= 0 {
        return 0;
    }

    (quota_us as f64 * multiple.clamp(0.0, 1.0)) as u64
}

#[cfg(target_os = "linux")]
pub fn burst_path(proc_cgroup: &str, unified: bool) -> Option<PathBuf> {
    for line in proc_cgroup.lines() {
        let Some((hierarchy, rest)) = line.split_once(':') else {
            continue;
        };
        let Some((controllers, path)) = rest.split_once(':') else {
            continue;
        };
        if path.is_empty() {
            continue;
        }

        let path = path.trim_start_matches('/');

        if unified {
            if hierarchy == "0" && controllers.is_empty() {
                return Some(Path::new(ROOT).join(path).join("cpu.max.burst"));
            }
        } else if controllers.split(',').any(|controller| controller == "cpu") {
            return Some(
                Path::new(ROOT)
                    .join("cpu")
                    .join(path)
                    .join("cpu.cfs_burst_us"),
            );
        }
    }

    None
}

#[cfg(target_os = "linux")]
fn is_unsupported(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::ReadOnlyFilesystem
    )
}

#[cfg(target_os = "linux")]
pub async fn write_burst(pid: i64, burst_us: u64) {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();

    if SUPPORTED.get() == Some(&false) {
        return;
    }

    let unified = is_unified();
    let proc_cgroup = match tokio::fs::read_to_string(format!("/proc/{pid}/cgroup")).await {
        Ok(proc_cgroup) => proc_cgroup,
        Err(err) => {
            tracing::debug!(pid, "failed to read cgroup of process: {}", err);

            return;
        }
    };

    let Some(path) = burst_path(&proc_cgroup, unified) else {
        tracing::debug!(pid, unified, "no cpu cgroup found for process");

        return;
    };

    match tokio::fs::write(&path, burst_us.to_string()).await {
        Ok(()) => {
            SUPPORTED.set(true).ok();

            tracing::debug!(pid, burst_us, "wrote cfs burst to {}", path.display());
        }
        Err(err) if is_unsupported(&err) => {
            if SUPPORTED.set(false).is_ok() {
                tracing::debug!(
                    "cfs burst is unsupported on this host, {} could not be written: {}",
                    path.display(),
                    err
                );
            }
        }
        Err(err) => {
            tracing::debug!(pid, "failed to write cfs burst: {}", err);
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn write_burst(_pid: i64, _burst_us: u64) {}

#[cfg(test)]
mod tests {
    use super::*;

    // limit_percent

    #[test]
    fn limit_percent_is_zero_without_a_quota() {
        assert_eq!(limit_percent(-1, 100000), 0);
        assert_eq!(limit_percent(100000, 0), 0);
    }

    // burst_us

    #[test]
    fn burst_us_never_exceeds_the_quota() {
        assert_eq!(burst_us(200000, 1.0), 200000);
        assert_eq!(burst_us(200000, 0.5), 100000);
        assert_eq!(burst_us(200000, 2.5), 200000);
        assert_eq!(burst_us(200000, f64::INFINITY), 200000);
    }

    #[test]
    fn burst_us_is_zero_without_a_quota() {
        assert_eq!(burst_us(0, 1.0), 0);
        assert_eq!(burst_us(-1, 1.0), 0);
    }

    #[test]
    fn burst_us_is_zero_for_a_non_positive_multiple() {
        assert_eq!(burst_us(200000, 0.0), 0);
        assert_eq!(burst_us(200000, -1.0), 0);
        assert_eq!(burst_us(200000, f64::NAN), 0);
    }

    struct Kernel {
        quota_us: i64,
        burst_us: u64,
    }

    impl Kernel {
        fn write_quota(&mut self, quota_us: i64) -> Result<(), ()> {
            if quota_us > 0 && self.burst_us > quota_us as u64 {
                return Err(());
            }

            self.quota_us = quota_us;
            Ok(())
        }

        fn write_burst(&mut self, burst_us: u64) -> Result<(), ()> {
            if self.quota_us > 0 && burst_us > self.quota_us as u64 {
                return Err(());
            }

            self.burst_us = burst_us;
            Ok(())
        }
    }

    #[test]
    fn lowering_a_quota_needs_the_burst_cleared_first() {
        let mut kernel = Kernel {
            quota_us: 400000,
            burst_us: burst_us(400000, 1.0),
        };

        assert!(kernel.write_quota(100000).is_err());

        assert!(kernel.write_burst(0).is_ok());
        assert!(kernel.write_quota(100000).is_ok());
        assert!(kernel.write_burst(burst_us(100000, 1.0)).is_ok());
        assert_eq!(kernel.burst_us, 100000);
    }

    #[test]
    fn raising_a_quota_survives_the_same_ordering() {
        let mut kernel = Kernel {
            quota_us: 100000,
            burst_us: burst_us(100000, 1.0),
        };

        assert!(kernel.write_burst(0).is_ok());
        assert!(kernel.write_quota(400000).is_ok());
        assert!(kernel.write_burst(burst_us(400000, 1.0)).is_ok());
        assert_eq!(kernel.burst_us, 400000);
    }

    // burst_path

    #[cfg(target_os = "linux")]
    #[test]
    fn burst_path_resolves_the_unified_line() {
        assert_eq!(
            burst_path("0::/system.slice/docker-3f1a.scope\n", true).unwrap(),
            Path::new("/sys/fs/cgroup/system.slice/docker-3f1a.scope/cpu.max.burst")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn burst_path_resolves_the_v1_cpu_controller() {
        let proc_cgroup = "12:devices:/docker/3f1a\n\
             4:cpu,cpuacct:/docker/3f1a\n\
             3:memory:/docker/3f1a\n\
             0::/\n";

        assert_eq!(
            burst_path(proc_cgroup, false).unwrap(),
            Path::new("/sys/fs/cgroup/cpu/docker/3f1a/cpu.cfs_burst_us")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn burst_path_ignores_controllers_that_merely_contain_cpu() {
        let proc_cgroup = "5:cpuset:/docker/3f1a\n4:cpuacct:/docker/3f1a\n";

        assert!(burst_path(proc_cgroup, false).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn burst_path_ignores_the_unified_line_on_a_v1_host() {
        assert!(burst_path("0::/docker/3f1a\n", false).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn burst_path_ignores_an_empty_cgroup_path() {
        assert!(burst_path("0::\n", true).is_none());
        assert!(burst_path("", true).is_none());
    }
}
