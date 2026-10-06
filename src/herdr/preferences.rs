use std::fs;
use std::path::Path;

const PANE_GRAPHICS: &str = "pane-graphics";

pub(crate) fn pane_graphics_enabled() -> bool {
    super::state_directory().is_none_or(|directory| pane_graphics_enabled_in(&directory))
}

pub(crate) fn save_pane_graphics_enabled(enabled: bool) {
    if let Some(directory) = super::state_directory() {
        save_pane_graphics_enabled_in(&directory, enabled);
    }
}

fn pane_graphics_enabled_in(directory: &Path) -> bool {
    fs::read_to_string(directory.join(PANE_GRAPHICS)).map_or(true, |value| value.trim() != "off")
}

fn save_pane_graphics_enabled_in(directory: &Path, enabled: bool) {
    if fs::create_dir_all(directory).is_ok() {
        let _ = fs::write(
            directory.join(PANE_GRAPHICS),
            if enabled { "on" } else { "off" },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_graphics_default_to_on_and_remember_off() {
        let directory = std::env::temp_dir().join(format!(
            "herdr-git-preferences-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(pane_graphics_enabled_in(&directory));
        save_pane_graphics_enabled_in(&directory, false);
        assert!(!pane_graphics_enabled_in(&directory));
        save_pane_graphics_enabled_in(&directory, true);
        assert!(pane_graphics_enabled_in(&directory));
        fs::remove_dir_all(directory).unwrap();
    }
}
