mod common;

use std::fs;
#[cfg(windows)]
use std::process::Command as ProcessCommand;
use std::time::UNIX_EPOCH;

use common::{git, long, tree, Sandbox};
use tempfile::TempDir;

#[test]
fn undo_restores_the_exact_tree() {
    let sandbox = Sandbox::new();
    sandbox.write("src/main.rs", b"fn main() {}\n");
    sandbox.write("docs/readme.md", b"hello\n");
    sandbox.write("delete-me.txt", b"gone\n");
    sandbox.init();
    sandbox.mark("pre");
    let before = tree(sandbox.repo.path());

    sandbox.write("src/main.rs", b"broken\n");
    sandbox.write("src/new.rs", b"new file\n");
    sandbox.remove("delete-me.txt");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);

    sandbox.ok(&["undo"]);
    assert_eq!(tree(sandbox.repo.path()), before);
}

#[test]
fn undo_removes_files_created_by_the_agent() {
    let sandbox = Sandbox::new();
    sandbox.write("keep.txt", b"keep\n");
    sandbox.write(".gitignore", b"*.tmp\n");
    sandbox.init();
    sandbox.mark("pre");

    sandbox.write("scratch/agent.txt", b"agent\n");
    sandbox.write("new/deep/file.txt", b"deep\n");
    sandbox.write("noise.tmp", b"ignored\n");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);

    sandbox.ok(&["undo"]);
    assert!(!sandbox.exists("scratch/agent.txt"));
    assert!(!sandbox.exists("new/deep/file.txt"));
    assert_eq!(sandbox.read("keep.txt"), b"keep\n");
}

#[test]
fn undo_restores_a_deleted_dotenv_and_binary() {
    let sandbox = Sandbox::new();
    let binary: Vec<u8> = (0..2_000_000).map(|value| (value % 251) as u8).collect();
    sandbox.write(".env", b"API_KEY=secret\n");
    sandbox.write("assets/blob.bin", &binary);
    sandbox.init();
    sandbox.mark("pre");
    let env_mtime = modified_ns(&sandbox.path(".env"));

    sandbox.remove(".env");
    sandbox.remove("assets/blob.bin");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);
    sandbox.ok(&["undo"]);

    assert_eq!(sandbox.read(".env"), b"API_KEY=secret\n");
    assert_eq!(sandbox.read("assets/blob.bin"), binary);
    assert_eq!(modified_ns(&sandbox.path(".env")), env_mtime);
}

#[test]
fn ignored_directories_are_never_touched() {
    let sandbox = Sandbox::new();
    sandbox.write("src/app.rs", b"v1\n");
    sandbox.write("node_modules/pkg/index.js", b"old module\n");
    sandbox.write("target/debug/out.bin", b"old target\n");
    sandbox.init();
    sandbox.mark("pre");

    sandbox.write("src/app.rs", b"v2\n");
    sandbox.write("node_modules/pkg/index.js", b"agent module\n");
    sandbox.write("target/debug/out.bin", b"agent target\n");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);
    sandbox.ok(&["undo"]);

    assert_eq!(sandbox.read("src/app.rs"), b"v1\n");
    assert_eq!(sandbox.read("node_modules/pkg/index.js"), b"agent module\n");
    assert_eq!(sandbox.read("target/debug/out.bin"), b"agent target\n");
}

#[test]
fn restoring_twice_is_a_no_op() {
    let sandbox = Sandbox::new();
    sandbox.write("a.txt", b"one\n");
    sandbox.init();
    sandbox.mark("pre");
    sandbox.write("a.txt", b"broken\n");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);

    sandbox.ok(&["back", "1"]);
    let after_first = tree(sandbox.repo.path());
    let turns_after_first = sandbox.turns();

    let output = sandbox.ok(&["back", "1"]);
    assert!(output.contains("already matches"));
    assert_eq!(tree(sandbox.repo.path()), after_first);
    assert_eq!(sandbox.turns(), turns_after_first);
}

