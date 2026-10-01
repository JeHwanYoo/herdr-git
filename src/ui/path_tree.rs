use std::collections::BTreeMap;

type SortKey = (String, String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PathRowKind {
    Directory { key: String, expanded: bool },
    File { index: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PathRow {
    pub(super) depth: usize,
    pub(super) label: String,
    pub(super) kind: PathRowKind,
}

#[derive(Debug, Default)]
pub(super) struct PathTree {
    directories: BTreeMap<SortKey, PathTree>,
    files: BTreeMap<SortKey, usize>,
}

impl PathTree {
    pub(super) fn from_paths<'a>(paths: impl IntoIterator<Item = (usize, &'a str)>) -> Self {
        let mut tree = Self::default();
        for (index, path) in paths {
            tree.insert(path, index);
        }
        tree
    }

    fn insert(&mut self, path: &str, index: usize) {
        match path.split_once('/') {
            Some((directory, rest)) => self
                .directories
                .entry(sort_key(directory))
                .or_default()
                .insert(rest, index),
            None => {
                self.files.insert(sort_key(path), index);
            }
        }
    }

    pub(super) fn emit(
        &self,
        depth: usize,
        prefix: &str,
        is_expanded: &dyn Fn(&str) -> bool,
        rows: &mut Vec<PathRow>,
    ) {
        for ((_, name), child) in &self.directories {
            let key = format!("{prefix}{name}");
            let expanded = is_expanded(&key);
            rows.push(PathRow {
                depth,
                label: name.clone(),
                kind: PathRowKind::Directory {
                    key: key.clone(),
                    expanded,
                },
            });
            if expanded {
                child.emit(depth + 1, &format!("{key}/"), is_expanded, rows);
            }
        }
        for ((_, name), index) in &self.files {
            rows.push(PathRow {
                depth,
                label: name.clone(),
                kind: PathRowKind::File { index: *index },
            });
        }
    }
}

fn sort_key(name: &str) -> SortKey {
    (name.to_lowercase(), name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{PathRowKind, PathTree};

    fn labels(tree: &PathTree, expanded: &[&str]) -> Vec<(usize, String)> {
        let mut rows = Vec::new();
        tree.emit(0, "", &|key| expanded.contains(&key), &mut rows);
        rows.into_iter().map(|row| (row.depth, row.label)).collect()
    }

    #[test]
    fn collapsed_directories_hide_their_children() {
        let paths = ["src/ui/view.rs", "src/main.rs", "README.md"];
        let tree = PathTree::from_paths(paths.iter().copied().enumerate());
        assert_eq!(
            labels(&tree, &[]),
            [(0, "src".to_owned()), (0, "README.md".to_owned())]
        );
        assert_eq!(
            labels(&tree, &["src", "src/ui"]),
            [
                (0, "src".to_owned()),
                (1, "ui".to_owned()),
                (2, "view.rs".to_owned()),
                (1, "main.rs".to_owned()),
                (0, "README.md".to_owned()),
            ]
        );
    }

    #[test]
    fn directory_keys_carry_the_prefix_and_files_keep_their_index() {
        let paths = ["docs/guide.md", "Cargo.toml"];
        let tree = PathTree::from_paths(paths.iter().copied().enumerate());
        let mut rows = Vec::new();
        tree.emit(1, "root:", &|_| true, &mut rows);
        assert_eq!(
            rows[0].kind,
            PathRowKind::Directory {
                key: "root:docs".to_owned(),
                expanded: true
            }
        );
        assert_eq!(rows[1].kind, PathRowKind::File { index: 0 });
        assert_eq!(rows[2].kind, PathRowKind::File { index: 1 });
        assert_eq!(rows[2].depth, 1);
    }
}
