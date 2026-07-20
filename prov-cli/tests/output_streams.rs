//! The output-stream contract: **stdout carries the machine value, stderr the
//! human narration.** A mutation prints the identifier(s) of the object it
//! produced — one per line, undecorated — to stdout, and everything a person
//! reads ("created …", "moved …") to stderr; a reader prints its data to stdout
//! and any incidental chatter ("ok: no findings") to stderr. Success is the exit
//! code, so `2>/dev/null` silences narration without eating data and
//! `$(prov new …)` captures a bare, pipeable path.
//!
//! Unlike `smoke.rs` (which merges the two streams to check exit status), this
//! test keeps them apart on purpose — it is the regression guard for *which
//! stream* each token lands on.

use std::path::Path;
use std::process::Command;

/// Run a command, returning `(success, stdout, stderr)` as three separate values.
fn run(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_prov"))
        .current_dir(dir)
        .args(args)
        .env("PROV_QUIET", "1")
        .env("EDITOR", "true")
        .output()
        .expect("run prov");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Assert a command succeeded, surfacing both streams on failure.
fn ok(dir: &Path, args: &[&str]) -> (String, String) {
    let (ok, out, err) = run(dir, args);
    assert!(ok, "`prov {}` failed:\nstdout:{out}\nstderr:{err}", args.join(" "));
    (out, err)
}

fn sandbox(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("prov-streams-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn mutations_put_the_resulting_path_on_stdout_and_narration_on_stderr() {
    let dir = sandbox("mutations");

    // `init` — stdout is the root document's path; the friendly report is stderr.
    let (out, err) = ok(&dir, &["init", "--yes"]);
    assert!(
        out.trim().ends_with("index.md"),
        "init stdout is the root doc path: {out:?}"
    );
    assert!(err.contains("initialized"), "init narrates on stderr: {err:?}");
    assert!(
        !out.contains("initialized"),
        "no narration leaks onto stdout: {out:?}"
    );

    // `new` — stdout is exactly the created node path, nothing else.
    let (out, err) = ok(&dir, &["new", "Rust", "--in", "index.md"]);
    assert_eq!(out.trim(), "rust.md", "new stdout is the bare path: {out:?}");
    assert!(err.contains("created"), "new narrates on stderr: {err:?}");

    // The stdout path is real and pipeable: it round-trips straight into a reader.
    let (title, _) = ok(&dir, &["get", out.trim(), "title"]);
    assert_eq!(title.trim(), "Rust", "the captured path is usable: {title:?}");

    // `mv` — stdout is the destination (the new handle), narration on stderr.
    let (out, err) = ok(&dir, &["mv", "rust.md", "notes/rust.md"]);
    assert_eq!(out.trim(), "notes/rust.md", "mv stdout is the destination: {out:?}");
    assert!(err.contains("moved"), "mv narrates on stderr: {err:?}");

    // `duplicate` — stdout is the copy's path.
    ok(&dir, &["new", "Zig", "--in", "index.md"]);
    let (out, err) = ok(&dir, &["duplicate", "zig.md"]);
    assert_eq!(out.trim(), "zig-copy.md", "duplicate stdout is the copy: {out:?}");
    assert!(err.contains("duplicated"), "duplicate narrates on stderr: {err:?}");

    // `set`/`unset` — stdout is the edited document's path (was previously silent).
    let (out, _) = ok(&dir, &["set", "zig.md", "summary", "a note"]);
    assert_eq!(out.trim(), "zig.md", "set stdout is the edited path: {out:?}");
    let (out, _) = ok(&dir, &["unset", "zig.md", "summary"]);
    assert_eq!(out.trim(), "zig.md", "unset stdout is the edited path: {out:?}");
}

#[test]
fn an_idempotent_no_op_still_yields_the_path_the_contract_is_the_result() {
    // The stdout contract is the *resulting object*, not the *action taken*: a
    // `new -p` that finds the document already there prints the same path, so a
    // daily-note cron's `$(prov new -p …)` is stable across first and later runs.
    let dir = sandbox("idempotent");
    ok(&dir, &["init", "--yes"]);

    let (out, err) = ok(&dir, &["new", "Today", "--in", "index.md", "-p"]);
    assert_eq!(out.trim(), "today.md", "first run: {out:?}");
    assert!(err.contains("created"), "first run narrates create: {err:?}");

    let (out, err) = ok(&dir, &["new", "Today", "--in", "index.md", "-p"]);
    assert_eq!(out.trim(), "today.md", "re-run yields the same path: {out:?}");
    assert!(err.contains("exists"), "re-run narrates a no-op: {err:?}");
}

#[test]
fn a_dry_run_narrates_but_emits_nothing_pipeable() {
    // `--dry-run` previews on stderr and leaves stdout empty — nothing was created,
    // so there is no object to name. A pipeline reading stdout acts on nothing.
    let dir = sandbox("dryrun");
    ok(&dir, &["init", "--yes"]);
    let (out, err) = ok(&dir, &["new", "Draft", "--in", "index.md", "--dry-run"]);
    assert!(out.trim().is_empty(), "dry-run stdout is empty: {out:?}");
    assert!(err.contains("would create"), "dry-run previews on stderr: {err:?}");
}

#[test]
fn convert_lists_the_changed_paths_on_stdout() {
    // A sweep's stdout is the set of documents it actually rewrote, one per line —
    // the `| git add` handle — with the count as stderr narration.
    let dir = sandbox("convert");
    ok(&dir, &["init", "--yes"]);
    ok(&dir, &["new", "A", "--in", "index.md"]);
    let (out, err) = ok(&dir, &["convert", "index.md", "path_style", "relative"]);
    assert_eq!(out.trim(), "index.md", "convert stdout is the changed path: {out:?}");
    assert!(err.contains("converted"), "convert narrates the count on stderr: {err:?}");
}

#[test]
fn readers_keep_data_on_stdout_and_chatter_on_stderr() {
    let dir = sandbox("readers");
    ok(&dir, &["init", "--yes"]);

    // `check` on a clean workspace: stdout empty (no findings), the "ok" on stderr.
    let (out, err) = ok(&dir, &["check"]);
    assert!(out.trim().is_empty(), "clean check stdout is empty: {out:?}");
    assert!(err.contains("ok"), "clean check says ok on stderr: {err:?}");

    // `config <key>` is a reader: the value is stdout, and nothing else.
    let (out, _) = ok(&dir, &["config", "identity"]);
    assert_eq!(out.trim(), "lazy", "config get value on stdout: {out:?}");

    // `config <key> <value>` (a mutation) echoes the value on stdout, "set …" on
    // stderr.
    let (out, err) = ok(&dir, &["config", "references.target", "id"]);
    assert_eq!(out.trim(), "id", "config set echoes the value on stdout: {out:?}");
    assert!(err.contains("set"), "config set narrates on stderr: {err:?}");

    // `backlinks` with no results: stdout empty, the "no backlinks" note on stderr.
    let (out, err) = ok(&dir, &["backlinks", "index.md"]);
    assert!(out.trim().is_empty(), "empty backlinks stdout is empty: {out:?}");
    assert!(err.contains("no backlinks"), "the note is on stderr: {err:?}");
}