#[test]
fn the_pre_restore_snapshot_can_undo_the_undo() {
    let sandbox = Sandbox::new();
    sandbox.write("a.txt", b"one\n");
    sandbox.init();
    sandbox.mark("pre");
    sandbox.write("a.txt", b"broken\n");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);

    sandbox.ok(&["undo"]);
    assert_eq!(sandbox.read("a.txt"), b"one\n");

    sandbox.ok(&["undo"]);
    assert_eq!(sandbox.read("a.txt"), b"broken\n");
}

#[test]
fn git_status_is_unchanged_by_undo() {
    let sandbox = Sandbox::new();
    git(sandbox.repo.path(), &["init", "-q"]);
    git(sandbox.repo.path(), &["config", "user.name", "ctrlz test"]);
    git(
        sandbox.repo.path(),
        &["config", "user.email", "ctrlz@example.invalid"],
    );
    sandbox.write("tracked.txt", b"v1\n");
    sandbox.write("other.txt", b"v1\n");
    git(sandbox.repo.path(), &["add", "."]);
    git(sandbox.repo.path(), &["commit", "-qm", "initial"]);

    sandbox.write("tracked.txt", b"staged change\n");
    git(sandbox.repo.path(), &["add", "tracked.txt"]);
    let before = git(sandbox.repo.path(), &["status", "--porcelain"]);

    sandbox.init();
    sandbox.mark("pre");
    sandbox.write("other.txt", b"agent change\n");
    sandbox.write("new.txt", b"new\n");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);
    sandbox.ok(&["undo"]);

    let after = git(sandbox.repo.path(), &["status", "--porcelain"]);
    assert_eq!(after, before);
    assert_eq!(sandbox.read("tracked.txt"), b"staged change\n");
}

#[test]
fn handles_unicode_spaces_crlf_and_long_paths() {
    let sandbox = Sandbox::new();
    let long_path = format!(
        "{}/{}/{}.txt",
        "a".repeat(100),
        "b".repeat(100),
        "c".repeat(80)
    );
    sandbox.write("naïve 文件.txt", b"unicode\n");
    sandbox.write("dir with spaces/file name.txt", b"spaces\n");
    sandbox.write("crlf.txt", b"one\r\ntwo\r\n");
    sandbox.write(&long_path, b"long\n");
    sandbox.init();
    sandbox.mark("pre");

    sandbox.write("naïve 文件.txt", b"changed\n");
    sandbox.write("dir with spaces/file name.txt", b"changed\n");
    sandbox.write("crlf.txt", b"changed\r\n");
    sandbox.write(&long_path, b"changed\n");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);
    sandbox.ok(&["undo"]);

    assert_eq!(sandbox.read("naïve 文件.txt"), b"unicode\n");
    assert_eq!(sandbox.read("dir with spaces/file name.txt"), b"spaces\n");
    assert_eq!(sandbox.read("crlf.txt"), b"one\r\ntwo\r\n");
    assert_eq!(sandbox.read(&long_path), b"long\n");
}

#[test]
fn never_writes_through_a_link_outside_the_root() {
    let sandbox = Sandbox::new();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("secret.txt"), b"outside\n").unwrap();
    sandbox.write("linkdir/target.txt", b"inside\n");
    sandbox.write("other.txt", b"ok\n");
    sandbox.init();
    sandbox.mark("pre");

    fs::remove_dir_all(long(&sandbox.path("linkdir"))).unwrap();
    if !make_dir_link(&sandbox.path("linkdir"), outside.path()) {
        eprintln!("skipping: this machine does not allow directory links");
        return;
    }

    sandbox.write("other.txt", b"broken\n");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);
    sandbox.ok(&["undo"]);

    assert_eq!(
        fs::read(outside.path().join("secret.txt")).unwrap(),
        b"outside\n"
    );
    assert!(!outside.path().join("target.txt").exists());
    assert_eq!(sandbox.read("other.txt"), b"ok\n");
}

#[test]
fn max_file_skips_are_reported_and_never_deleted() {
    let sandbox = Sandbox::new();
    let big = vec![7u8; 2048];
    sandbox.write("small.txt", b"small\n");
    sandbox.write("big.bin", &big);
    sandbox.init();
    sandbox.ok(&[
        "mark",
        "--label",
        "pre",
        "--agent",
        "test",
        "--max-file",
        "1K",
    ]);

    let show = sandbox.ok(&["show", "1"]);
    assert!(show.contains("big.bin"));
    assert!(show.contains("--max-file"));

    let changed = vec![9u8; 2048];
    sandbox.write("big.bin", &changed);
    sandbox.write("small.txt", b"broken\n");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);
    sandbox.ok(&["undo"]);

    assert_eq!(sandbox.read("big.bin"), changed);
    assert_eq!(sandbox.read("small.txt"), b"small\n");
}

