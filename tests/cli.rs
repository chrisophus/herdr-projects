//! End-to-end checks of the built binary with a scrubbed environment.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

fn hp(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .env_clear()
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn context_prints_a_usable_prefix_in_a_scrubbed_environment() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("my root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());

    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let prefix = text.lines().next().unwrap().strip_prefix("Commands: ").unwrap();
    // Fixed shape `<binary> --root <root>`, with the spaced root shell-quoted.
    assert_eq!(prefix, format!("{BIN} --root '{root_arg}'"));

    // The printed prefix works as typed, from a bare shell.
    let listed = Command::new("/bin/sh")
        .env_clear()
        .env("HOME", home.path())
        .args(["-c", &format!("{prefix} list")])
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "demo\tactive\tno threads\n");
}

#[test]
fn peek_records_nothing_and_context_records_seen_items() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "demo"]).status.success());
    let item = "+++\nid = \"20260917T000000Z-routine-r-1\"\nkind = \"routine\"\nsubject = \"r\"\ncreated = \"x\"\nsummary = \"s\"\n+++\n";
    std::fs::write(root.join("demo/inbox/20260917T000000Z-routine-r-1.md"), item).unwrap();
    let seen = root.join("demo/.state/inbox-seen.json");

    assert!(hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]).status.success());
    assert!(!seen.exists());
    assert!(hp(home.path(), &["--root", root_arg, "context", "demo"]).status.success());
    assert!(std::fs::read_to_string(&seen).unwrap().contains("routine-r-1"));
}

#[test]
fn path_like_names_and_slugs_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(!hp(home.path(), &["--root", root_arg, "new", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "open", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "context", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "thread", "list", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "delete", "../x", "--force"]).status.success());
    assert!(!root.exists());
    assert!(!home.path().join("x").exists());
}

#[test]
fn ticker_start_without_projects_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    assert!(hp(home.path(), &["ticker", "start"]).status.success());
    assert!(!home.path().join(".herdr-projects").exists());
    assert!(!home.path().join(".config").exists());
}

#[test]
fn roles_and_the_log_work_from_the_binary() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "demo"]).status.success());

    let out = hp(home.path(), &["--root", root_arg, "role", "list", "demo"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("implementer\tBuilds the change"), "{text}");
    assert_eq!(text.lines().count(), 4);

    // `init` restores a deleted default and leaves an edited one alone.
    std::fs::remove_file(root.join("demo/roles/scout.md")).unwrap();
    std::fs::write(root.join("demo/roles/reviewer.md"), "Mine.\n").unwrap();
    let out = hp(home.path(), &["--root", root_arg, "role", "init", "demo"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "wrote roles/scout.md\n");
    assert_eq!(std::fs::read_to_string(root.join("demo/roles/reviewer.md")).unwrap(), "Mine.\n");

    // The digest names the roles; the log is empty until the binary does something.
    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Roles: implementer (Builds the change on its own branch and opens a pull request); reviewer; scout ("), "{text}");
    let out = hp(home.path(), &["--root", root_arg, "log", "demo"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "no events yet\n");
    assert!(!hp(home.path(), &["--root", root_arg, "log", "demo", "--since", "2w"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "log", "demo", "--thread", "x"]).status.success());

    // An event line, as `inbox` writes them, comes back in order and as JSON.
    std::fs::write(root.join("demo/events.jsonl"), "{\"ts\":\"2026-09-25T10:00:00Z\",\"kind\":\"thread-started\",\"thread\":\"t-0001\",\"summary\":\"\\\"Fix\\\" as implementer (claude)\"}\nnot json\n{\"ts\":\"2026-09-25T10:05:00Z\",\"kind\":\"inbox:pr\",\"thread\":\"t-0001\",\"summary\":\"opened\"}\n").unwrap();
    let out = hp(home.path(), &["--root", root_arg, "log", "demo", "--thread", "t-0001", "--limit", "1"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.ends_with("  t-0001 inbox:pr: opened\n"), "{text}");
    assert_eq!(text.lines().count(), 1);
    let out = hp(home.path(), &["--root", root_arg, "log", "demo", "--json", "--limit", "0"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert!(text.starts_with("{\"ts\":\"2026-09-25T10:00:00Z\",\"kind\":\"thread-started\""));
}
