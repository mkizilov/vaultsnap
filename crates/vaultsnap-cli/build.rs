//! Records where a binary came from, for `vaultsnap --version` and `vaultsnap version`: the git
//! commit it was built from, the target it runs on, and the build profile.
//!
//! None of it can fail the build. Without git, or outside a checkout (a source archive), the
//! commit is `unknown`; a release build from an archive can set `VAULTSNAP_BUILD_COMMIT`.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=VAULTSNAP_BUILD_COMMIT");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");

    let commit = env::var("VAULTSNAP_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(git_commit)
        .unwrap_or_else(|| "unknown".to_owned());
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    let profile = env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());

    println!("cargo:rustc-env=VAULTSNAP_BUILD_COMMIT={}", commit.trim());
    println!("cargo:rustc-env=VAULTSNAP_BUILD_TARGET={target}");
    println!("cargo:rustc-env=VAULTSNAP_BUILD_PROFILE={profile}");
}

/// The abbreviated commit of the checkout this crate is in, with `-dirty` if tracked files have
/// uncommitted changes.
fn git_commit() -> Option<String> {
    let directory = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from)?;
    let output = Command::new("git")
        .args(["describe", "--always", "--dirty", "--abbrev=12"])
        .current_dir(directory)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}
