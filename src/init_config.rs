//! Project initialization and ecosystem detection.

use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use toml::{Table, Value as TomlValue};

use crate::identity::{ProductIdentity, RuntimeMode};

/// Result of initializing a project-layer Shipyard config.
#[derive(Clone, Debug, PartialEq)]
pub struct InitResult {
    /// Generated project configuration.
    pub config: Table,
    /// Directory containing the tracked project configuration.
    pub project_dir: PathBuf,
    /// Written `.shipyard/config.toml` path.
    pub config_path: PathBuf,
    /// Gitignore file updated with the local overlay directory.
    pub gitignore_path: PathBuf,
}

/// Initialization failure.
#[derive(Debug)]
pub enum InitError {
    /// Filesystem operation failed.
    Io {
        /// Path that was being accessed.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

impl Display for InitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl Error for InitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
        }
    }
}

/// Generate and write a non-interactive project config for `path`.
///
/// This mirrors Python Shipyard's current `shipyard init` behavior: `discover-only`
/// is an output choice in the CLI, not a write-suppression mode.
pub fn run_init(path: &Path, mode: RuntimeMode) -> Result<InitResult, InitError> {
    let identity = ProductIdentity::for_mode(mode);
    let info = detect_project(path);
    let project_name = project_name(path);
    let config = build_config_data(&info, &project_name);
    let project_dir = path.join(identity.tracked_project_dir_name);
    let config_path = project_dir.join("config.toml");

    let writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&config_path)
        .map_err(|source| InitError::Io {
            path: config_path.clone(),
            source,
        })?;
    fs::create_dir_all(&project_dir).map_err(|source| InitError::Io {
        path: project_dir.clone(),
        source,
    })?;
    fs::write(&config_path, format!("{config}\n")).map_err(|source| InitError::Io {
        path: config_path.clone(),
        source,
    })?;
    drop(writer_domain);
    let gitignore_path = ensure_gitignore(path, identity.local_overlay_dir_name)?;

    Ok(InitResult {
        config,
        project_dir,
        config_path,
        gitignore_path,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectInfo {
    ecosystems: Vec<Ecosystem>,
    platforms: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidationCommands {
    install: Option<&'static str>,
    build: Option<&'static str>,
    test: Option<&'static str>,
    validate: Option<&'static str>,
}

impl ValidationCommands {
    const fn new(
        install: Option<&'static str>,
        build: Option<&'static str>,
        test: Option<&'static str>,
        validate: Option<&'static str>,
    ) -> Self {
        Self {
            install,
            build,
            test,
            validate,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Ecosystem {
    name: &'static str,
    family: &'static str,
    commands: ValidationCommands,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EcosystemSpec {
    name: &'static str,
    family: &'static str,
    markers: &'static [&'static str],
    commands: ValidationCommands,
    check: CheckKind,
    priority: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckKind {
    Markers,
    XcodeProject,
    Dotnet,
    Flutter,
    Dart,
}

fn project_name(path: &Path) -> String {
    path.file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_owned()
}

fn detect_project(path: &Path) -> ProjectInfo {
    let ecosystems = detect_all(path);
    let platforms = infer_platforms(&ecosystems);
    ProjectInfo {
        ecosystems,
        platforms,
    }
}

fn build_config_data(info: &ProjectInfo, project_name: &str) -> Table {
    let mut data = Table::new();
    data.insert(
        "project".to_owned(),
        TomlValue::Table(project_table(info, project_name)),
    );
    if let Some(validation) = validation_table(info) {
        data.insert("validation".to_owned(), TomlValue::Table(validation));
    }
    let targets = targets_table(&info.platforms);
    if !targets.is_empty() {
        data.insert("targets".to_owned(), TomlValue::Table(targets));
    }
    data
}

fn project_table(info: &ProjectInfo, project_name: &str) -> Table {
    let mut project = Table::new();
    project.insert(
        "name".to_owned(),
        TomlValue::String(project_name.to_owned()),
    );
    project.insert(
        "platforms".to_owned(),
        TomlValue::Array(
            info.platforms
                .iter()
                .cloned()
                .map(TomlValue::String)
                .collect(),
        ),
    );
    if let Some(primary) = info.ecosystems.first() {
        project.insert(
            "type".to_owned(),
            TomlValue::String(primary.name.to_owned()),
        );
    }
    project
}

fn validation_table(info: &ProjectInfo) -> Option<Table> {
    let primary = info.ecosystems.first()?;
    let mut default = Table::new();
    insert_optional_command(&mut default, "install", primary.commands.install);
    insert_optional_command(&mut default, "build", primary.commands.build);
    insert_optional_command(&mut default, "test", primary.commands.test);
    insert_optional_command(&mut default, "validate", primary.commands.validate);
    if default.is_empty() {
        return None;
    }

    let mut validation = Table::new();
    validation.insert("default".to_owned(), TomlValue::Table(default));
    Some(validation)
}

fn insert_optional_command(table: &mut Table, key: &str, command: Option<&'static str>) {
    if let Some(command) = command {
        table.insert(key.to_owned(), TomlValue::String(command.to_owned()));
    }
}

fn targets_table(platforms: &[String]) -> Table {
    let mut targets = Table::new();
    if platforms.iter().any(|platform| platform == "macos") {
        targets.insert(
            "mac".to_owned(),
            TomlValue::Table(target_table("local", "macos-arm64")),
        );
    }
    if platforms.iter().any(|platform| platform == "linux") {
        targets.insert(
            "ubuntu".to_owned(),
            TomlValue::Table(target_table("cloud", "linux-x64")),
        );
    }
    if platforms.iter().any(|platform| platform == "windows") {
        targets.insert(
            "windows".to_owned(),
            TomlValue::Table(target_table("cloud", "windows-x64")),
        );
    }
    targets
}

fn target_table(backend: &str, platform: &str) -> Table {
    let mut target = Table::new();
    target.insert("backend".to_owned(), TomlValue::String(backend.to_owned()));
    target.insert(
        "platform".to_owned(),
        TomlValue::String(platform.to_owned()),
    );
    target
}

fn ensure_gitignore(path: &Path, local_overlay_dir_name: &str) -> Result<PathBuf, InitError> {
    let gitignore = path.join(".gitignore");
    let entry = format!("{local_overlay_dir_name}/");
    match fs::read_to_string(&gitignore) {
        Ok(mut content) => {
            if content.contains(&entry) {
                return Ok(gitignore);
            }
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&entry);
            content.push('\n');
            fs::write(&gitignore, content).map_err(|source| InitError::Io {
                path: gitignore.clone(),
                source,
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::write(&gitignore, format!("{entry}\n")).map_err(|source| InitError::Io {
                path: gitignore.clone(),
                source,
            })?;
        }
        Err(source) => {
            return Err(InitError::Io {
                path: gitignore,
                source,
            });
        }
    }
    Ok(gitignore)
}

fn detect_all(path: &Path) -> Vec<Ecosystem> {
    let mut specs: Vec<(usize, EcosystemSpec)> = registry().into_iter().enumerate().collect();
    specs.sort_by(|(left_index, left), (right_index, right)| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left_index.cmp(right_index))
    });

    let mut seen_families = BTreeSet::new();
    let mut ecosystems = Vec::new();
    for (_, spec) in specs {
        if seen_families.contains(spec.family) || !matches_spec(path, &spec) {
            continue;
        }
        seen_families.insert(spec.family);
        ecosystems.push(Ecosystem {
            name: spec.name,
            family: spec.family,
            commands: spec.commands,
        });
    }
    ecosystems
}

fn infer_platforms(ecosystems: &[Ecosystem]) -> Vec<String> {
    let families: BTreeSet<&str> = ecosystems
        .iter()
        .map(|ecosystem| ecosystem.family)
        .collect();
    if families.contains("apple") && families.len() == 1 {
        return strings(&["macos"]);
    }

    let cross_platform = BTreeSet::from([
        "cpp", "rust", "go", "node", "python", "jvm", "dotnet", "dart", "deno",
    ]);
    let mut platforms = if families
        .iter()
        .any(|family| cross_platform.contains(family))
    {
        strings(&["macos", "linux", "windows"])
    } else if families.contains("apple") {
        strings(&["macos"])
    } else {
        strings(&["macos", "linux"])
    };

    if families.contains("apple") && !platforms.iter().any(|platform| platform == "macos") {
        platforms.insert(0, "macos".to_owned());
    }
    platforms
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn matches_spec(path: &Path, spec: &EcosystemSpec) -> bool {
    match spec.check {
        CheckKind::Markers => spec.markers.iter().any(|marker| path.join(marker).exists()),
        CheckKind::XcodeProject => {
            has_root_entry_with_extension(path, "xcodeproj")
                || has_root_entry_with_extension(path, "xcworkspace")
        }
        CheckKind::Dotnet => {
            has_root_entry_with_extension(path, "csproj")
                || has_root_entry_with_extension(path, "fsproj")
                || has_root_entry_with_extension(path, "sln")
        }
        CheckKind::Flutter => {
            pubspec_contains(path, "flutter:") || pubspec_contains(path, "flutter_test:")
        }
        CheckKind::Dart => {
            path.join("pubspec.yaml").exists() && !pubspec_contains(path, "flutter:")
        }
    }
}

fn has_root_entry_with_extension(path: &Path, extension: &str) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .path()
            .extension()
            .is_some_and(|entry_extension| entry_extension == extension)
    })
}

fn pubspec_contains(path: &Path, needle: &str) -> bool {
    fs::read_to_string(path.join("pubspec.yaml")).is_ok_and(|content| content.contains(needle))
}

fn registry() -> Vec<EcosystemSpec> {
    let mut specs = Vec::new();
    specs.extend(cpp_and_apple_specs());
    specs.extend(rust_and_go_specs());
    specs.extend(node_specs());
    specs.extend(python_specs());
    specs.extend(jvm_and_dotnet_specs());
    specs.extend(dart_specs());
    specs.extend(server_runtime_specs());
    specs
}

fn cpp_and_apple_specs() -> Vec<EcosystemSpec> {
    vec![
        spec(
            "cmake",
            "cpp",
            &["CMakeLists.txt"],
            commands(
                None,
                Some("cmake -S . -B build && cmake --build build"),
                Some("ctest --test-dir build --output-on-failure"),
            ),
            CheckKind::Markers,
            0,
        ),
        spec(
            "swift-spm",
            "apple",
            &["Package.swift"],
            commands(None, Some("swift build"), Some("swift test")),
            CheckKind::Markers,
            10,
        ),
        spec(
            "xcode",
            "apple",
            &[],
            commands(
                None,
                Some("xcodebuild -scheme default build"),
                Some("xcodebuild -scheme default test"),
            ),
            CheckKind::XcodeProject,
            5,
        ),
    ]
}

fn rust_and_go_specs() -> Vec<EcosystemSpec> {
    vec![
        spec(
            "rust",
            "rust",
            &["Cargo.toml"],
            commands(None, Some("cargo build"), Some("cargo test")),
            CheckKind::Markers,
            0,
        ),
        spec(
            "go",
            "go",
            &["go.mod"],
            commands(None, Some("go build ./..."), Some("go test ./...")),
            CheckKind::Markers,
            0,
        ),
    ]
}

fn node_specs() -> Vec<EcosystemSpec> {
    vec![
        spec(
            "node-pnpm",
            "node",
            &["pnpm-lock.yaml"],
            commands(
                Some("pnpm install --frozen-lockfile"),
                Some("pnpm run build"),
                Some("pnpm test"),
            ),
            CheckKind::Markers,
            50,
        ),
        spec(
            "node-bun",
            "node",
            &["bun.lockb"],
            commands(
                Some("bun install --frozen-lockfile"),
                Some("bun run build"),
                Some("bun test"),
            ),
            CheckKind::Markers,
            40,
        ),
        spec(
            "node-yarn",
            "node",
            &["yarn.lock"],
            commands(
                Some("yarn install --frozen-lockfile"),
                Some("yarn build"),
                Some("yarn test"),
            ),
            CheckKind::Markers,
            30,
        ),
        spec(
            "node-npm",
            "node",
            &["package-lock.json"],
            commands(Some("npm ci"), Some("npm run build"), Some("npm test")),
            CheckKind::Markers,
            20,
        ),
        spec(
            "node-npm-default",
            "node",
            &["package.json"],
            commands(Some("npm install"), Some("npm run build"), Some("npm test")),
            CheckKind::Markers,
            10,
        ),
    ]
}

fn python_specs() -> Vec<EcosystemSpec> {
    vec![
        spec(
            "python-uv",
            "python",
            &["uv.lock"],
            commands(
                Some("uv sync"),
                Some("uv run python -m build"),
                Some("uv run pytest"),
            ),
            CheckKind::Markers,
            50,
        ),
        spec(
            "python-poetry",
            "python",
            &["poetry.lock"],
            commands(
                Some("poetry install"),
                Some("poetry build"),
                Some("poetry run pytest"),
            ),
            CheckKind::Markers,
            40,
        ),
        spec(
            "python-pipenv",
            "python",
            &["Pipfile.lock"],
            commands(Some("pipenv install"), None, Some("pipenv run pytest")),
            CheckKind::Markers,
            30,
        ),
        spec(
            "python-pip",
            "python",
            &["requirements.txt"],
            commands(
                Some("pip install -r requirements.txt"),
                None,
                Some("pytest"),
            ),
            CheckKind::Markers,
            20,
        ),
        spec(
            "python-setuptools",
            "python",
            &["setup.py"],
            commands(
                Some("pip install -e ."),
                Some("python setup.py build"),
                Some("pytest"),
            ),
            CheckKind::Markers,
            10,
        ),
    ]
}

fn jvm_and_dotnet_specs() -> Vec<EcosystemSpec> {
    vec![
        spec(
            "gradle",
            "jvm",
            &["build.gradle", "build.gradle.kts"],
            commands(None, Some("./gradlew build"), Some("./gradlew test")),
            CheckKind::Markers,
            10,
        ),
        spec(
            "maven",
            "jvm",
            &["pom.xml"],
            commands(None, Some("mvn package"), Some("mvn test")),
            CheckKind::Markers,
            5,
        ),
        spec(
            "dotnet",
            "dotnet",
            &[],
            commands(
                Some("dotnet restore"),
                Some("dotnet build"),
                Some("dotnet test"),
            ),
            CheckKind::Dotnet,
            0,
        ),
    ]
}

fn dart_specs() -> Vec<EcosystemSpec> {
    vec![
        spec(
            "flutter",
            "dart",
            &[],
            commands(
                Some("flutter pub get"),
                Some("flutter build"),
                Some("flutter test"),
            ),
            CheckKind::Flutter,
            10,
        ),
        spec(
            "dart",
            "dart",
            &["pubspec.yaml"],
            commands(Some("dart pub get"), None, Some("dart test")),
            CheckKind::Dart,
            5,
        ),
        spec(
            "deno",
            "deno",
            &["deno.json", "deno.jsonc"],
            commands(None, None, Some("deno test")),
            CheckKind::Markers,
            0,
        ),
    ]
}

fn server_runtime_specs() -> Vec<EcosystemSpec> {
    vec![
        spec(
            "ruby",
            "ruby",
            &["Gemfile"],
            commands(Some("bundle install"), None, Some("bundle exec rake test")),
            CheckKind::Markers,
            0,
        ),
        spec(
            "elixir",
            "elixir",
            &["mix.exs"],
            commands(Some("mix deps.get"), Some("mix compile"), Some("mix test")),
            CheckKind::Markers,
            0,
        ),
        spec(
            "php",
            "php",
            &["composer.json"],
            commands(Some("composer install"), None, Some("./vendor/bin/phpunit")),
            CheckKind::Markers,
            0,
        ),
    ]
}

const fn commands(
    install: Option<&'static str>,
    build: Option<&'static str>,
    test: Option<&'static str>,
) -> ValidationCommands {
    ValidationCommands::new(install, build, test, None)
}

const fn spec(
    name: &'static str,
    family: &'static str,
    markers: &'static [&'static str],
    commands: ValidationCommands,
    check: CheckKind,
    priority: i32,
) -> EcosystemSpec {
    EcosystemSpec {
        name,
        family,
        markers,
        commands,
        check,
        priority,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{build_config_data, detect_project, run_init};
    use crate::identity::RuntimeMode;

    #[test]
    fn detects_rust_project_and_generates_python_compatible_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .expect("cargo");

        let info = detect_project(temp.path());
        assert_eq!(info.ecosystems[0].name, "rust");
        assert_eq!(info.platforms, ["macos", "linux", "windows"]);

        let config = build_config_data(&info, "demo");
        let json = serde_json::to_value(&config).expect("json");
        assert_eq!(json["project"]["name"], "demo");
        assert_eq!(json["project"]["type"], "rust");
        assert_eq!(json["validation"]["default"]["build"], "cargo build");
        assert_eq!(json["validation"]["default"]["test"], "cargo test");
        assert_eq!(json["targets"]["mac"]["backend"], "local");
        assert_eq!(json["targets"]["ubuntu"]["backend"], "cloud");
        assert_eq!(json["targets"]["windows"]["platform"], "windows-x64");
    }

    #[test]
    fn family_detection_keeps_highest_priority_marker() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("package.json"), "{}").expect("package");
        std::fs::write(temp.path().join("pnpm-lock.yaml"), "").expect("lock");
        std::fs::write(temp.path().join("requirements.txt"), "").expect("requirements");

        let info = detect_project(temp.path());
        let names: Vec<&str> = info
            .ecosystems
            .iter()
            .map(|ecosystem| ecosystem.name)
            .collect();
        assert_eq!(names, ["node-pnpm", "python-pip"]);
    }

    #[test]
    fn apple_only_projects_target_macos_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("Package.swift"), "// swift").expect("package");

        let info = detect_project(temp.path());

        assert_eq!(info.ecosystems[0].name, "swift-spm");
        assert_eq!(info.platforms, ["macos"]);
    }

    #[test]
    fn unknown_projects_use_macos_and_linux_defaults() {
        let temp = tempfile::tempdir().expect("tempdir");

        let info = detect_project(temp.path());
        let config = build_config_data(&info, "unknown");
        let json = serde_json::to_value(&config).expect("json");

        assert_eq!(
            json["project"]["platforms"],
            serde_json::json!(["macos", "linux"])
        );
        assert_eq!(json["targets"]["mac"]["platform"], "macos-arm64");
        assert_eq!(json["targets"]["ubuntu"]["platform"], "linux-x64");
        assert!(json["targets"].get("windows").is_none());
    }

    #[test]
    fn run_init_writes_project_config_and_isolated_gitignore() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .expect("cargo");

        let result = run_init(temp.path(), RuntimeMode::Isolated).expect("init");

        assert!(result.project_dir.ends_with(".shipyard"));
        assert!(result.config_path.exists());
        let config_text = std::fs::read_to_string(&result.config_path).expect("config");
        assert!(config_text.contains("[project]"));
        assert!(config_text.contains("type = \"rust\""));
        let parsed: Value =
            serde_json::to_value(config_text.parse::<toml::Table>().expect("toml")).expect("json");
        assert_eq!(parsed["targets"]["ubuntu"]["backend"], "cloud");
        let gitignore = std::fs::read_to_string(&result.gitignore_path).expect("gitignore");
        assert!(gitignore.contains(".shipyard-dev.local/"));
    }

    #[test]
    fn run_init_shipyard_mode_uses_python_local_overlay_name() {
        let temp = tempfile::tempdir().expect("tempdir");

        run_init(temp.path(), RuntimeMode::Shipyard).expect("init");

        let gitignore = std::fs::read_to_string(temp.path().join(".gitignore")).expect("gitignore");
        assert_eq!(gitignore, ".shipyard.local/\n");
    }
}
