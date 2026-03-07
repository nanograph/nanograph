#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

const CLEARED_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "NANOGRAPH_EMBEDDINGS_MOCK",
    "NANOGRAPH_EMBED_MODEL",
    "NANOGRAPH_EMBED_BATCH_SIZE",
    "NANOGRAPH_EMBED_CHUNK_CHARS",
    "NANOGRAPH_EMBED_CHUNK_OVERLAP_CHARS",
];

#[derive(Clone, Copy, Debug)]
pub enum ExampleProject {
    Starwars,
    Revops,
}

impl ExampleProject {
    fn dir_name(self) -> &'static str {
        match self {
            Self::Starwars => "starwars",
            Self::Revops => "revops",
        }
    }

    pub fn schema_file(self) -> &'static str {
        match self {
            Self::Starwars => "starwars.pg",
            Self::Revops => "revops.pg",
        }
    }

    pub fn data_file(self) -> &'static str {
        match self {
            Self::Starwars => "starwars.jsonl",
            Self::Revops => "revops.jsonl",
        }
    }

    pub fn query_file(self) -> &'static str {
        match self {
            Self::Starwars => "starwars.gq",
            Self::Revops => "revops.gq",
        }
    }

    pub fn default_db(self) -> &'static str {
        match self {
            Self::Starwars => "starwars.nano",
            Self::Revops => "omni.nano",
        }
    }
}

pub struct ExampleWorkspace {
    temp: TempDir,
    example: ExampleProject,
}

pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
}

impl ExampleWorkspace {
    pub fn copy(example: ExampleProject) -> Self {
        let temp = TempDir::new().unwrap();
        let src = repo_root().join("examples").join(example.dir_name());
        copy_dir_contents(&src, temp.path());
        Self { temp, example }
    }

    pub fn root(&self) -> &Path {
        self.temp.path()
    }

    pub fn file(&self, rel_path: &str) -> PathBuf {
        self.root().join(rel_path)
    }

    pub fn db_path(&self) -> PathBuf {
        self.file(self.example.default_db())
    }

    pub fn write_file(&self, rel_path: &str, contents: &str) {
        let path = self.file(rel_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    pub fn delete_file(&self, rel_path: &str) {
        let path = self.file(rel_path);
        if path.is_file() {
            fs::remove_file(path).unwrap();
        } else if path.is_dir() {
            fs::remove_dir_all(path).unwrap();
        }
    }

    pub fn append_file(&self, rel_path: &str, contents: &str) {
        let path = self.file(rel_path);
        let mut existing = fs::read_to_string(&path).unwrap_or_default();
        existing.push_str(contents);
        self.write_file(rel_path, &existing);
    }

    pub fn read_file(&self, rel_path: &str) -> String {
        fs::read_to_string(self.file(rel_path)).unwrap()
    }

    pub fn run_ok(&self, args: &[&str]) -> CommandResult {
        let output = self.command(args).output().unwrap();
        if !output.status.success() {
            panic!(
                "nanograph command failed\nargs: {:?}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
                args,
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        CommandResult {
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }

    pub fn run_fail(&self, args: &[&str]) -> CommandResult {
        let output = self.command(args).output().unwrap();
        if output.status.success() {
            panic!(
                "nanograph command unexpectedly succeeded\nargs: {:?}\nstdout:\n{}\nstderr:\n{}",
                args,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        CommandResult {
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }

    pub fn json_value(&self, args: &[&str]) -> Value {
        parse_json_value(&self.run_ok(args).stdout)
    }

    pub fn json_rows(&self, args: &[&str]) -> Vec<Value> {
        parse_json_array(&self.run_ok(args).stdout)
    }

    pub fn jsonl_rows(&self, args: &[&str]) -> Vec<Value> {
        self.run_ok(args)
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    pub fn init(&self) {
        let value = self.json_value(&["--json", "init"]);
        assert_eq!(value["status"], "ok");
    }

    pub fn load(&self) {
        let value = self.json_value(&[
            "--json",
            "load",
            "--data",
            self.example.data_file(),
            "--mode",
            "overwrite",
        ]);
        assert_eq!(value["status"], "ok");
    }

    pub fn check(&self) {
        let value = self.json_value(&["--json", "check", "--query", self.example.query_file()]);
        assert_eq!(value["status"], "ok");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nanograph"));
        command.current_dir(self.root());
        for env_key in CLEARED_ENV_VARS {
            command.env_remove(env_key);
        }
        command.args(args);
        command
    }
}

pub fn parse_json_value(output: &str) -> Value {
    let trimmed = output.trim();
    serde_json::from_str(trimmed).unwrap_or_else(|_| {
        let start = trimmed
            .find(['{', '['])
            .unwrap_or_else(|| panic!("output did not contain JSON payload:\n{}", output));
        let payload = &trimmed[start..];
        serde_json::from_str(payload)
            .unwrap_or_else(|err| panic!("failed to parse JSON output: {}\n{}", err, output))
    })
}

pub fn parse_json_array(output: &str) -> Vec<Value> {
    let value = parse_json_value(output);
    value.as_array().cloned().unwrap_or_default()
}

pub fn scalar_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(inner) => inner.to_string(),
        Value::Number(inner) => inner.to_string(),
        Value::String(inner) => inner.clone(),
        other => other.to_string(),
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn copy_dir_contents(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let source_path = entry.path();
        let target_path = dst.join(entry.file_name());
        let file_type = entry.file_type().unwrap();
        if file_type.is_dir() {
            copy_dir_contents(&source_path, &target_path);
        } else if file_type.is_file() {
            fs::copy(&source_path, &target_path).unwrap();
        }
    }
}
