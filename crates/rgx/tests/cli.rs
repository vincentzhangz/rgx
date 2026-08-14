//! End-to-end tests driving the real `rgx::execute` in-process with buffer
//! writers. Because the binary is a thin wrapper around the library, these
//! exercise the full CLI logic and are captured by the coverage report.

use std::fs;
use std::path::{Path, PathBuf};

/// Run rgx in-process, returning (exit_code, stdout, stderr).
fn run(args: &[&str]) -> (i32, String, String) {
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = rgx::execute(args.iter().map(|s| s.to_string()), &mut out, &mut err);
    (
        code,
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    )
}

fn remove_all_retry(path: &Path) {
    for attempt in 0..5 {
        match fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                ) && attempt + 1 < 5 =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50 * (1 << attempt)));
            }
            Err(_) => return,
        }
    }
}

fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rgx-cli-test-{name}-{}", std::process::id()));
    remove_all_retry(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn standard_fixture(root: &Path) {
    write(
        &root.join("src/main.rs"),
        "fn main() {\n    let x = 42;\n    println!(\"hello {}\", x);\n}\n",
    );
    write(
        &root.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    );
    write(
        &root.join("README.md"),
        "# hello world\n\nthe greeting is hello.\n",
    );
    write(&root.join("notes/empty.txt"), "");
    write(&root.join("notes/tabs.txt"), "\tindented\n\tsecond tab\n");
    write(&root.join("data.bin"), "\x00\x01\x02hello\x00tail");
}

#[test]
fn indexed_matches_brute_force() {
    let root = fixture("equiv");
    standard_fixture(&root);

    let patterns = [
        "hello",
        "Hello",
        "hello|add",
        "fn main",
        "42",
        "a + b",
        "[a-z]+",
        "\\d+",
        "tabs?",
        "i32",
        "greeting",
        "x = 4",
    ];

    for pat in patterns {
        let (bc, bo, _be) = run(&["--no-index", pat, root.to_str().unwrap()]);
        let (ic, io, _ie) = run(&[pat, root.to_str().unwrap()]);
        assert_eq!(
            ic, bc,
            "exit codes differ for {pat}: indexed={ic} brute={bc}"
        );
        assert_eq!(
            io, bo,
            "outputs differ for {pat}\nindexed:\n{io}\nbrute:\n{bo}"
        );
    }
}

#[test]
fn exit_codes() {
    let root = fixture("exitcodes");
    standard_fixture(&root);
    let (code, stdout, _) = run(&["hello", root.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(!stdout.is_empty());

    let (code, stdout, _) = run(&["zzzzzqqqqq", root.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(stdout.is_empty());

    let (code, _stdout, stderr) = run(&["[", root.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(!stderr.is_empty(), "error should be reported on stderr");
}

#[test]
fn ignore_case() {
    let root = fixture("case");
    standard_fixture(&root);
    let (sensitive, _, _) = run(&["HELLO", root.to_str().unwrap()]);
    let (insensitive, stdout, _) = run(&["-i", "HELLO", root.to_str().unwrap()]);
    assert_eq!(sensitive, 1, "case-sensitive query must not match");
    assert_eq!(insensitive, 0, "case-insensitive query must match");
    let line = stdout.lines().next().unwrap();
    assert!(line.contains("hello"), "line content: {line}");
}

#[test]
fn json_output_shape() {
    let root = fixture("json");
    standard_fixture(&root);
    let (code, stdout, _) = run(&["--json", "hello", root.to_str().unwrap()]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines.len() >= 2);
    for line in &lines {
        assert!(line.starts_with("{\"path\":\""), "line: {line}");
        assert!(line.contains("\"line_number\":"), "line: {line}");
        assert!(line.contains("\"submatches\":[{\"start\":"));
    }
}

#[test]
fn stats_and_time_flags() {
    let root = fixture("stats");
    standard_fixture(&root);
    run(&["hello", root.to_str().unwrap()]);
    let (code, _, stderr) = run(&["--stats", "--time", "hello", root.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(stderr.contains("rgx: index:"), "stats on stderr: {stderr}");
    assert!(stderr.contains("rgx: total:"), "timing on stderr: {stderr}");
}

#[test]
fn builds_automatically() {
    let root = fixture("autobuild");
    standard_fixture(&root);
    let (code, _, _) = run(&["hello", root.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(root.join(".rgx/lookup.dat").exists());
    assert!(root.join(".rgx/postings.dat").exists());
    assert!(root.join(".rgx/grams.dat").exists());
}

#[test]
fn update_is_incremental() {
    let root = fixture("update");
    standard_fixture(&root);
    let (code, _, _) = run(&["hello", root.to_str().unwrap()]);
    assert_eq!(code, 0);

    write(&root.join("new.txt"), "a brand new hello\n");
    let (code, _, stderr) = run(&["--stats", "--update", "hello", root.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(
        stderr.contains("reused"),
        "stats should report reused files: {stderr}"
    );
    assert!(
        stderr.contains("re-read"),
        "stats should report the re-read file: {stderr}"
    );
}

#[test]
fn corrupt_index_exits_2_without_panic() {
    let root = fixture("corrupt");
    standard_fixture(&root);
    let (code, _, _) = run(&["hello", root.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(root.join(".rgx/lookup.dat").exists());

    fs::write(root.join(".rgx/lookup.dat"), b"garbage-not-a-lookup").unwrap();
    let (code, _stdout, stderr) = run(&["hello", root.to_str().unwrap()]);
    assert_eq!(code, 2, "corrupt index must be reported as an error");
    assert!(stderr.contains("cannot load index"), "stderr: {stderr}");
}

#[test]
fn search_from_subdirectory_uses_valid_paths() {
    let root = fixture("relpaths");
    standard_fixture(&root);
    let bin = env!("CARGO_BIN_EXE_rgx");
    let out = std::process::Command::new(bin)
        .args(["hello", "."])
        .current_dir(root.join("src"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.is_empty());
    for line in stdout.lines() {
        let path = line.split(':').next().unwrap();
        let p = Path::new(path);
        assert!(p.exists(), "result path must exist: {path}");
        assert!(
            p.ends_with("main.rs") || p.ends_with("README.md"),
            "unexpected result path: {path}"
        );
    }
}

#[test]
fn help_flag() {
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("USAGE"));
    assert!(stdout.contains("--no-index"));
}

#[test]
fn invalid_option_exits_2() {
    let (code, _stdout, stderr) = run(&["--bogus", "x"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("unknown option"));
}

/// One true subprocess test: prove the real binary built by cargo behaves
/// end to end (correct exit code and output on stdout).
#[test]
fn binary_smoke_test() {
    let root = fixture("binary");
    standard_fixture(&root);
    let bin = env!("CARGO_BIN_EXE_rgx");
    let out = std::process::Command::new(bin)
        .args(["hello", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.lines().count() >= 2);
    assert!(stdout.lines().all(|l| l.contains(":")), "lines: {stdout}");
}

#[test]
fn broken_pipe_does_not_crash() {
    let root = fixture("pipe");
    standard_fixture(&root);
    let bin = env!("CARGO_BIN_EXE_rgx");
    let mut child = std::process::Command::new(bin)
        .args(["hello", root.to_str().unwrap()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let status = child.wait().unwrap();
    let code = status.code();
    assert!(
        code.is_some(),
        "process must exit normally (not crash): {status:?}"
    );
    assert!(
        code == Some(0) || code == Some(1),
        "unexpected code: {code:?}"
    );
}
