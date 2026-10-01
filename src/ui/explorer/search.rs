use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use grep::matcher::Matcher;
use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::sinks::Lossy;
use grep::searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;

pub(super) const PATH_RESULT_LIMIT: usize = 200;
pub(super) const CONTENT_RESULT_LIMIT: usize = 1000;
const MAX_SEARCHED_FILE_SIZE: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MatchType {
    Filename,
    Path,
    Content,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SearchResult {
    pub(super) path: String,
    pub(super) line: Option<u64>,
    pub(super) column: Option<usize>,
    pub(super) match_range: Option<Range<usize>>,
    pub(super) match_type: MatchType,
}

#[derive(Debug)]
pub(super) struct SearchUpdate {
    pub(super) generation: u64,
    pub(super) root: PathBuf,
    pub(super) results: Vec<SearchResult>,
    pub(super) done: bool,
    pub(super) error: Option<String>,
}

struct SearchRequest {
    generation: u64,
    root: PathBuf,
    query: String,
}

pub(super) struct SearchService {
    requests: Sender<SearchRequest>,
    updates: Receiver<SearchUpdate>,
    generation: Arc<AtomicU64>,
}

impl SearchService {
    pub(super) fn start() -> Result<Self, String> {
        let (requests, request_rx) = mpsc::channel::<SearchRequest>();
        let (update_tx, updates) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let current = Arc::clone(&generation);
        thread::Builder::new()
            .name("herdr-git-search".to_owned())
            .spawn(move || {
                while let Ok(mut request) = request_rx.recv() {
                    while let Ok(newer) = request_rx.try_recv() {
                        request = newer;
                    }
                    let cancel = Cancel {
                        generation: &current,
                        expected: request.generation,
                    };
                    if cancel.is_cancelled() {
                        continue;
                    }
                    let mut send = |results: Vec<SearchResult>, done: bool, error| {
                        update_tx
                            .send(SearchUpdate {
                                generation: request.generation,
                                root: request.root.clone(),
                                results,
                                done,
                                error,
                            })
                            .is_ok()
                    };
                    if let Err(error) = search(&request.root, &request.query, &cancel, &mut send)
                        && !cancel.is_cancelled()
                    {
                        send(Vec::new(), true, Some(error));
                    }
                }
            })
            .map_err(|error| format!("Could not start the search worker: {error}"))?;
        Ok(Self {
            requests,
            updates,
            generation,
        })
    }

    pub(super) fn search(&self, root: PathBuf, query: String) -> Result<u64, String> {
        let generation = self.cancel();
        self.requests
            .send(SearchRequest {
                generation,
                root,
                query,
            })
            .map_err(|_| "The search worker stopped".to_owned())?;
        Ok(generation)
    }

    pub(super) fn cancel(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(super) fn try_recv(&self) -> Option<SearchUpdate> {
        self.updates.try_recv().ok()
    }
}

struct Cancel<'a> {
    generation: &'a AtomicU64,
    expected: u64,
}

impl Cancel<'_> {
    fn is_cancelled(&self) -> bool {
        self.generation.load(Ordering::Acquire) != self.expected
    }
}

fn search(
    root: &Path,
    query: &str,
    cancel: &Cancel<'_>,
    send: &mut dyn FnMut(Vec<SearchResult>, bool, Option<String>) -> bool,
) -> Result<(), String> {
    let files = walk_files(root, cancel)?;
    if cancel.is_cancelled() {
        return Ok(());
    }
    let mut results = rank_paths(&files, query);
    if !send(results.clone(), false, None) {
        return Ok(());
    }
    results.extend(search_contents(root, &files, query, cancel)?);
    if !cancel.is_cancelled() {
        send(results, true, None);
    }
    Ok(())
}

