use std::collections::HashSet;

use crate::git::{ChangeSection, WorkingChange};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) enum TreeRowKind {
    Directory {
        key: String,
        expanded: bool,
        section: Option<ChangeSection>,
    },
    File {
        change_index: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) struct TreeRow {
    pub(in crate::ui) depth: usize,
    pub(in crate::ui) label: String,
    pub(in crate::ui) kind: TreeRowKind,
}

#[derive(Default)]
struct DirectoryNode {
    directories: Vec<(String, DirectoryNode)>,
    files: Vec<(String, usize)>,
}

impl DirectoryNode {
    fn insert(&mut self, components: &[&str], change_index: usize) {
        match components {
            [] => {}
            [name] => self.files.push(((*name).to_owned(), change_index)),
            [head, rest @ ..] => {
                let position = match self.directories.iter().position(|(name, _)| name == head) {
                    Some(position) => position,
                    None => {
                        self.directories.push(((*head).to_owned(), Self::default()));
                        self.directories.len() - 1
                    }
                };
                self.directories[position].1.insert(rest, change_index);
            }
        }
    }

    fn emit(
        &self,
        depth: usize,
        prefix: &str,
        section_key: &str,
        collapsed: &HashSet<String>,
        result: &mut Vec<TreeRow>,
    ) {
        let mut directories = self.directories.iter().collect::<Vec<_>>();
        directories.sort_by_cached_key(|(name, _)| (name.to_lowercase(), name.clone()));
        for (name, child) in directories {
            let key = format!("{section_key}{prefix}{name}");
            let expanded = !collapsed.contains(&key);
            result.push(TreeRow {
                depth,
                label: name.clone(),
                kind: TreeRowKind::Directory {
                    key,
                    expanded,
                    section: None,
                },
            });
            if expanded {
                child.emit(
                    depth + 1,
                    &format!("{prefix}{name}/"),
                    section_key,
                    collapsed,
                    result,
                );
            }
        }
        let mut files = self.files.iter().collect::<Vec<_>>();
        files.sort_by_cached_key(|(name, _)| (name.to_lowercase(), name.clone()));
        for (name, change_index) in files {
            result.push(TreeRow {
                depth,
                label: name.clone(),
                kind: TreeRowKind::File {
                    change_index: *change_index,
                },
            });
        }
    }
}

pub(in crate::ui) fn rows(changes: &[WorkingChange], collapsed: &HashSet<String>) -> Vec<TreeRow> {
    let mut result = Vec::new();
    for section in [
        ChangeSection::Working,
        ChangeSection::Commit,
        ChangeSection::Staged,
        ChangeSection::Unstaged,
    ] {
        let mut root = DirectoryNode::default();
        let mut members = 0;
        for (change_index, change) in changes
            .iter()
            .enumerate()
            .filter(|(_, change)| change.section == section)
        {
            root.insert(&change.path.split('/').collect::<Vec<_>>(), change_index);
            members += 1;
        }
        if members == 0 {
            continue;
        }
        let section_key = format!("{}:", section_id(section));
        let section_collapsed = collapsed.contains(&section_key);
        result.push(TreeRow {
            depth: 0,
            label: section_label(section).to_owned(),
            kind: TreeRowKind::Directory {
                key: section_key.clone(),
                expanded: !section_collapsed,
                section: Some(section),
            },
        });
        if section_collapsed {
            continue;
        }
        root.emit(1, "", &section_key, collapsed, &mut result);
    }
    result
}

fn section_id(section: ChangeSection) -> &'static str {
    match section {
        ChangeSection::Working => "working",
        ChangeSection::Commit => "commit",
        ChangeSection::Staged => "staged",
        ChangeSection::Unstaged => "unstaged",
    }
}

