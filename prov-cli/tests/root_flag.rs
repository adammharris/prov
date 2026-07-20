//! `-C <dir>` / `--root <dir>` and `PROV_ROOT` — running prov against a vault
//! without `cd`-ing into it (the `git -C` model). The flag goes before the
//! subcommand; it wins over the env var; and relative path arguments resolve
//! against the chosen directory.

use std::path::PathBuf;
use std::process::Command;

fn prov() -> Command {
    Command::new(env!("CARGO_BIN_EXE_prov"))
}

/// An isolated `base/vault` where `vault` is an initialized workspace and `base`
/// is a neutral outside directory to invoke from.
fn isolated(tag: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("prov-root-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let vault = base.join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let ok = prov()
        .current_dir(&vault)
        .args(["init", "--yes"])
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "init the vault");
    (base, vault)
}

#[test]
fn dash_c_operates_on_a_vault_from_outside_it() {
    let (base, vault) = isolated("flag");
    let out = prov()
        .current_dir(&base)
        .args(["-C", vault.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn prov_root_env_operates_on_a_vault_from_outside_it() {
    let (base, vault) = isolated("env");
    let out = prov()
        .current_dir(&base)
        .env("PROV_ROOT", &vault)
        .args(["check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn the_flag_wins_over_the_env_var() {
    let (base, vault) = isolated("prec");
    // The env var points nowhere useful; the flag points at the vault and wins.
    let out = prov()
        .current_dir(&base)
        .env("PROV_ROOT", base.join("nowhere"))
        .args(["-C", vault.to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "flag must win over env: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn relative_arguments_resolve_against_the_root_dir() {
    let (base, vault) = isolated("relpath");
    let out = prov()
        .current_dir(&base)
        .args([
            "-C",
            vault.to_str().unwrap(),
            "new",
            "Remote",
            "--in",
            "index.md",
            "-p",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(vault.join("remote.md").exists(), "created inside the vault");
    assert!(!base.join("remote.md").exists(), "not in the invoking cwd");
}

#[test]
fn a_nonexistent_root_dir_errors_cleanly() {
    let (base, _vault) = isolated("bad");
    let out = prov()
        .current_dir(&base)
        .args(["-C", base.join("nope").to_str().unwrap(), "check"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a missing --root must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not use root directory"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
