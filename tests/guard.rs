mod common;

use std::fs;

use common::Sandbox;
use serde_json::Value;

fn init(sandbox: &Sandbox) {
    let root = sandbox.repo.path().to_string_lossy().to_string();
    sandbox.ok(&["guard", "init", "--root", &root]);
}

fn flags(sandbox: &Sandbox) -> Vec<Value> {
    let text = sandbox.ok(&["flags", "--json", "--since", "24h"]);
    serde_json::from_str(&text).expect("flags --json must print a JSON array")
}

#[test]
fn a_deleted_file_is_flagged_and_restored_byte_for_byte() {
    let sandbox = Sandbox::new();
    sandbox.write("notes.txt", "important");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);

    sandbox.remove("notes.txt");
    sandbox.ok(&["guard", "check"]);

    let listed = flags(&sandbox);
    assert_eq!(listed.len(), 1, "one deletion, one flag");
    assert_eq!(listed[0]["kind"], "deleted");
    assert_eq!(listed[0]["severity"], "warn");
    assert_eq!(listed[0]["recoverable"], true);
    assert!(listed[0]["path"].as_str().unwrap().ends_with("notes.txt"));

    sandbox.ok(&["recover"]);
    assert_eq!(sandbox.read("notes.txt"), b"important");
}

#[test]
fn binary_content_round_trips() {
    let sandbox = Sandbox::new();
    let payload = vec![0u8, 1, 2, 3, 255, 0, 128];
    sandbox.write("blob.bin", &payload);
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);
    sandbox.remove("blob.bin");
    sandbox.ok(&["guard", "check"]);
    sandbox.ok(&["recover"]);
    assert_eq!(sandbox.read("blob.bin"), payload);
}

#[test]
fn deleting_a_protected_file_is_critical() {
    let sandbox = Sandbox::new();
    sandbox.write(".env", "SECRET=1");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);
    sandbox.remove(".env");
    sandbox.ok(&["guard", "check"]);

    let listed = flags(&sandbox);
    assert_eq!(listed[0]["severity"], "critical");
    sandbox.ok(&["recover"]);
    assert_eq!(sandbox.read(".env"), b"SECRET=1");
}

#[test]
fn a_deletion_that_was_never_captured_is_reported_as_lost() {
    let sandbox = Sandbox::new();
    sandbox.write("ghost.txt", "vanished");
    init(&sandbox);
    // Deliberately no `guard scan`: the baseline is what makes recovery
    // possible, and the guard must say so rather than pretend. This is the
    // path the watcher takes when it sees a delete for an unknown file.
    let ghost = sandbox.path("ghost.txt").to_string_lossy().to_string();
    sandbox.remove("ghost.txt");
    sandbox.ok(&["guard", "check", "--path", &ghost]);

    let listed = flags(&sandbox);
    assert_eq!(listed[0]["kind"], "deleted");
    assert_eq!(listed[0]["recoverable"], false);
    assert_eq!(listed[0]["severity"], "critical");

    let id = listed[0]["id"].as_i64().unwrap().to_string();
    let error = sandbox.fail(&["recover", &id]);
    assert!(
        error.contains("never captured"),
        "expected an explanation, got: {error}"
    );
}

#[test]
fn a_gitignored_dotfile_is_still_captured() {
    let sandbox = Sandbox::new();
    sandbox.write(".gitignore", "*\n");
    sandbox.write(".env", "SECRET=1");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);
    sandbox.remove(".env");
    sandbox.ok(&["guard", "check"]);
    sandbox.ok(&["recover"]);
    assert_eq!(sandbox.read(".env"), b"SECRET=1");
}

#[test]
fn an_edit_during_an_agent_turn_is_attributed() {
    let sandbox = Sandbox::new();
    sandbox.write("src/app.rs", "fn main() {}\n");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);

    sandbox.ok(&[
        "guard",
        "activity",
        "--agent",
        "test-agent",
        "--session",
        "s1",
    ]);
    sandbox.write("src/app.rs", "fn main() { evil(); }\n");
    sandbox.ok(&["guard", "check"]);

    let listed = flags(&sandbox);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["kind"], "modified");
    assert_eq!(listed[0]["agent"], "test-agent");
    assert_eq!(listed[0]["session"], "s1");
    assert_eq!(listed[0]["confidence"], "window");
}

