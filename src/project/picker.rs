use std::path::PathBuf;
use std::process::{Command, Output};

#[cfg(target_os = "linux")]
use std::io::ErrorKind;

pub fn pick_project_directory() -> Result<Option<PathBuf>, String> {
    picker_output().and_then(selected_path)
}

#[cfg(target_os = "macos")]
fn picker_output() -> Result<Output, String> {
    Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "POSIX path of (choose folder with prompt \"Add Git Project\")",
        ])
        .output()
        .map_err(|error| format!("Could not open the folder picker: {error}"))
}

#[cfg(target_os = "linux")]
fn picker_output() -> Result<Output, String> {
    for (program, args) in [
        (
            "zenity",
            &["--file-selection", "--directory", "--title=Add Git Project"][..],
        ),
        (
            "kdialog",
            &["--getexistingdirectory", ".", "--title", "Add Git Project"][..],
        ),
    ] {
        match Command::new(program).args(args).output() {
            Ok(output) => return Ok(output),
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("Could not open the folder picker: {error}")),
        }
    }
    Err("No folder picker is available. Install zenity or kdialog.".to_owned())
}

fn selected_path(output: Output) -> Result<Option<PathBuf>, String> {
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if detail.is_empty() || detail.contains("User canceled") || detail.contains("-128") {
            return Ok(None);
        }
        return Err(format!("Folder picker failed: {detail}"));
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}
