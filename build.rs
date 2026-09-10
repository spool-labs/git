//! Stamp the Git-helper source revision into the public binary version.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TAPE_SOURCE_REVISION");
    if let Some(git_dir) = run_git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/logs/HEAD");
    }

    let sha = std::env::var("TAPE_SOURCE_REVISION")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| run_git(&["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    let suffix = if std::env::var_os("TAPE_SOURCE_REVISION").is_some() {
        ""
    } else if is_dirty() {
        "-dirty"
    } else {
        ""
    };

    println!("cargo:rustc-env=TAPE_BUILD_SHA={sha}");
    println!("cargo:rustc-env=TAPE_BUILD_SUFFIX={suffix}");
}

fn is_dirty() -> bool {
    let _ = Command::new("git")
        .args(["update-index", "-q", "--refresh"])
        .status();
    Command::new("git")
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .status()
        .is_ok_and(|status| !status.success())
}

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}