#[test]
fn an_edit_without_an_agent_marker_is_not_a_flag() {
    let sandbox = Sandbox::new();
    sandbox.write("src/app.rs", "fn main() {}\n");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);

    sandbox.write("src/app.rs", "fn main() { changed(); }\n");
    sandbox.ok(&["guard", "check"]);

    assert!(
        flags(&sandbox).is_empty(),
        "edits outside an agent turn should update the shadow index, not the ledger"
    );
}

#[test]
fn checking_twice_does_not_duplicate_flags() {
    let sandbox = Sandbox::new();
    sandbox.write("notes.txt", "important");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);
    sandbox.remove("notes.txt");

    sandbox.ok(&["guard", "check"]);
    let first = flags(&sandbox).len();
    sandbox.ok(&["guard", "check"]);
    let second = flags(&sandbox).len();

    assert_eq!(first, 1);
    assert_eq!(second, first);
}

#[test]
fn a_rename_is_not_reported_as_a_deletion() {
    let sandbox = Sandbox::new();
    sandbox.write("a.txt", "same bytes");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);

    fs::rename(sandbox.path("a.txt"), sandbox.path("b.txt")).unwrap();
    sandbox.ok(&["guard", "check"]);

    let listed = flags(&sandbox);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["kind"], "renamed");
    assert_eq!(listed[0]["severity"], "info");
    assert!(listed[0]["note"].as_str().unwrap().contains("b.txt"));
}

#[test]
fn a_window_restore_brings_back_several_files() {
    let sandbox = Sandbox::new();
    sandbox.write("one.txt", "first");
    sandbox.write("two.txt", "second");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);

    sandbox.remove("one.txt");
    sandbox.remove("two.txt");
    sandbox.ok(&["guard", "check"]);

    sandbox.ok(&["recover", "--since", "1h"]);
    assert_eq!(sandbox.read("one.txt"), b"first");
    assert_eq!(sandbox.read("two.txt"), b"second");
}

#[test]
fn a_dry_run_changes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.write("notes.txt", "important");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);
    sandbox.remove("notes.txt");
    sandbox.ok(&["guard", "check"]);

    sandbox.ok(&["recover", "--dry-run"]);
    assert!(!sandbox.exists("notes.txt"));

    sandbox.ok(&["recover"]);
    assert_eq!(sandbox.read("notes.txt"), b"important");
}

#[test]
fn build_output_is_never_shadowed() {
    let sandbox = Sandbox::new();
    sandbox.write("src/main.rs", "fn main() {}\n");
    sandbox.write("node_modules/pkg/index.js", "module.exports = 1\n");
    sandbox.write("target/debug/app", "binary\n");
    init(&sandbox);

    let output = sandbox.ok(&["guard", "scan"]);
    assert!(
        output.contains("1 captured"),
        "only the source file should be captured, got: {output}"
    );
}

#[test]
fn status_reports_the_configured_roots() {
    let sandbox = Sandbox::new();
    init(&sandbox);
    let output = sandbox.ok(&["guard", "status"]);
    assert!(output.contains("stopped"), "nothing was started: {output}");
    assert!(output.contains("roots"), "{output}");
    assert!(output.contains("shadowed"), "{output}");
}

#[test]
fn init_without_a_root_explains_the_choice() {
    let sandbox = Sandbox::new();
    let error = sandbox.fail(&["guard", "init"]);
    assert!(
        error.contains("--root"),
        "clap should require a root, got: {error}"
    );
}

#[test]
fn the_daemon_starts_reports_and_stops() {
    let sandbox = Sandbox::new();
    sandbox.write("a.txt", "x");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);

    sandbox.ok(&["guard", "start"]);
    let running = sandbox.ok(&["guard", "status"]);
    assert!(running.contains("running"), "{running}");

    // A second start must not spawn a second watcher.
    let again = sandbox.ok(&["guard", "start"]);
    assert!(again.contains("already running"), "{again}");

    sandbox.ok(&["guard", "stop"]);
    let stopped = sandbox.ok(&["guard", "status"]);
    assert!(stopped.contains("stopped"), "{stopped}");
}

#[test]
fn the_human_table_names_the_agent_and_the_path() {
    let sandbox = Sandbox::new();
    sandbox.write("notes.txt", "important");
    init(&sandbox);
    sandbox.ok(&["guard", "scan"]);
    sandbox.remove("notes.txt");
    sandbox.ok(&["guard", "check"]);

    let output = sandbox.ok(&["flags"]);
    assert!(output.contains("severity"), "{output}");
    assert!(output.contains("notes.txt"), "{output}");
    assert!(output.contains("unattributed"), "{output}");
}
