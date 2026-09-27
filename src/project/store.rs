use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::git::Repository;

const PROJECTS_FILE: &str = "projects.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRegistry {
    state_file: Option<PathBuf>,
    roots: Vec<PathBuf>,
}

impl ProjectRegistry {
    pub fn load(state_file: Option<PathBuf>) -> Result<Self, String> {
        let Some(path) = state_file.as_ref() else {
            return Ok(Self {
                state_file: None,
                roots: Vec::new(),
            });
        };
        let raw = match fs::read(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(format!("Could not read registered projects: {error}")),
        };
        let mut roots = Vec::new();
        if !raw.is_empty() {
            let value: serde_json::Value = serde_json::from_slice(&raw)
                .map_err(|error| format!("Registered projects are invalid: {error}"))?;
            for root in value["projects"].as_array().into_iter().flatten() {
                let Some(root) = root.as_str().map(PathBuf::from) else {
                    continue;
                };
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        Ok(Self { state_file, roots })
    }

    pub fn from_environment() -> Result<Self, String> {
        let state_file = env::var_os("HERDR_PLUGIN_STATE_DIR")
            .map(PathBuf::from)
            .map(|directory| directory.join(PROJECTS_FILE));
        Self::load(state_file)
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn add(&mut self, selected: &Path) -> Result<bool, String> {
        let repository = Repository::discover(selected)
            .map_err(|_| "Choose a directory inside a Git repository.".to_owned())?;
        let root = repository.project_root()?;
        if self.roots.contains(&root) {
            return Ok(false);
        }
        self.roots.push(root);
        if let Err(error) = self.save() {
            self.roots.pop();
            return Err(error);
        }
        Ok(true)
    }

    pub fn remove(&mut self, root: &Path) -> Result<bool, String> {
        let previous = self.roots.len();
        let Some(index) = self.roots.iter().position(|candidate| candidate == root) else {
            return Ok(false);
        };
        let removed = self.roots.remove(index);
        if let Err(error) = self.save() {
            self.roots.insert(index, removed);
            return Err(error);
        }
        debug_assert_eq!(self.roots.len() + 1, previous);
        Ok(true)
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = self.state_file.as_ref() else {
            return Err("HERDR_PLUGIN_STATE_DIR is not set; Projects are not saved".to_owned());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Could not create plugin state: {error}"))?;
        }
        let projects = self
            .roots
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec_pretty(&json!({ "projects": projects }))
            .map_err(|error| format!("Could not encode registered projects: {error}"))?;
        fs::write(path, bytes)
            .map_err(|error| format!("Could not save registered projects: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::ProjectRegistry;

    #[test]
    fn add_without_a_state_directory_fails_and_registers_nothing() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-unsaved-{unique}"));
        fs::create_dir_all(&root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-b", "main"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let mut registry = ProjectRegistry::load(None).unwrap();

        assert_eq!(
            registry.add(&root).unwrap_err(),
            "HERDR_PLUGIN_STATE_DIR is not set; Projects are not saved"
        );
        assert!(registry.roots().is_empty());
        assert!(!registry.remove(&root).unwrap());

        fs::remove_dir_all(root).unwrap();
    }
}
