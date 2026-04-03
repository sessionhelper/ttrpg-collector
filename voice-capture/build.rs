//! Build script that computes a version string from git state.
//!
//! - Clean tag: "v0.3.0"
//! - Not on tag: "feature/foo-abc1234"
//! - Dirty working tree: appends "-dirty"

use std::process::Command;

fn main() {
    let version = git_version().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_VERSION={version}");
    // Re-run if git state changes
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/");
}

fn git_version() -> Option<String> {
    // Try `git describe --tags --exact-match` for clean tag
    let tag = Command::new("git")
        .args(["describe", "--tags", "--exact-match"])
        .output()
        .ok()?;

    if tag.status.success() {
        let mut v = String::from_utf8_lossy(&tag.stdout).trim().to_string();
        if is_dirty() {
            v.push_str("-dirty");
        }
        return Some(v);
    }

    // Not on a tag — use branch-hash
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "000000".to_string());

    let mut v = format!("{branch}-{hash}");
    if is_dirty() {
        v.push_str("-dirty");
    }
    Some(v)
}

fn is_dirty() -> bool {
    Command::new("git")
        .args(["diff", "--quiet", "HEAD"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(false)
}
