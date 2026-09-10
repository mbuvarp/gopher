//! Installation runs from the verified downloaded bundle, without starting the
//! worker or touching session.json. Hold both installer and app instance locks
//! while replacing files so other installers and app launches cannot race it.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const IDENTITY: &str = "=anchor apple generic and identifier \"dev.mbuvarp.gopher\" and certificate leaf[subject.OU] = \"DZ4XZQXHZ7\"";

fn command(program: &str, args: &[&str]) -> Result<String> {
    let result = Command::new(program).args(args).output()?;
    ensure!(
        result.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

fn text(path: &Path) -> Result<&str> {
    path.to_str()
        .context("Installation path is not valid UTF-8")
}

fn property(app: &Path, key: &str) -> Result<String> {
    command(
        "/usr/bin/plutil",
        &[
            "-extract",
            key,
            "raw",
            "-o",
            "-",
            text(&app.join("Contents/Info.plist"))?,
        ],
    )
}

fn version(value: &str) -> Result<(u32, u32, u32)> {
    let parts: Vec<_> = value.split('.').collect();
    ensure!(parts.len() == 3, "Unsupported installed version: {value}");
    for part in &parts {
        ensure!(
            !part.is_empty()
                && part.bytes().all(|c| c.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0')),
            "Unsupported installed version: {value}"
        );
    }
    Ok((parts[0].parse()?, parts[1].parse()?, parts[2].parse()?))
}

fn verify(app: &Path) -> Result<()> {
    command(
        "/usr/bin/codesign",
        &["--verify", "--deep", "--strict", "-R", IDENTITY, text(app)?],
    )?;
    ensure!(
        property(app, "CFBundleIdentifier")? == "dev.mbuvarp.gopher",
        "Unexpected application identity"
    );
    Ok(())
}

fn lock_file(path: &Path) -> Result<File> {
    ensure!(
        !path.is_symlink(),
        "Refusing a symlink at {}",
        path.display()
    );
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}

fn applications_directory(home: &Path) -> Result<PathBuf> {
    let apps = home.join("Applications");
    // Check before create_dir_all/canonicalize can follow the destination.
    match fs::symlink_metadata(&apps) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Refusing a symlink or non-directory at {}",
            apps.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&apps)?,
        Err(error) => return Err(error.into()),
    }
    Ok(apps.canonicalize()?)
}

fn is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
}

