//! Build script: stamp the binary with its git commit and build time, exposed at runtime via
//! `env!("MPC_GIT_SHA")` / `env!("MPC_BUILT_AT")` (see `web::version`). Best-effort — outside a git
//! checkout or without `git`/`date` on PATH it falls back to `"unknown"` / empty, never failing.

use std::process::Command;

fn cmd(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn main() {
    let sha =
        cmd("git", &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MPC_GIT_SHA={sha}");

    let built =
        cmd("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MPC_BUILT_AT={built}");

    // Re-run when the checked-out commit changes so the stamped SHA stays current. A commit on a
    // branch updates the *ref* file (`.git/refs/heads/<branch>`), not `.git/HEAD` (which only changes
    // on checkout), so watch both — read HEAD to resolve the ref it points at. Also watch
    // `packed-refs`, where the ref moves after `git gc`/`git pack-refs` (the loose file disappears).
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD") {
        if let Some(reference) = head.strip_prefix("ref:").map(str::trim) {
            println!("cargo:rerun-if-changed=.git/{reference}");
        }
    }
}
