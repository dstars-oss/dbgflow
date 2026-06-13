use dbgflow_common::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbgEngLocation {
    pub path: PathBuf,
    pub source: DbgEngSource,
}

pub const DBGFLOW_DBGENG_DIR_ENV: &str = "DBGFLOW_DBGENG_DIR";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DbgEngSource {
    Environment,
    AppStore,
    WindowsSdk,
    System32,
}

pub fn resolve_dbgeng() -> Result<DbgEngLocation> {
    resolve_dbgeng_from_roots(&default_roots())
}

fn resolve_dbgeng_from_roots(roots: &DbgEngRoots) -> Result<DbgEngLocation> {
    if let Some(path) = roots
        .environment_dbgeng_dir
        .as_deref()
        .and_then(find_dbgeng_in_debuggers_dir)
    {
        return Ok(DbgEngLocation {
            path,
            source: DbgEngSource::Environment,
        });
    }

    if let Some(path) = find_app_store_dbgeng(&roots.program_files_windows_apps) {
        return Ok(DbgEngLocation {
            path,
            source: DbgEngSource::AppStore,
        });
    }

    if let Some(path) = find_sdk_dbgeng(&roots.windows_kits_roots) {
        return Ok(DbgEngLocation {
            path,
            source: DbgEngSource::WindowsSdk,
        });
    }

    let system32 = roots.system_root.join("System32").join("dbgeng.dll");
    if system32.is_file() {
        return Ok(DbgEngLocation {
            path: system32,
            source: DbgEngSource::System32,
        });
    }

    Err(DbgFlowError::Backend(
        "dbgeng.dll not found in App Store, Windows SDK, or System32 locations".to_string(),
    ))
}

fn default_roots() -> DbgEngRoots {
    let program_files = env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    let program_files_x86 = env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"));
    let system_root = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let environment_dbgeng_dir = env::var_os(DBGFLOW_DBGENG_DIR_ENV).map(PathBuf::from);

    DbgEngRoots {
        environment_dbgeng_dir,
        program_files_windows_apps: program_files.join("WindowsApps"),
        windows_kits_roots: vec![
            program_files_x86.join("Windows Kits").join("10"),
            program_files.join("Windows Kits").join("10"),
        ],
        system_root,
    }
}

fn find_app_store_dbgeng(root: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    let mut packages = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("Microsoft.WinDbg"))
        })
        .collect::<Vec<_>>();
    packages.sort();
    packages.reverse();

    packages
        .into_iter()
        .find_map(|package| find_file_limited(&package, "dbgeng.dll", 4))
}

fn find_sdk_dbgeng(roots: &[PathBuf]) -> Option<PathBuf> {
    let arch = debugger_arch();
    roots.iter().find_map(|root| {
        find_dbgeng_in_debuggers_dir(&root.join("Debuggers").join(arch))
            .or_else(|| find_dbgeng_in_debuggers_dir(root))
    })
}

fn find_dbgeng_in_debuggers_dir(dir: &Path) -> Option<PathBuf> {
    let path = dir.join("dbgeng.dll");
    path.is_file().then_some(path)
}

fn find_file_limited(root: &Path, file_name: &str, max_depth: usize) -> Option<PathBuf> {
    if max_depth == 0 {
        return None;
    }

    let entries = fs::read_dir(root).ok()?;
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case(file_name))
        {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_file_limited(&path, file_name, max_depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

fn debugger_arch() -> &'static str {
    if cfg!(target_arch = "x86") {
        "x86"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    }
}

#[cfg(test)]
#[derive(Debug)]
struct DbgEngRoots {
    environment_dbgeng_dir: Option<PathBuf>,
    program_files_windows_apps: PathBuf,
    windows_kits_roots: Vec<PathBuf>,
    system_root: PathBuf,
}

#[cfg(not(test))]
#[derive(Debug)]
struct DbgEngRoots {
    environment_dbgeng_dir: Option<PathBuf>,
    program_files_windows_apps: PathBuf,
    windows_kits_roots: Vec<PathBuf>,
    system_root: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_app_store_over_sdk_and_system32() {
        let root = unique_test_dir("dbgeng-resolver-appstore");
        let apps = root.join("WindowsApps");
        let sdk = root.join("Windows Kits").join("10");
        let system_root = root.join("Windows");

        touch(
            apps.join("Microsoft.WinDbg_1.0.0.0_x64__8wekyb3d8bbwe")
                .join("amd64")
                .join("dbgeng.dll"),
        );
        touch(
            sdk.join("Debuggers")
                .join(debugger_arch())
                .join("dbgeng.dll"),
        );
        touch(system_root.join("System32").join("dbgeng.dll"));

        let location = resolve_dbgeng_from_roots(&DbgEngRoots {
            environment_dbgeng_dir: None,
            program_files_windows_apps: apps,
            windows_kits_roots: vec![sdk],
            system_root,
        })
        .expect("resolve dbgeng");

        assert_eq!(location.source, DbgEngSource::AppStore);
    }

    #[test]
    fn prefers_sdk_over_system32() {
        let root = unique_test_dir("dbgeng-resolver-sdk");
        let sdk = root.join("Windows Kits").join("10");
        let system_root = root.join("Windows");

        touch(
            sdk.join("Debuggers")
                .join(debugger_arch())
                .join("dbgeng.dll"),
        );
        touch(system_root.join("System32").join("dbgeng.dll"));

        let location = resolve_dbgeng_from_roots(&DbgEngRoots {
            environment_dbgeng_dir: None,
            program_files_windows_apps: root.join("WindowsApps"),
            windows_kits_roots: vec![sdk],
            system_root,
        })
        .expect("resolve dbgeng");

        assert_eq!(location.source, DbgEngSource::WindowsSdk);
    }

    #[test]
    fn prefers_environment_dbgeng_dir_over_other_sources() {
        let root = unique_test_dir("dbgeng-resolver-env");
        let env_dir = root.join("Debuggers").join(debugger_arch());
        let sdk = root.join("Windows Kits").join("10");

        touch(env_dir.join("dbgeng.dll"));
        touch(
            sdk.join("Debuggers")
                .join(debugger_arch())
                .join("dbgeng.dll"),
        );

        let location = resolve_dbgeng_from_roots(&DbgEngRoots {
            environment_dbgeng_dir: Some(env_dir.clone()),
            program_files_windows_apps: root.join("WindowsApps"),
            windows_kits_roots: vec![sdk],
            system_root: root.join("Windows"),
        })
        .expect("resolve dbgeng");

        assert_eq!(location.source, DbgEngSource::Environment);
        assert_eq!(location.path, env_dir.join("dbgeng.dll"));
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let root = env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create test root");
        root
    }

    fn touch(path: PathBuf) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, b"").expect("touch file");
    }
}
