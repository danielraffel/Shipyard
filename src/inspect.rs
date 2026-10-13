//! `shipyard inspect`: read a repository's CI and say what to change.
//!
//! Inspect is an advisor. It runs only when asked, reads GitHub and the
//! repository's own files, and never sits on the path of `ship`, `pr`, the
//! guards, the daemon or any gate step. Every check declares which facts it
//! applies to, and those facts come from [`profile`]: what the project
//! actually builds with, pins, runs on and tests with, each with the file
//! line or API row it was read from.

pub mod profile;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::LazyLock;

    use regex::Regex;

    /// Every Rust source under `dir`, recursively.
    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    static STRING: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""(?:[^"\\]|\\.)*""#).expect("string regex"));
    /// A path through the crate root, `super` or `self` that names the
    /// `inspect` module, directly or inside a grouped import.
    static ROOTED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\b(?:crate|shipyard|super|self)\s*::\s*(?:\{[^;]*?\b)?inspect\b\s*(?:::|[,};]|as\b)",
        )
        .expect("rooted regex")
    });
    /// A bare `inspect::` path; `inspect_cmd::` and `.inspect(` do not match.
    static BARE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\binspect\s*::").expect("bare regex"));

    /// Whether Rust source `text` names the `inspect` module, ignoring
    /// comments and string literals.
    fn names_inspect_module(text: &str) -> bool {
        let code: String = text
            .lines()
            .map(|line| {
                let line = STRING.replace_all(line, "\"\"");
                let line = line.split("//").next().unwrap_or("").to_owned();
                line + "\n"
            })
            .collect();
        ROOTED.is_match(&code) || BARE.is_match(&code)
    }

    #[test]
    fn the_matcher_sees_every_import_form_and_nothing_else() {
        for flagged in [
            "use crate::inspect;",
            "use crate::inspect::profile::{self, gather};",
            "use crate::{config::LoadedConfig, inspect::profile};",
            "use super::{cloud, inspect};",
            "use crate::inspect as probe;",
            "use crate::{config as c, inspect as probe};",
            "use crate::{\n    config,\n    inspect,\n};",
            "use shipyard::inspect::profile;",
            "let x = crate::inspect::profile::SCHEMA;",
            "let x = inspect::profile::SCHEMA;",
        ] {
            assert!(names_inspect_module(flagged), "must flag: {flagged}");
        }
        for clean in [
            "mod inspect_cmd;",
            "return inspect_cmd::inspect_command(cwd);",
            "let v: Vec<_> = iter.inspect(|x| dbg!(x)).collect();",
            "// use crate::inspect;",
            r#"let s = "crate::inspect";"#,
            "use crate::{config, inspector};",
        ] {
            assert!(!names_inspect_module(clean), "must not flag: {clean}");
        }
    }

    /// Inspect stays off every hot path: outside its own modules, only the
    /// `inspect` command handler names it, so `ship`, `pr`, the guards, the
    /// daemon and start-up never reach its code. Wall time on the shared
    /// hosts tracks load, so this pins the structure that keeps start-up
    /// unchanged rather than a timing that would flake.
    #[test]
    fn only_the_inspect_command_reaches_inspect() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let src = root.join("src");
        let mut sources = Vec::new();
        rust_sources(&src, &mut sources);
        assert!(
            sources.len() > 100,
            "control: the walk must see the crate, saw {} files",
            sources.len()
        );
        let allowed = |relative: &str| relative == "inspect.rs" || relative.starts_with("inspect/");
        let mut reached_from = Vec::new();
        let mut handler_seen = false;
        for path in sources {
            let relative = path
                .strip_prefix(&src)
                .expect("under src")
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&path).expect("read source");
            let names_inspect = names_inspect_module(&text);
            if relative == "app/inspect_cmd.rs" {
                handler_seen = names_inspect;
                continue;
            }
            if allowed(&relative) {
                continue;
            }
            // The handler is dispatched from `app.rs` alone.
            let names_handler = text.contains("inspect_cmd::") && relative != "app.rs";
            if names_inspect || names_handler {
                reached_from.push(relative);
            }
        }
        assert!(
            handler_seen,
            "control: the command handler must name the module this test looks for"
        );
        assert!(
            reached_from.is_empty(),
            "inspect is reachable from outside its command: {reached_from:?}"
        );
    }
}
