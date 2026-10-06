use std::ffi::OsStr;
use std::process::{Command, Stdio};

use semver::Version;
use serde_json::Value;

pub(crate) fn skipped_version() -> Option<String> {
    std::fs::read_to_string(super::state_directory()?.join("skipped-version")).ok()
}

pub(crate) fn save_skipped_version(tag: &str) {
    if let Some(directory) = super::state_directory()
        && std::fs::create_dir_all(&directory).is_ok()
    {
        let _ = std::fs::write(directory.join("skipped-version"), tag);
    }
}

pub(crate) fn latest_version() -> Result<Option<String>, String> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--location",
            "--connect-timeout",
            "5",
            "--max-time",
            "15",
            "--header",
            "Accept: application/vnd.github+json",
            "--user-agent",
            "herdr-git",
            "https://api.github.com/repos/JeHwanYoo/herdr-git/releases/latest",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Could not check for updates: {error}"))?;
    if !output.status.success() {
        return Err("Could not reach the GitHub release page".to_owned());
    }
    let release = serde_json::from_slice::<Value>(&output.stdout)
        .map_err(|_| "GitHub returned an unreadable release".to_owned())?;
    Ok(available_version(&release))
}

fn available_version(release: &Value) -> Option<String> {
    if release["draft"].as_bool()? || release["prerelease"].as_bool()? {
        return None;
    }
    let tag = release["tag_name"].as_str()?;
    let version = Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    (version > current && version.pre.is_empty()).then(|| tag.to_owned())
}

pub(crate) fn install_update(tag: &str) -> Result<(), String> {
    install_update_with(&super::herdr_binary(), tag)
}

fn install_update_with(binary: &OsStr, tag: &str) -> Result<(), String> {
    let output = Command::new("sh")
        .args([
            "-c",
            include_str!("../../scripts/update.sh"),
            "herdr-git-update",
            tag,
        ])
        .env("HERDR_BIN_PATH", binary)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Could not start update: {error}"))?;
    if !output.status.success() {
        let detail = if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        };
        return Err(super::command_error("Could not install update", detail));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{available_version, install_update_with};
    use serde_json::json;

    #[test]
    fn only_newer_stable_releases_offer_an_update() {
        let release = |tag, draft, prerelease| json!({"tag_name": tag, "draft": draft, "prerelease": prerelease});
        assert_eq!(
            available_version(&release("v1.0.0", false, false)),
            Some("v1.0.0".into())
        );
        for tag in [
            "v0.1.0",
            env!("CARGO_PKG_VERSION"),
            "v1.0.0-beta.1",
            "invalid",
        ] {
            assert_eq!(available_version(&release(tag, false, false)), None);
        }
        assert_eq!(available_version(&release("v1.0.0", true, false)), None);
        assert_eq!(available_version(&release("v1.0.0", false, true)), None);
        assert_eq!(available_version(&json!({})), None);
    }

    #[test]
    fn reinstall_script_pins_the_release_and_reports_cli_failure() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("herdr-git-update-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("fake herdr");
        fs::write(&binary, "#!/bin/sh\n[ \"$#\" = 6 ] || exit 1\n[ \"$1\" = plugin ] || exit 1\n[ \"$2\" = install ] || exit 1\n[ \"$3\" = JeHwanYoo/herdr-git ] || exit 1\n[ \"$4\" = --ref ] || exit 1\n[ \"$5\" = v1.0.0 ] || exit 1\n[ \"$6\" = --yes ] || exit 1\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(install_update_with(binary.as_os_str(), "v1.0.0"), Ok(()));
        fs::write(&binary, "#!/bin/sh\necho 'build failed' >&2\nexit 1\n").unwrap();
        assert_eq!(
            install_update_with(binary.as_os_str(), "v1.0.0"),
            Err("Could not install update: build failed".into())
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
