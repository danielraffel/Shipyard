use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use toml::Table;

use crate::identity::{ProductIdentity, RuntimeMode};
use crate::paths::RuntimePaths;

/// Result type for configuration operations.
pub type ConfigResult<T> = Result<T, ConfigLoadError>;

/// Records how the local overlay directory was resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalOverlaySource {
    /// No local overlay was found.
    None,
    /// The current checkout provided the local overlay directly.
    Direct,
    /// The current checkout is a git worktree and borrowed the local
    /// overlay from the main checkout.
    WorktreeFallback,
}

/// Layered Shipyard configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedConfig {
    /// Merged configuration table.
    pub data: Table,
    /// Machine-global configuration directory.
    pub global_dir: PathBuf,
    /// Per-project tracked configuration directory, if present.
    pub project_dir: Option<PathBuf>,
    /// Per-project private overlay directory, if resolved.
    pub local_dir: Option<PathBuf>,
    /// How the local overlay was resolved.
    pub local_overlay_source: LocalOverlaySource,
}

impl LoadedConfig {
    /// Load only the trusted machine-global configuration layer.
    ///
    /// Security-sensitive machine policy must use this instead of the normal
    /// layered loader because tracked and checkout-local configuration is
    /// controlled by the repository being operated on.
    pub fn load_machine_global(mode: RuntimeMode) -> ConfigResult<Self> {
        Self::load_machine_global_from_dir(RuntimePaths::current(mode).global_dir)
    }

    /// Load only an explicitly resolved machine-global configuration layer.
    pub fn load_machine_global_from_dir(global_dir: PathBuf) -> ConfigResult<Self> {
        Self::load(Some(global_dir), None, None, LocalOverlaySource::None)
    }

    /// Load and merge config layers for a given mode and directory.
    pub fn load_from_cwd(mode: RuntimeMode, cwd: &Path) -> ConfigResult<Self> {
        Self::load_from_cwd_with_global_dir(mode, cwd, RuntimePaths::current(mode).global_dir)
    }

    /// Load config layers while honoring an explicitly resolved global directory.
    pub fn load_from_cwd_with_global_dir(
        mode: RuntimeMode,
        cwd: &Path,
        global_dir: PathBuf,
    ) -> ConfigResult<Self> {
        let identity = ProductIdentity::for_mode(mode);
        let project_dir = cwd.join(identity.tracked_project_dir_name);
        let direct_local_dir = cwd.join(identity.local_overlay_dir_name);

        let (local_dir, local_overlay_source) = if has_config_file(&direct_local_dir) {
            (Some(direct_local_dir), LocalOverlaySource::Direct)
        } else if let Some(fallback) = worktree_main_local_dir(cwd, identity.local_overlay_dir_name)
        {
            (Some(fallback), LocalOverlaySource::WorktreeFallback)
        } else {
            (None, LocalOverlaySource::None)
        };

        Self::load(
            Some(global_dir),
            project_dir.exists().then_some(project_dir),
            local_dir,
            local_overlay_source,
        )
    }

    /// Load and merge config from explicit directories.
    pub fn load(
        global_dir: Option<PathBuf>,
        project_dir: Option<PathBuf>,
        local_dir: Option<PathBuf>,
        local_overlay_source: LocalOverlaySource,
    ) -> ConfigResult<Self> {
        let global_dir =
            global_dir.unwrap_or_else(|| RuntimePaths::current(RuntimeMode::Shipyard).global_dir);
        let mut data = Table::new();

        merge_if_present(&mut data, &global_dir.join("config.toml"))?;

        if let Some(project_dir) = &project_dir {
            merge_if_present(&mut data, &project_dir.join("config.toml"))?;
        }

        if let Some(local_dir) = &local_dir {
            merge_if_present(&mut data, &local_dir.join("config.toml"))?;
        }

        Ok(Self {
            data,
            global_dir,
            project_dir,
            local_dir,
            local_overlay_source,
        })
    }

    /// Resolve a dotted key from the merged configuration.
    #[must_use]
    pub fn get<'a>(&'a self, dotted_key: &str) -> Option<&'a toml::Value> {
        let mut current = self.data.get(dotted_key.split('.').next()?)?;
        for part in dotted_key.split('.').skip(1) {
            current = current.get(part)?;
        }
        Some(current)
    }

    /// Resolve a dotted key as a string.
    #[must_use]
    pub fn get_str<'a>(&'a self, dotted_key: &str) -> Option<&'a str> {
        self.get(dotted_key)?.as_str()
    }
}

/// Errors that can occur while loading configuration.
#[derive(Debug)]
pub enum ConfigLoadError {
    /// File I/O failed.
    Io {
        /// Path to the config file that was being read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// TOML parsing failed.
    Parse {
        /// Path to the config file that failed to parse.
        path: PathBuf,
        /// Underlying TOML parser error.
        source: Box<toml::de::Error>,
    },
}

impl Display for ConfigLoadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read config file {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(
                    f,
                    "failed to parse config file {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for ConfigLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source.as_ref()),
        }
    }
}

