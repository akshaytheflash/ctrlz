#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Output};

use assert_cmd::Command;
use tempfile::TempDir;

pub struct Sandbox {
    pub repo: TempDir,
    pub store: TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        Self {
            repo: TempDir::new().expect("repo tempdir"),
            store: TempDir::new().expect("store tempdir"),
        }
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        let mut path = self.repo.path().to_path_buf();
        for part in relative.split('/') {
            path.push(part);
        }
        path
    }

    pub fn write(&self, relative: &str, bytes: impl AsRef<[u8]>) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(long(parent)).expect("create parent");
        }
        fs::write(long(&path), bytes).unwrap_or_else(|error| {
            panic!(
                "write {} as {}: {error}",
                path.display(),
                long(&path).display()
            )
        });
    }

    pub fn read(&self, relative: &str) -> Vec<u8> {
        fs::read(long(&self.path(relative))).expect("read fixture")
    }

    pub fn exists(&self, relative: &str) -> bool {
        self.path(relative).exists()
    }

    pub fn remove(&self, relative: &str) {
        fs::remove_file(long(&self.path(relative))).expect("remove fixture");
    }

    pub fn cmd(&self) -> Command {
        let mut command = Command::cargo_bin("ctrlz").expect("ctrlz binary");
        command
            .current_dir(self.repo.path())
            .env("CTRLZ_HOME", self.store.path());
        command
    }

    pub fn output(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("run ctrlz")
    }

    pub fn ok(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "ctrlz {:?} failed\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    pub fn fail(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            !output.status.success(),
            "ctrlz {:?} unexpectedly succeeded\n{}",
            args,
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    pub fn init(&self) {
        self.ok(&["init"]);
    }

    pub fn mark(&self, label: &str) {
        self.ok(&["mark", "--label", label, "--agent", "test"]);
    }

    pub fn turns(&self) -> usize {
        let output = self.ok(&["list", "--limit", "1000"]);
        output
            .lines()
            .skip(1)
            .filter(|line| !line.trim().is_empty())
            .count()
    }
}

pub fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(long(dir)) else {
        return;
    };
    for entry in entries {
        let entry = entry.expect("read dir entry");
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let meta = fs::symlink_metadata(long(&path)).expect("stat");
        if meta.is_dir() {
            walk(root, &path, files);
        } else if meta.is_file() {
            let relative = path
                .strip_prefix(long(root))
                .expect("relative path")
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(relative, fs::read(long(&path)).expect("read tree file"));
        }
    }
}

pub fn long(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy().replace('/', r"\");
        if text.starts_with(r"\\?\") {
            return PathBuf::from(text);
        }
        if let Some(rest) = text.strip_prefix(r"\\") {
            return PathBuf::from(format!(r"\\?\UNC\{rest}"));
        }
        PathBuf::from(format!(r"\\?\{text}"))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

pub fn git(repo: &Path, args: &[&str]) -> String {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}
