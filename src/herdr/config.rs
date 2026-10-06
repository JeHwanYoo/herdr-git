use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const KITTY_GRAPHICS: &str = "kitty_graphics";
const KITTY_GRAPHICS_SECTION: &str = "terminal";
const LEGACY_KITTY_GRAPHICS_SECTION: &str = "experimental";

pub(crate) fn config_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("HERDR_CONFIG_PATH") {
        return Some(path.into());
    }
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("herdr").join("config.toml"))
}

pub(crate) fn kitty_graphics(path: &Path) -> Result<Option<bool>, String> {
    match fs::read_to_string(path) {
        Ok(config) => Ok(configured_kitty_graphics(&config)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read {}: {error}", path.display())),
    }
}

pub(crate) fn set_kitty_graphics(path: &Path, enabled: bool) -> Result<(), String> {
    let config = match fs::read_to_string(path) {
        Ok(config) => config,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("Could not read {}: {error}", path.display())),
    };
    let updated = with_kitty_graphics(&config, enabled);
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)
            .map_err(|error| format!("Could not create {}: {error}", directory.display()))?;
    }
    let staged = path.with_extension("toml.herdr-git");
    fs::write(&staged, updated)
        .and_then(|()| fs::rename(&staged, path))
        .map_err(|error| format!("Could not write {}: {error}", path.display()))
}

fn configured_kitty_graphics(config: &str) -> Option<bool> {
    bool_value(config, KITTY_GRAPHICS_SECTION, KITTY_GRAPHICS)
        .or_else(|| bool_value(config, LEGACY_KITTY_GRAPHICS_SECTION, KITTY_GRAPHICS))
}

fn with_kitty_graphics(config: &str, enabled: bool) -> String {
    let config = set_bool(config, KITTY_GRAPHICS_SECTION, KITTY_GRAPHICS, enabled);
    if bool_value(&config, LEGACY_KITTY_GRAPHICS_SECTION, KITTY_GRAPHICS).is_some() {
        set_bool(
            &config,
            LEGACY_KITTY_GRAPHICS_SECTION,
            KITTY_GRAPHICS,
            enabled,
        )
    } else {
        config
    }
}

fn section_header(line: &str) -> Option<&str> {
    let line = line.trim();
    let name = line.strip_prefix('[')?.split(']').next()?;
    Some(if line.starts_with("[[") {
        ""
    } else {
        name.trim()
    })
}

fn assignment<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (name, value) = line.split_once('=')?;
    (name.trim() == key && !name.trim_start().starts_with('#')).then_some(value)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.split('#').next()?.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn bool_value(config: &str, section: &str, key: &str) -> Option<bool> {
    let mut current = "";
    let mut found = None;
    for line in config.lines() {
        if let Some(header) = section_header(line) {
            current = header;
        } else if current == section
            && let Some(value) = assignment(line, key)
        {
            found = parse_bool(value);
        }
    }
    found
}

fn set_bool(config: &str, section: &str, key: &str, value: bool) -> String {
    let mut lines: Vec<String> = config.lines().map(str::to_owned).collect();
    let mut current = String::new();
    let mut header = None;
    let mut assigned = false;
    for (index, line) in lines.iter_mut().enumerate() {
        if let Some(name) = section_header(line) {
            current = name.to_owned();
            if current == section && header.is_none() {
                header = Some(index);
            }
        } else if current == section
            && let Some(old) = assignment(line, key)
        {
            let comment = old.find('#').map(|start| old[start..].to_owned());
            let name = line[..line.len() - old.len() - 1].trim_end().to_owned();
            *line = match comment {
                Some(comment) => format!("{name} = {value} {comment}"),
                None => format!("{name} = {value}"),
            };
            assigned = true;
        }
    }
    if !assigned {
        let entry = format!("{key} = {value}");
        match header {
            Some(index) => lines.insert(index + 1, entry),
            None => {
                if lines.last().is_some_and(|line| !line.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(format!("[{section}]"));
                lines.push(entry);
            }
        }
    }
    let mut updated = lines.join("\n");
    updated.push('\n');
    updated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_terminal_key_before_the_legacy_experimental_key() {
        assert_eq!(configured_kitty_graphics(""), None);
        assert_eq!(
            configured_kitty_graphics("[experimental]\nkitty_graphics = true\n"),
            Some(true)
        );
        assert_eq!(
            configured_kitty_graphics(
                "[experimental]\nkitty_graphics = true\n[terminal]\nkitty_graphics = false # off\n"
            ),
            Some(false)
        );
        assert_eq!(
            configured_kitty_graphics("[terminal]\n# kitty_graphics = true\n"),
            None
        );
        assert_eq!(
            configured_kitty_graphics("[ui]\nkitty_graphics = true\n"),
            None
        );
    }

    #[test]
    fn writes_the_terminal_key_and_keeps_a_legacy_key_in_step() {
        let config = "onboarding = false\n[experimental]\nallow_nested = false\nkitty_graphics = true\n\n[[keys.command]]\nkey = \"prefix+u\"\n";
        let updated = with_kitty_graphics(config, false);
        assert_eq!(
            updated,
            "onboarding = false\n[experimental]\nallow_nested = false\nkitty_graphics = false\n\n[[keys.command]]\nkey = \"prefix+u\"\n\n[terminal]\nkitty_graphics = false\n"
        );
        assert_eq!(configured_kitty_graphics(&updated), Some(false));
        assert_eq!(with_kitty_graphics(&updated, false), updated);
    }

    #[test]
    fn replaces_an_existing_terminal_key_in_place_and_keeps_its_comment() {
        let config = "[terminal]\nshell_mode = \"auto\"\n  kitty_graphics = false  # pane images\n[update]\nchannel = \"stable\"\n";
        assert_eq!(
            with_kitty_graphics(config, true),
            "[terminal]\nshell_mode = \"auto\"\n  kitty_graphics = true # pane images\n[update]\nchannel = \"stable\"\n"
        );
        assert_eq!(
            with_kitty_graphics("[terminal]\nshell_mode = \"auto\"", true),
            "[terminal]\nkitty_graphics = true\nshell_mode = \"auto\"\n"
        );
        assert_eq!(
            with_kitty_graphics("", true),
            "[terminal]\nkitty_graphics = true\n"
        );
    }

    #[test]
    fn saves_through_the_file_system() {
        let directory = std::env::temp_dir().join(format!(
            "herdr-git-config-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("herdr").join("config.toml");
        assert_eq!(kitty_graphics(&path), Ok(None));
        set_kitty_graphics(&path, false).unwrap();
        assert_eq!(kitty_graphics(&path), Ok(Some(false)));
        set_kitty_graphics(&path, true).unwrap();
        assert_eq!(kitty_graphics(&path), Ok(Some(true)));
        assert!(!path.with_extension("toml.herdr-git").exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