fn walk_files(root: &Path, cancel: &Cancel<'_>) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    for entry in WalkBuilder::new(root).build() {
        if cancel.is_cancelled() {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(root) {
            let path = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn rank_paths(files: &[String], query: &str) -> Vec<SearchResult> {
    let needle = query.to_lowercase();
    let mut ranked = files
        .iter()
        .filter_map(|path| {
            let lower = path.to_lowercase();
            let name_start = lower.rfind('/').map_or(0, |slash| slash + 1);
            let name = &lower[name_start..];
            let (rank, spread, match_type, range) = if name == needle {
                (0, 0, MatchType::Filename, Some(name_start..lower.len()))
            } else if name.starts_with(&needle) {
                let end = name_start + needle.len();
                (1, 0, MatchType::Filename, Some(name_start..end))
            } else if let Some(offset) = name.find(&needle) {
                let start = name_start + offset;
                (2, 0, MatchType::Filename, Some(start..start + needle.len()))
            } else if let Some(spread) = fuzzy_spread(name, &needle) {
                (3, spread, MatchType::Filename, None)
            } else if let Some(start) = lower.find(&needle) {
                (4, 0, MatchType::Path, Some(start..start + needle.len()))
            } else {
                let spread = fuzzy_spread(&lower, &needle)?;
                (4, spread + 1, MatchType::Path, None)
            };
            let range = range.filter(|_| lower.len() == path.len());
            Some(((rank, spread, path.len()), path, match_type, range))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
    ranked
        .into_iter()
        .take(PATH_RESULT_LIMIT)
        .map(|(_, path, match_type, match_range)| SearchResult {
            path: path.clone(),
            line: None,
            column: None,
            match_range,
            match_type,
        })
        .collect()
}

fn fuzzy_spread(haystack: &str, needle: &str) -> Option<usize> {
    let mut wanted = needle.chars().peekable();
    let mut first = None;
    for (index, character) in haystack.chars().enumerate() {
        if wanted.peek() == Some(&character) {
            first.get_or_insert(index);
            wanted.next();
            if wanted.peek().is_none() {
                return Some(index - first.unwrap_or(index));
            }
        }
    }
    None
}

fn content_matcher(query: &str) -> Result<RegexMatcher, String> {
    RegexMatcherBuilder::new()
        .case_smart(true)
        .fixed_strings(true)
        .line_terminator(Some(b'\n'))
        .build(query)
        .map_err(|error| error.to_string())
}

fn search_contents(
    root: &Path,
    files: &[String],
    query: &str,
    cancel: &Cancel<'_>,
) -> Result<Vec<SearchResult>, String> {
    let matcher = content_matcher(query)?;
    let next = AtomicUsize::new(0);
    let found = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let workers = thread::available_parallelism().map_or(1, usize::from);
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut searcher = SearcherBuilder::new()
                    .binary_detection(BinaryDetection::quit(b'\0'))
                    .line_number(true)
                    .build();
                let stop = || {
                    cancel.is_cancelled() || found.load(Ordering::Relaxed) >= CONTENT_RESULT_LIMIT
                };
                while !stop() {
                    let Some(path) = files.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    let full = root.join(path);
                    if full
                        .metadata()
                        .is_ok_and(|metadata| metadata.len() > MAX_SEARCHED_FILE_SIZE)
                    {
                        continue;
                    }
                    let mut local = Vec::new();
                    let sink = Lossy(|line_number, line: &str| {
                        if let Ok(Some(matched)) = matcher.find(line.as_bytes()) {
                            local.push(SearchResult {
                                path: path.clone(),
                                line: Some(line_number),
                                column: Some(line[..matched.start()].chars().count() + 1),
                                match_range: Some(matched.start()..matched.end()),
                                match_type: MatchType::Content,
                            });
                        }
                        Ok(!stop())
                    });
                    if searcher.search_path(&matcher, &full, sink).is_ok() && !local.is_empty() {
                        found.fetch_add(local.len(), Ordering::Relaxed);
                        results.lock().expect("search results lock").extend(local);
                    }
                }
            });
        }
    });
    let mut results = results.into_inner().expect("search results lock");
    results.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.line.cmp(&right.line))
    });
    results.truncate(CONTENT_RESULT_LIMIT);
    Ok(results)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{Cancel, MatchType, fuzzy_spread, rank_paths, search};

    fn paths(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn path_ranking_prefers_filename_exact_prefix_substring_fuzzy_then_path() {
        let files = paths(&[
            "docs/cargo-guide/readme.md",
            "src/scargo.rs",
            "Cargo.lock",
            "cargo",
            "src/cxaxrxgxo.rs",
            "crates/cargo_util.rs",
        ]);
        let ranked = rank_paths(&files, "cargo");
        assert_eq!(
            ranked
                .iter()
                .map(|result| result.path.as_str())
                .collect::<Vec<_>>(),
            [
                "cargo",
                "Cargo.lock",
                "crates/cargo_util.rs",
                "src/scargo.rs",
                "src/cxaxrxgxo.rs",
                "docs/cargo-guide/readme.md",
            ]
        );
        assert_eq!(ranked[0].match_type, MatchType::Filename);
        assert_eq!(ranked[5].match_type, MatchType::Path);
        assert_eq!(ranked[1].match_range, Some(0..5));
    }

    #[test]
    fn fuzzy_spread_measures_the_matched_span() {
        assert_eq!(fuzzy_spread("main.rs", "mrs"), Some(6));
        assert_eq!(fuzzy_spread("main.rs", "ma"), Some(1));
        assert_eq!(fuzzy_spread("main.rs", "rsm"), None);
    }

    #[test]
    fn search_lists_paths_first_then_content_lines_and_skips_ignored_files() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-search-{unique}"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(
            root.join("src/app.rs"),
            "fn main() {\n    let cargo = \"Cargo\";\n}\n",
        )
        .unwrap();
        fs::write(root.join("src/data.bin"), b"cargo\0binary").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/cargo.txt"), "cargo\n").unwrap();
        let generation = AtomicU64::new(1);
        let cancel = Cancel {
            generation: &generation,
            expected: 1,
        };
        let mut updates = Vec::new();
        search(&root, "cargo", &cancel, &mut |results, done, error| {
            updates.push((results, done, error));
            true
        })
        .unwrap();
        let (results, done, error) = updates.last().unwrap();
        assert!(*done && error.is_none());
        assert_eq!(updates.len(), 2);
        let rendered = results
            .iter()
            .map(|result| match result.line {
                Some(line) => format!("{}:{line}", result.path),
                None => result.path.clone(),
            })
            .collect::<Vec<_>>();
        assert_eq!(rendered, ["Cargo.toml", "src/app.rs:2"]);
        let content = &results[1];
        assert_eq!(content.match_type, MatchType::Content);
        assert_eq!(content.column, Some(9));
        assert_eq!(content.match_range, Some(8..13));
        fs::remove_dir_all(root).unwrap();
    }
}