#[test]
fn no_change_marks_reuse_the_latest_turn() {
    let sandbox = Sandbox::new();
    sandbox.write("a.txt", b"a\n");
    sandbox.init();
    sandbox.mark("before");
    assert_eq!(sandbox.turns(), 1);

    sandbox.mark("after");
    assert_eq!(sandbox.turns(), 1);
    let list = sandbox.ok(&["list"]);
    assert!(list.contains("after"));
}

#[test]
fn show_and_diff_report_text_and_binary_changes() {
    let sandbox = Sandbox::new();
    sandbox.write("text.txt", b"one\n");
    sandbox.write("bin.dat", b"\x00\x01\x02");
    sandbox.init();
    sandbox.mark("pre");

    sandbox.write("text.txt", b"two\n");
    sandbox.write("bin.dat", b"\x00\x09\x09");
    sandbox.write("new.txt", b"new\n");
    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);

    let show = sandbox.ok(&["show", "2"]);
    assert!(show.contains("~ text.txt"));
    assert!(show.contains("~ bin.dat"));
    assert!(show.contains("+ new.txt"));

    let diff = sandbox.ok(&["diff", "2"]);
    assert!(diff.contains("-one"));
    assert!(diff.contains("+two"));
    assert!(diff.contains("binary bin.dat"));
    assert!(diff.contains("+new"));
}

#[test]
#[allow(clippy::permissions_set_readonly_false)]
fn restore_preserves_mode_bits() {
    let sandbox = Sandbox::new();
    sandbox.write("script.sh", b"#!/bin/sh\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            long(&sandbox.path("script.sh")),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    #[cfg(windows)]
    {
        let path = long(&sandbox.path("script.sh"));
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
    }

    sandbox.init();
    sandbox.mark("pre");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            long(&sandbox.path("script.sh")),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }
    #[cfg(windows)]
    {
        let path = long(&sandbox.path("script.sh"));
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&path, permissions).unwrap();
    }

    sandbox.ok(&["mark", "--label", "stop", "--agent", "agent"]);
    sandbox.ok(&["undo"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(long(&sandbox.path("script.sh")))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }
    #[cfg(windows)]
    {
        assert!(fs::metadata(long(&sandbox.path("script.sh")))
            .unwrap()
            .permissions()
            .readonly());
    }
}

#[test]
fn claude_hook_records_one_turn_and_always_exits_zero() {
    let sandbox = Sandbox::new();
    sandbox.write("a.txt", b"a\n");
    sandbox.init();
    let payload = serde_json::json!({
        "session_id": "session-123",
        "transcript_path": "C:\\tmp\\transcript.jsonl",
        "cwd": sandbox.repo.path().to_string_lossy(),
        "hook_event_name": "UserPromptSubmit"
    })
    .to_string();

    let mut command = sandbox.cmd();
    command
        .args(["hook", "claude"])
        .write_stdin(payload.clone());
    let output = command.output().unwrap();
    assert!(output.status.success());
    assert_eq!(sandbox.turns(), 1);
    let list = sandbox.ok(&["list"]);
    assert!(list.contains("claude"));
    let show = sandbox.ok(&["show", "1"]);
    assert!(show.contains("session-123"));

    let not_a_store = sandbox.store.path().join("this-is-a-file");
    fs::write(&not_a_store, b"x").unwrap();
    let mut broken = sandbox.cmd();
    broken
        .env("CTRLZ_HOME", &not_a_store)
        .args(["hook", "claude"])
        .write_stdin(payload);
    let output = broken.output().unwrap();
    assert!(output.status.success());
}

fn modified_ns(path: &std::path::Path) -> u128 {
    fs::metadata(long(path))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn make_dir_link(link: &std::path::Path, target: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return true;
        }
        ProcessCommand::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}