fn section_label(section: ChangeSection) -> &'static str {
    match section {
        ChangeSection::Working => "Working changes",
        ChangeSection::Commit => "Commit changes",
        ChangeSection::Staged => "Staged",
        ChangeSection::Unstaged => "Unstaged",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{TreeRowKind, rows};
    use crate::git::{ChangeSection, WorkingChange};

    fn change(section: ChangeSection, path: &str) -> WorkingChange {
        WorkingChange {
            section,
            status: "M".to_owned(),
            path: path.to_owned(),
        }
    }

    fn labels(changes: &[WorkingChange], collapsed: &HashSet<String>) -> Vec<String> {
        rows(changes, collapsed)
            .into_iter()
            .map(|row| row.label)
            .collect()
    }

    #[test]
    fn builds_section_directory_and_file_rows() {
        let changes = vec![
            change(ChangeSection::Staged, "src/app/main.rs"),
            change(ChangeSection::Staged, "src/lib.rs"),
            change(ChangeSection::Unstaged, "README.md"),
        ];
        assert_eq!(
            labels(&changes, &HashSet::new()),
            [
                "Staged",
                "src",
                "app",
                "main.rs",
                "lib.rs",
                "Unstaged",
                "README.md"
            ]
        );
    }

    #[test]
    fn collapsed_directory_hides_descendants_only() {
        let changes = vec![
            change(ChangeSection::Staged, "src/app/main.rs"),
            change(ChangeSection::Staged, "tests/main.rs"),
        ];
        let collapsed = HashSet::from(["staged:src".to_owned()]);
        assert_eq!(
            labels(&changes, &collapsed),
            ["Staged", "src", "tests", "main.rs"]
        );
        let tree = rows(&changes, &collapsed);
        assert!(matches!(
            tree[1].kind,
            TreeRowKind::Directory {
                expanded: false,
                ..
            }
        ));
    }

    #[test]
    fn sections_emit_in_order_with_their_keys_and_sections() {
        let changes = vec![
            change(ChangeSection::Unstaged, "d.rs"),
            change(ChangeSection::Staged, "c.rs"),
            change(ChangeSection::Commit, "b.rs"),
            change(ChangeSection::Working, "a.rs"),
        ];
        let tree = rows(&changes, &HashSet::new());
        let sections = tree
            .iter()
            .filter_map(|row| match &row.kind {
                TreeRowKind::Directory {
                    key,
                    section: Some(section),
                    ..
                } => Some((row.label.as_str(), key.as_str(), *section)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sections,
            [
                ("Working changes", "working:", ChangeSection::Working),
                ("Commit changes", "commit:", ChangeSection::Commit),
                ("Staged", "staged:", ChangeSection::Staged),
                ("Unstaged", "unstaged:", ChangeSection::Unstaged),
            ]
        );
    }

    #[test]
    fn file_rows_carry_their_change_index_and_directories_carry_no_section() {
        let changes = vec![
            change(ChangeSection::Unstaged, "src/b.rs"),
            change(ChangeSection::Staged, "a.rs"),
        ];
        let tree = rows(&changes, &HashSet::new());
        let files = tree
            .iter()
            .filter_map(|row| match row.kind {
                TreeRowKind::File { change_index } => Some((row.label.as_str(), change_index)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(files, [("a.rs", 1), ("b.rs", 0)]);
        for (label, index) in files {
            assert_eq!(changes[index].path.rsplit('/').next(), Some(label));
        }
        let plain = tree
            .iter()
            .filter(|row| row.label == "src")
            .map(|row| &row.kind)
            .collect::<Vec<_>>();
        assert!(matches!(
            plain.as_slice(),
            [TreeRowKind::Directory { section: None, .. }]
        ));
    }

    #[test]
    fn directories_sort_before_files_case_insensitively() {
        let changes = vec![
            change(ChangeSection::Unstaged, "zeta.txt"),
            change(ChangeSection::Unstaged, "Beta/one.rs"),
            change(ChangeSection::Unstaged, "alpha.txt"),
            change(ChangeSection::Unstaged, "beta.rs"),
            change(ChangeSection::Unstaged, "alpha/two.rs"),
        ];
        let tree = rows(&changes, &HashSet::new());
        assert_eq!(
            tree.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            [
                "Unstaged",
                "alpha",
                "two.rs",
                "Beta",
                "one.rs",
                "alpha.txt",
                "beta.rs",
                "zeta.txt"
            ]
        );
        assert_eq!(
            tree.iter().map(|row| row.depth).collect::<Vec<_>>(),
            [0, 1, 2, 1, 2, 1, 1, 1]
        );
        assert!(matches!(
            &tree[3].kind,
            TreeRowKind::Directory { key, .. } if key == "unstaged:Beta"
        ));
    }
}
