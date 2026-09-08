//! CLI-level tests: process exit codes (0 success / 1 runtime error /
//! 2 usage error, as promised by the README), quiet mode, and the output
//! extension warning. These run the real binary via assert_cmd.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use assert_cmd::Command;
use predicates::prelude::*;

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch() -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("fs2iso-cli-{}-{}", std::process::id(), n))
}

fn cmd() -> Command {
    Command::cargo_bin("fs2iso").unwrap()
}

#[test]
fn usage_error_exits_2() {
    // no output path and no inputs at all -> clap usage error
    cmd()
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn missing_input_exits_1() {
    let dir = scratch();
    fs::create_dir_all(&dir).unwrap();
    // output exists -> passes the extension check; input does not exist
    cmd()
        .args(["out.iso", "no-such-dir-xyz"])
        .current_dir(&dir)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("fs2iso: error:"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn successful_build_exits_0_with_summary() {
    let dir = scratch();
    fs::create_dir_all(dir.join("payload")).unwrap();
    fs::write(dir.join("payload/readme.txt"), b"hello\n").unwrap();
    cmd()
        .args(["--flat", "out.iso", "payload"])
        .current_dir(&dir)
        .assert()
        .code(0)
        .stdout(predicate::str::contains("wrote out.iso"))
        .stdout(predicate::str::contains("label: OUT"))
        .stderr(predicate::str::is_empty());
    assert!(dir.join("out.iso").is_file());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn quiet_suppresses_summary() {
    let dir = scratch();
    fs::create_dir_all(dir.join("payload")).unwrap();
    fs::write(dir.join("payload/readme.txt"), b"hello\n").unwrap();
    cmd()
        .args(["--flat", "-q", "out.iso", "payload"])
        .current_dir(&dir)
        .assert()
        .code(0)
        .stdout(predicate::str::is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn non_iso_extension_warns_on_stderr() {
    let dir = scratch();
    fs::create_dir_all(dir.join("payload")).unwrap();
    fs::write(dir.join("payload/readme.txt"), b"hello\n").unwrap();
    cmd()
        .args(["--flat", "out.weird", "payload"])
        .current_dir(&dir)
        .assert()
        .code(0)
        .stderr(predicate::str::contains("does not end in .iso/.img"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_help_and_version() {
    cmd()
        .arg("--help")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("--no-eltorito"));
    cmd().arg("--version").assert().code(0);
}
