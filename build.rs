use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    // File lists cannot detect newly created untracked files. Intentionally watch
    // a nonexistent path so Cargo refreshes this small Git probe on every build,
    // without recursively scanning the repository (including target and dist).
    let refresh = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
        .join("always-refresh-git-metadata");
    println!("cargo:rerun-if-changed={}", refresh.display());
    let revision = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=normal"])
        .map(|status| {
            if status.is_empty() {
                "clean"
            } else {
                "uncommitted changes"
            }
        })
        .unwrap_or("checkout status unknown");
    println!("cargo:rustc-env=GOPHER_BUILD_REVISION={revision}");
    println!("cargo:rustc-env=GOPHER_BUILD_STATUS={dirty}");
}