fn matching_process(pid: u32, target: &Path) -> Result<bool> {
    let result = Command::new("/bin/ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "uid=,comm="])
        .output()?;
    if !result.status.success() {
        return Ok(false);
    }
    let result = String::from_utf8(result.stdout)?;
    let line = result.trim();
    let Some((uid, path)) = line.split_once(char::is_whitespace) else {
        return Ok(false);
    };
    Ok(uid == command("/usr/bin/id", &["-u"])?
        && path.trim() == text(&target.join("Contents/MacOS/gopher"))?)
}

fn has_update_installer(processes: &str, uid: &str, target: &Path) -> bool {
    let executable = target.join("Contents/Frameworks/Sparkle.framework/Versions/B/Autoupdate");
    processes.lines().any(|line| {
        line.trim()
            .split_once(char::is_whitespace)
            .is_some_and(|(owner, path)| owner == uid && Path::new(path.trim()) == executable)
    })
}

fn ensure_no_update_installer(target: &Path) -> Result<()> {
    let processes = command("/bin/ps", &["-axww", "-o", "uid=,comm="])?;
    ensure!(
        !has_update_installer(&processes, &command("/usr/bin/id", &["-u"])?, target),
        "Gopher's in-app updater is running. Let it finish, then rerun the installer"
    );
    Ok(())
}

fn acquire_app_lock(lock: &File, data: &Path, target: &Path) -> Result<bool> {
    match fs2::FileExt::try_lock_exclusive(lock) {
        Ok(()) => return Ok(false),
        Err(error) if is_contended(&error) => {}
        Err(error) => return Err(error.into()),
    }
    let session: serde_json::Value = serde_json::from_slice(&fs::read(data.join("session.json"))?)?;
    let pid = session["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 1)
        .context("Cannot identify running Gopher; quit it and rerun the installer")?;
    ensure!(
        session["ended_at"].is_null() && matching_process(pid, target)?,
        "Another Gopher instance is running; quit it and rerun the installer"
    );
    ensure!(
        property(target, "GopherInstallerProtocol").ok().as_deref() == Some("1"),
        "This older Gopher cannot wait for active PR actions. Quit Gopher manually and rerun the installer"
    );
    verify(target)?;
    println!("Waiting for Gopher to finish active PR actions and quit…");
    command("/bin/kill", &["-TERM", &pid.to_string()])?;
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        match fs2::FileExt::try_lock_exclusive(lock) {
            Ok(()) => {
                if !matching_process(pid, target)? {
                    return Ok(true);
                }
            }
            Err(error) if is_contended(&error) => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            Instant::now() < deadline,
            "Gopher did not finish quitting; installation unchanged. No process was force-killed"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn replace(source: &Path, target: &Path, backup: &Path) -> Result<()> {
    let existed = target.exists();
    if existed {
        fs::rename(target, backup)?;
    }
    if let Err(error) = fs::rename(source, target) {
        if existed {
            fs::rename(backup, target).with_context(|| {
                format!(
                    "Replacement failed ({error}); old app is at {}",
                    backup.display()
                )
            })?;
        }
        return Err(error.into());
    }
    Ok(())
}

fn launch(target: &Path) -> Result<()> {
    command("/usr/bin/open", &[text(target)?])?;
    Ok(())
}

pub fn install(no_launch: bool) -> Result<()> {
    ensure!(
        command("/usr/bin/id", &["-u"])? != "0",
        "Run the installer as your normal user, without sudo"
    );
    let source = std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("Installer must run from Gopher.app")?
        .to_path_buf();
    verify(&source)?;
    ensure!(
        property(&source, "CFBundleShortVersionString")? == env!("CARGO_PKG_VERSION"),
        "Downloaded version does not match its executable"
    );
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unset")?);
    let apps = applications_directory(&home)?;
    let target = apps.join("Gopher.app");
    ensure!(
        source.canonicalize()? != target,
        "Installer must run from the downloaded archive, not the installed app"
    );
    let installer_lock = lock_file(&apps.join(".gopher-installer.lock"))?;
    fs2::FileExt::try_lock_exclusive(&installer_lock)
        .context("Another Gopher installer is running")?;
    ensure!(
        !target.is_symlink(),
        "Refusing to replace a symlink at {}",
        target.display()
    );
    let existed = target.exists();
    if existed {
        verify(&target).context("Existing Gopher could not be verified; leaving it unchanged")?;
        match version(&property(&target, "CFBundleShortVersionString")?)?
            .cmp(&version(env!("CARGO_PKG_VERSION"))?)
        {
            std::cmp::Ordering::Equal => {
                println!("Gopher {} is already current.", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            std::cmp::Ordering::Greater => {
                bail!("Installed Gopher is newer; refusing to downgrade")
            }
            std::cmp::Ordering::Less => {}
        }
    }
    let stage = tempfile::Builder::new()
        .prefix(".gopher-install-")
        .tempdir_in(&apps)?;
    let staged_app = stage.path().join("Gopher.app");
    command("/usr/bin/ditto", &[text(&source)?, text(&staged_app)?])?;
    verify(&staged_app)?;
    let data = crate::config::Config::directory()?;
    fs::create_dir_all(&data)?;
    let app_lock = lock_file(&data.join("gopher.lock"))?;
    ensure_no_update_installer(&target)?;
    let was_running = acquire_app_lock(&app_lock, &data, &target)?;
    // A scheduled update can start between staging and the app finishing quit.
    // Its installer runs outside the host and does not own Gopher's app lock.
    ensure_no_update_installer(&target)?;
    let backup = stage.path().join("previous.app");
    if let Err(error) = replace(&staged_app, &target, &backup) {
        // Never let temporary-directory cleanup delete the only surviving old app.
        if backup.exists() {
            let _ = stage.keep();
        }
        drop(app_lock);
        if was_running && !no_launch && target.exists() {
            let _ = launch(&target);
        }
        return Err(error);
    }
    // Gopher must be able to acquire its own lock when it launches.
    drop(app_lock);
    if !no_launch
        && (!existed || was_running)
        && let Err(error) = launch(&target)
    {
        // Opening can fail due to desktop/Gatekeeper state. Keep the verified app
        // and old backup for recovery rather than roll back after it might run a migration.
        let saved = stage.keep();
        bail!(
            "Installed Gopher but could not request launch: {error}. Open {} manually. Recovery files: {}",
            target.display(),
            saved.display()
        );
    }
    println!(
        "Installed Gopher {} at {}.",
        env!("CARGO_PKG_VERSION"),
        target.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applications_directory_rejects_symlinks_without_touching_the_target() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let outside = root.path().join("outside");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, home.join("Applications")).unwrap();
        assert!(applications_directory(&home).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        assert_eq!(
            fs::read_to_string(outside.join("keep")).unwrap(),
            "unchanged"
        );
        fs::remove_file(home.join("Applications")).unwrap();
        std::os::unix::fs::symlink(root.path().join("missing"), home.join("Applications")).unwrap();
        assert!(applications_directory(&home).is_err());
        assert!(!root.path().join("missing").exists());
        fs::remove_file(home.join("Applications")).unwrap();
        let apps = applications_directory(&home).unwrap();
        assert!(apps.is_dir());
        assert_eq!(applications_directory(&home).unwrap(), apps);
    }

    #[test]
    fn update_installer_detection_matches_owner_and_exact_bundle_path() {
        let target = Path::new("/Users/test/Applications/Gopher.app");
        let helper = "/Users/test/Applications/Gopher.app/Contents/Frameworks/Sparkle.framework/Versions/B/Autoupdate";
        assert!(has_update_installer(
            &format!(" 501 {helper}\n"),
            "501",
            target
        ));
        assert!(!has_update_installer(
            &format!(" 502 {helper}\n"),
            "501",
            target
        ));
        assert!(!has_update_installer(
            &format!(" 501 {helper}-other\n"),
            "501",
            target
        ));
        assert!(!has_update_installer(
            "501 /other/Gopher.app/Autoupdate",
            "501",
            target
        ));
    }

    #[test]
    fn versions_are_numeric_and_reject_ambiguous_inputs() {
        assert!(version("0.10.0").unwrap() > version("0.9.9").unwrap());
        for value in ["1", "1.2", "01.2.3", "1.2.3-beta", "1.2.-3", "1.2.3.4"] {
            assert!(version(value).is_err(), "{value}");
        }
    }

    #[test]
    fn replacement_failure_restores_old_installation() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("Gopher.app");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("old"), "preserve").unwrap();
        assert!(
            replace(
                &root.path().join("missing"),
                &target,
                &root.path().join("backup")
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(target.join("old")).unwrap(), "preserve");
    }

    #[test]
    fn replacement_keeps_backup_and_does_not_merge_old_resources() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("Gopher.app");
        let source = root.path().join("new.app");
        let backup = root.path().join("backup");
        fs::create_dir(&target).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(target.join("old"), "old").unwrap();
        fs::write(source.join("new"), "new").unwrap();
        replace(&source, &target, &backup).unwrap();
        assert!(!target.join("old").exists());
        assert!(backup.join("old").exists());
        assert!(target.join("new").exists());
    }

    #[test]
    fn installer_lock_excludes_concurrent_installs() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lock");
        let first = lock_file(&path).unwrap();
        let second = lock_file(&path).unwrap();
        fs2::FileExt::try_lock_exclusive(&first).unwrap();
        assert!(is_contended(
            &fs2::FileExt::try_lock_exclusive(&second).unwrap_err()
        ));
        drop(first);
        fs2::FileExt::try_lock_exclusive(&second).unwrap();
    }
}
