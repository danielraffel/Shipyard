//! Record GitHub answers and replay them offline.
//!
//! A recording is a directory of JSON files, one per request:
//! `{"argv": [...], "response": "..."}`. [`FixtureReader`] serves a request
//! only when a recorded argv matches it exactly; anything else is an error,
//! never an empty answer, so a fixture that drifted from the reader's
//! requests fails loudly.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

/// Serves recorded answers by exact argv.
#[derive(Debug, Default)]
pub struct FixtureReader {
    answers: BTreeMap<Vec<String>, String>,
    served: Mutex<Vec<Vec<String>>>,
}

impl FixtureReader {
    /// Load every `*.json` file under `dir`.
    ///
    /// # Errors
    /// When the directory or a file cannot be read or parsed.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let mut answers = BTreeMap::new();
        let mut paths: Vec<PathBuf> = fs::read_dir(dir)
            .map_err(|error| format!("read {}: {error}", dir.display()))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect();
        paths.sort();
        for path in paths {
            let raw = fs::read_to_string(&path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            let value: Value = serde_json::from_str(&raw)
                .map_err(|error| format!("parse {}: {error}", path.display()))?;
            let (Some(argv), Some(response)) = (
                value.get("argv").and_then(Value::as_array),
                value.get("response").and_then(Value::as_str),
            ) else {
                continue;
            };
            let argv: Vec<String> = argv
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            answers.insert(argv, response.to_owned());
        }
        Ok(Self {
            answers,
            served: Mutex::new(Vec::new()),
        })
    }

    /// Answer one request.
    ///
    /// # Errors
    /// When no recorded argv matches exactly.
    pub fn read(&self, argv: &[String]) -> Result<String, String> {
        if let Ok(mut served) = self.served.lock() {
            served.push(argv.to_vec());
        }
        self.answers.get(argv).cloned().ok_or_else(|| {
            let shown: Vec<&str> = argv.iter().map(String::as_str).take(6).collect();
            format!("no fixture for gh {}", shown.join(" "))
        })
    }

    /// Every argv requested so far.
    #[must_use]
    pub fn served(&self) -> Vec<Vec<String>> {
        self.served
            .lock()
            .map(|served| served.clone())
            .unwrap_or_default()
    }
}

/// Writes every request and answer to a directory.
#[derive(Debug)]
pub struct Recorder {
    dir: PathBuf,
    next: AtomicU64,
}

impl Recorder {
    /// Record into `dir` (created if needed).
    ///
    /// # Errors
    /// When the directory cannot be created.
    pub fn new(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            next: AtomicU64::new(1),
        })
    }

    /// Record one successful answer.
    pub fn record(&self, argv: &[String], response: &str) {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        let body = json!({"argv": argv, "response": response});
        let _ = fs::write(
            self.dir.join(format!("{index:05}.json")),
            serde_json::to_string_pretty(&body).unwrap_or_default(),
        );
    }
}