fn merge_if_present(base: &mut Table, path: &Path) -> ConfigResult<()> {
    if !path.exists() {
        return Ok(());
    }

    let contents = fs::read_to_string(path).map_err(|source| ConfigLoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed = contents
        .parse::<toml::Table>()
        .map_err(|source| ConfigLoadError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
    deep_merge(base, &parsed);
    Ok(())
}

fn deep_merge(base: &mut Table, overlay: &Table) {
    for (key, value) in overlay {
        match (base.get_mut(key), value) {
            (Some(toml::Value::Table(base_table)), toml::Value::Table(overlay_table)) => {
                deep_merge(base_table, overlay_table);
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn has_config_file(dir: &Path) -> bool {
    dir.join("config.toml").exists()
}

fn worktree_main_local_dir(base: &Path, local_overlay_dir_name: &str) -> Option<PathBuf> {
    let common_output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(base)
        .output()
        .ok()?;

    if !common_output.status.success() {
        return None;
    }

    let common_dir = String::from_utf8_lossy(&common_output.stdout);
    let common_dir = common_dir.trim();
    if common_dir.is_empty() {
        return None;
    }

    let common_dir = if Path::new(common_dir).is_absolute() {
        PathBuf::from(common_dir)
    } else {
        base.join(common_dir)
    };

    let main_checkout = common_dir.parent()?.to_path_buf();
    let toplevel_output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(base)
        .output()
        .ok()?;

    if !toplevel_output.status.success() {
        return None;
    }

    let repo_toplevel = String::from_utf8_lossy(&toplevel_output.stdout);
    let repo_toplevel = PathBuf::from(repo_toplevel.trim());

    if main_checkout.canonicalize().ok()? == repo_toplevel.canonicalize().ok()? {
        return None;
    }

    let candidate = main_checkout.join(local_overlay_dir_name);
    has_config_file(&candidate).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::{Command, Stdio};

    use tempfile::TempDir;

    use super::{LoadedConfig, LocalOverlaySource};
    use crate::identity::RuntimeMode;

    #[test]
    fn merges_global_project_and_local_layers() {
        let sandbox = TempDir::new().expect("tempdir");
        let global_dir = sandbox.path().join("global");
        let project_dir = sandbox.path().join(".shipyard");
        let local_dir = sandbox.path().join(".shipyard.local");

        std::fs::create_dir_all(&global_dir).expect("global dir");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::create_dir_all(&local_dir).expect("local dir");

        std::fs::write(
            global_dir.join("config.toml"),
            "[cloud]\nprovider = \"github-hosted\"\n[defaults]\npriority = \"normal\"\n",
        )
        .expect("write global");
        std::fs::write(
            project_dir.join("config.toml"),
            "[cloud]\nprovider = \"namespace\"\n[project]\nname = \"my-project\"\n",
        )
        .expect("write project");
        std::fs::write(
            local_dir.join("config.toml"),
            "[targets.ubuntu]\nhost = \"vm.local\"\n",
        )
        .expect("write local");

        let config = LoadedConfig::load(
            Some(global_dir),
            Some(project_dir),
            Some(local_dir),
            LocalOverlaySource::Direct,
        )
        .expect("load config");

        assert_eq!(config.get_str("cloud.provider"), Some("namespace"));
        assert_eq!(config.get_str("defaults.priority"), Some("normal"));
        assert_eq!(config.get_str("project.name"), Some("my-project"));
        assert_eq!(config.get_str("targets.ubuntu.host"), Some("vm.local"));
    }

    #[test]
    fn cwd_loader_honors_explicit_global_dir_and_keeps_project_layers() {
        let sandbox = TempDir::new().expect("tempdir");
        let global_dir = sandbox.path().join("explicit-global");
        let repo = sandbox.path().join("repo");
        let project_dir = repo.join(".shipyard");
        let local_dir = repo.join(".shipyard.local");
        std::fs::create_dir_all(&global_dir).expect("global dir");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::create_dir_all(&local_dir).expect("local dir");
        std::fs::write(
            global_dir.join("config.toml"),
            "[github.auth]\nsource = \"env\"\ntoken_env = \"EXPLICIT_TOKEN\"\n",
        )
        .expect("global config");
        std::fs::write(
            project_dir.join("config.toml"),
            "[governance]\nrequired_status_checks = [\"windows\"]\n",
        )
        .expect("project config");
        std::fs::write(
            local_dir.join("config.toml"),
            "[project]\nname = \"observer-test\"\n",
        )
        .expect("local config");

        let config = LoadedConfig::load_from_cwd_with_global_dir(
            RuntimeMode::Shipyard,
            &repo,
            global_dir.clone(),
        )
        .expect("load config");

        assert_eq!(config.global_dir, global_dir);
        assert_eq!(
            config.get_str("github.auth.token_env"),
            Some("EXPLICIT_TOKEN")
        );
        assert_eq!(
            config
                .get("governance.required_status_checks")
                .and_then(toml::Value::as_array)
                .and_then(|checks| checks.first())
                .and_then(toml::Value::as_str),
            Some("windows")
        );
        assert_eq!(config.get_str("project.name"), Some("observer-test"));
    }

    #[test]
    fn worktree_can_borrow_main_checkout_local_overlay_in_shipyard_mode() {
        let sandbox = TempDir::new().expect("tempdir");
        let main_repo = sandbox.path().join("main");
        seed_git_repo(&main_repo);

        std::fs::create_dir_all(main_repo.join(".shipyard")).expect("project dir");
        std::fs::write(
            main_repo.join(".shipyard").join("config.toml"),
            "[targets.windows]\nbackend = \"ssh-windows\"\n",
        )
        .expect("write project config");
        std::fs::create_dir_all(main_repo.join(".shipyard.local")).expect("local dir");
        std::fs::write(
            main_repo.join(".shipyard.local").join("config.toml"),
            "[targets.windows]\nhost = \"win.example\"\n",
        )
        .expect("write local config");
        std::fs::write(main_repo.join(".gitignore"), ".shipyard.local/\n").expect("gitignore");
        std::fs::write(main_repo.join("README.md"), "seed\n").expect("readme");

        git(&["add", "."], &main_repo);
        git(&["commit", "-q", "-m", "seed"], &main_repo);

        let worktree = sandbox.path().join("feature");
        git(
            &[
                "worktree",
                "add",
                "-b",
                "feature/x",
                worktree.to_str().expect("worktree path"),
            ],
            &main_repo,
        );

        let config =
            LoadedConfig::load_from_cwd(RuntimeMode::Shipyard, &worktree).expect("load config");

        assert_eq!(config.get_str("targets.windows.host"), Some("win.example"));
        assert_eq!(
            config.local_overlay_source,
            LocalOverlaySource::WorktreeFallback
        );
    }

    #[test]
    fn main_checkout_subdir_does_not_trigger_worktree_fallback() {
        let sandbox = TempDir::new().expect("tempdir");
        let repo = sandbox.path().join("main");
        seed_git_repo(&repo);

        std::fs::create_dir_all(repo.join(".shipyard")).expect("project dir");
        std::fs::write(
            repo.join(".shipyard").join("config.toml"),
            "[project]\nname = \"demo\"\n",
        )
        .expect("write project config");
        std::fs::create_dir_all(repo.join(".shipyard.local")).expect("local dir");
        std::fs::write(
            repo.join(".shipyard.local").join("config.toml"),
            "[targets.windows]\nhost = \"win.example\"\n",
        )
        .expect("write local config");
        std::fs::create_dir_all(repo.join("src")).expect("subdir");
        std::fs::write(repo.join("README.md"), "seed\n").expect("readme");

        git(&["add", "."], &repo);
        git(&["commit", "-q", "-m", "seed"], &repo);

        let subdir = repo.join("src");
        let config =
            LoadedConfig::load_from_cwd(RuntimeMode::Shipyard, &subdir).expect("load config");

        assert_eq!(config.local_overlay_source, LocalOverlaySource::None);
        assert_eq!(config.local_dir, None);
        assert_eq!(config.get_str("project.name"), None);
    }

    #[test]
    fn isolated_mode_prefers_shipyard_rust_local_overlay() {
        let sandbox = TempDir::new().expect("tempdir");
        let repo = sandbox.path().join("repo");
        std::fs::create_dir_all(repo.join(".shipyard")).expect("project dir");
        std::fs::create_dir_all(repo.join(".shipyard.local")).expect("legacy local dir");
        std::fs::create_dir_all(repo.join(".shipyard-dev.local")).expect("rust local dir");

        std::fs::write(
            repo.join(".shipyard").join("config.toml"),
            "[project]\nname = \"demo\"\n",
        )
        .expect("write project");
        std::fs::write(
            repo.join(".shipyard.local").join("config.toml"),
            "[targets.windows]\nhost = \"legacy.example\"\n",
        )
        .expect("write legacy local");
        std::fs::write(
            repo.join(".shipyard-dev.local").join("config.toml"),
            "[targets.windows]\nhost = \"rust.example\"\n",
        )
        .expect("write rust local");

        let config =
            LoadedConfig::load_from_cwd(RuntimeMode::Isolated, &repo).expect("load config");

        assert_eq!(config.get_str("targets.windows.host"), Some("rust.example"));
        assert_eq!(config.local_overlay_source, LocalOverlaySource::Direct);
    }

    fn seed_git_repo(repo: &Path) {
        std::fs::create_dir_all(repo).expect("repo dir");
        git(&["init", "--quiet", "--initial-branch=main"], repo);
    }

    fn git(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git command should run");

        assert!(status.success(), "git command failed: {args:?}");
    }
}
