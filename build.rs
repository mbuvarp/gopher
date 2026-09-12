use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    // Watch source files and Git metadata, including linked worktree metadata.
    // A commit must refresh the displayed revision even without source changes.
    if let Some(files) = git(&["ls-files", "--cached", "--others", "--exclude-standard"]) {
        for file in files.lines() {
            println!("cargo:rerun-if-changed={file}");
        }
    }
    for name in ["HEAD", "index", "refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
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
