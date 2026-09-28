use std::collections::HashMap;
use std::path::PathBuf;

use super::parse::parse_history;
use super::process::{GitLines, git};
use super::{Commit, Repository};

pub const HISTORY_PAGE_SIZE: usize = 100;
const FIELDS: &str = "--format=%x1e%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%D%x1f%s%x1f%b";

#[derive(Debug)]
pub struct HistoryPage {
    pub commits: Vec<Commit>,
    pub offset: usize,
    pub has_more: bool,
}

pub struct HistorySession {
    root: PathBuf,
    stream: GitLines,
    lookahead: Option<String>,
    offset: usize,
}

impl HistorySession {
    fn next_sha(&mut self) -> Result<Option<String>, String> {
        while let Some(line) = self.stream.next()? {
            let sha = line.trim();
            if !sha.is_empty() {
                return Ok(Some(sha.to_owned()));
            }
        }
        Ok(None)
    }

    pub fn next_page(&mut self, count: usize) -> Result<HistoryPage, String> {
        if count == 0 {
            return Err("History page size must be positive".to_owned());
        }
        let mut rows = Vec::with_capacity(count);
        if let Some(row) = self.lookahead.take() {
            rows.push(row);
        }
        while rows.len() < count {
            let Some(row) = self.next_sha()? else {
                break;
            };
            rows.push(row);
        }
        self.lookahead = self.next_sha()?;
        let mut commits = Vec::with_capacity(rows.len());
        if !rows.is_empty() {
            let mut args = vec!["log", "--no-walk=unsorted", "--decorate=full", FIELDS];
            args.extend(rows.iter().map(String::as_str));
            args.push("--");
            let mut metadata: HashMap<_, _> = parse_history(&git(&self.root, &args)?)?
                .into_iter()
                .map(|commit| (commit.sha.clone(), commit))
                .collect();
            for sha in rows {
                let commit = metadata
                    .remove(&sha)
                    .ok_or_else(|| format!("Missing commit {sha}; Refresh to retry"))?;
                commits.push(commit);
            }
        }
        let offset = self.offset;
        self.offset += commits.len();
        Ok(HistoryPage {
            commits,
            offset,
            has_more: self.lookahead.is_some(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct HistoryMaintenanceTarget {
    pub common_dir: PathBuf,
    pub has_graph: bool,
}

impl Repository {
    #[cfg(test)]
    pub fn history_session(&self) -> Result<HistorySession, String> {
        self.history_session_from(None)
    }

    pub fn history_session_from(&self, start: Option<&str>) -> Result<HistorySession, String> {
        Ok(HistorySession {
            root: self.root().to_owned(),
            stream: GitLines::start(
                self.root(),
                &[
                    "log",
                    start.unwrap_or("--all"),
                    "--topo-order",
                    "--date-order",
                    "--format=%H",
                ],
            )?,
            lookahead: None,
            offset: 0,
        })
    }

    pub fn history_maintenance_target(&self) -> Result<Option<HistoryMaintenanceTarget>, String> {
        if git(self.root(), &["config", "--bool", "core.commitGraph"])
            .is_ok_and(|v| v.trim() == "false")
            || git(
                self.root(),
                &["config", "--int", "maintenance.commit-graph.auto"],
            )
            .is_ok_and(|v| v.trim() == "0")
            || git(self.root(), &["rev-parse", "--is-shallow-repository"])?.trim() == "true"
            || !git(
                self.root(),
                &["for-each-ref", "--format=%(refname)", "refs/replace/"],
            )?
            .trim()
            .is_empty()
        {
            return Ok(None);
        }
        if git(self.root(), &["show-ref", "--head"]).is_err() {
            return Ok(None);
        }
        let common = git(
            self.root(),
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common_dir = std::fs::canonicalize(common.trim()).map_err(|e| e.to_string())?;
        if common_dir.join("info/grafts").is_file() {
            return Ok(None);
        }
        let objects = git(
            self.root(),
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ],
        )?;
        let info = PathBuf::from(objects.trim()).join("info");
        Ok(Some(HistoryMaintenanceTarget {
            common_dir,
            has_graph: info.join("commit-graph").is_file()
                || info.join("commit-graphs/commit-graph-chain").is_file(),
        }))
    }

    pub fn maintain_history(&self, target: &HistoryMaintenanceTarget) -> Result<(), String> {
        let mut args = vec![
            "-c",
            "maintenance.autoDetach=false",
            "-c",
            "gc.autoDetach=false",
            "maintenance",
            "run",
            "--task=commit-graph",
            "--quiet",
        ];
        if target.has_graph {
            args.push("--auto");
        }
        git(self.root(), &args).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture() -> Repository {
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "herdr-history-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        let mut input = String::new();
        for i in 1..=205 {
            let message = format!("Subject {i}\n\nBody {i}\nsecond line\n");
            input.push_str(&format!("commit refs/heads/main\nmark :{i}\ncommitter Test <test@example.com> {} +0000\ndata {}\n{}", 1700000000 + i, message.len(), message));
            if i > 1 {
                input.push_str(&format!("from :{}\n", i - 1));
            }
            input.push('\n');
        }
        input.push_str("commit refs/heads/feature\nmark :206\ncommitter Test <test@example.com> 1700000206 +0000\ndata 7\nFeature\nfrom :100\n\ncommit refs/heads/main\nmark :207\ncommitter Test <test@example.com> 1700000207 +0000\ndata 5\nMerge\nfrom :205\nmerge :206\n\n");
        let mut child = Command::new("git")
            .current_dir(&root)
            .args(["fast-import", "--quiet"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        Repository::discover(&root).unwrap()
    }

    #[test]
    fn history_pages_preserve_graph_order_and_multiline_metadata() {
        let repo = fixture();
        let before = repo.status_and_refs().unwrap();
        let expected: Vec<String> = super::super::process::git(
            repo.root(),
            &[
                "log",
                "--all",
                "--topo-order",
                "--date-order",
                "--graph",
                "--format=%x1e%H",
            ],
        )
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('\u{1e}'))
        .map(|(_, sha)| sha.to_owned())
        .collect();
        let mut session = repo.history_session().unwrap();
        let first = session.next_page(100).unwrap();
        assert_eq!(first.offset, 0);
        assert_eq!(first.commits.len(), 100);
        assert!(first.has_more);
        let second = session.next_page(100).unwrap();
        assert_eq!(second.offset, 100);
        assert_eq!(second.commits.len(), 100);
        assert!(second.has_more);
        let last = session.next_page(100).unwrap();
        assert_eq!(last.offset, 200);
        assert_eq!(last.commits.len(), 7);
        assert!(!last.has_more);
        let commits: Vec<_> = first
            .commits
            .into_iter()
            .chain(second.commits)
            .chain(last.commits)
            .collect();
        assert_eq!(
            commits.iter().map(|c| c.sha.clone()).collect::<Vec<_>>(),
            expected
        );
        assert!(commits.iter().any(|commit| commit.parents.len() == 2));
        assert!(commits.iter().any(|c| c.body == "Body 100\nsecond line"));
        assert_eq!(session.next_page(100).unwrap().commits.len(), 0);
        drop(session);
        assert_eq!(repo.status_and_refs().unwrap(), before);
        std::fs::remove_dir_all(repo.root()).unwrap();
    }

    #[test]
    fn history_exact_boundary_and_drop_reap_the_stream() {
        let repo = fixture();
        let mut session = repo.history_session().unwrap();
        let pid = session.stream.pid();
        let page = session.next_page(207).unwrap();
        assert!(!page.has_more);
        assert_eq!(page.commits.len(), 207);
        assert!(session.next_page(0).is_err());
        drop(session);
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        let mut session = repo.history_session().unwrap();
        session.next_page(1).unwrap();
        let pid = session.stream.pid();
        drop(session);
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        std::fs::remove_dir_all(repo.root()).unwrap();
    }

    #[test]
    fn linked_worktrees_resolve_the_same_maintenance_cache_and_disabled_auto_skips() {
        let repo = fixture();
        let linked = repo.root().with_extension("worktree");
        super::super::process::git(
            repo.root(),
            &[
                "worktree",
                "add",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        )
        .unwrap();
        let other = Repository::discover(&linked).unwrap();
        let target = repo.history_maintenance_target().unwrap().unwrap();
        assert_eq!(
            other
                .history_maintenance_target()
                .unwrap()
                .unwrap()
                .common_dir,
            target.common_dir
        );
        other.maintain_history(&target).unwrap();
        assert!(
            repo.history_maintenance_target()
                .unwrap()
                .unwrap()
                .has_graph
        );
        super::super::process::git(
            repo.root(),
            &["config", "maintenance.commit-graph.auto", "0"],
        )
        .unwrap();
        assert!(other.history_maintenance_target().unwrap().is_none());
        super::super::process::git(
            repo.root(),
            &["worktree", "remove", linked.to_str().unwrap()],
        )
        .unwrap();
        std::fs::remove_dir_all(repo.root()).unwrap();
    }

    #[test]
    #[ignore = "requires HERDR_HISTORY_BENCH_REPO; writes the Git acceleration cache"]
    fn benchmark_large_repository_history() {
        let path = std::env::var_os("HERDR_HISTORY_BENCH_REPO").expect("benchmark repository");
        let repo = Repository::discover(std::path::Path::new(&path)).unwrap();
        let before = repo.status_and_refs().unwrap();
        if let Some(target) = repo.history_maintenance_target().unwrap() {
            let start = std::time::Instant::now();
            repo.maintain_history(&target).unwrap();
            eprintln!("maintenance_ms={}", start.elapsed().as_millis());
        }
        let mut session = repo.history_session().unwrap();
        for i in 0..10 {
            let start = std::time::Instant::now();
            let page = session.next_page(HISTORY_PAGE_SIZE).unwrap();
            eprintln!(
                "page={} count={} elapsed_ms={}",
                i + 1,
                page.commits.len(),
                start.elapsed().as_millis()
            );
            if !page.has_more {
                break;
            }
        }
        drop(session);
        assert_eq!(repo.status_and_refs().unwrap(), before);
    }

    #[test]
    fn maintenance_creates_shared_cache_without_changing_repository_data() {
        let repo = fixture();
        let before = repo.status_and_refs().unwrap();
        let config = std::fs::read(repo.root().join(".git/config")).unwrap();
        let target = repo.history_maintenance_target().unwrap().unwrap();
        assert!(!target.has_graph);
        let lock = target.common_dir.join("objects/maintenance.lock");
        std::fs::write(&lock, "another maintenance owns this lock").unwrap();
        repo.maintain_history(&target).unwrap();
        assert!(
            !repo
                .history_maintenance_target()
                .unwrap()
                .unwrap()
                .has_graph
        );
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            "another maintenance owns this lock"
        );
        std::fs::remove_file(lock).unwrap();
        repo.maintain_history(&target).unwrap();
        let updated = repo.history_maintenance_target().unwrap().unwrap();
        assert!(updated.has_graph);
        assert_eq!(updated.common_dir, target.common_dir);
        repo.maintain_history(&updated).unwrap();
        assert_eq!(repo.status_and_refs().unwrap(), before);
        assert_eq!(
            std::fs::read(repo.root().join(".git/config")).unwrap(),
            config
        );
        super::super::process::git(repo.root(), &["config", "core.commitGraph", "false"]).unwrap();
        assert!(repo.history_maintenance_target().unwrap().is_none());
        std::fs::remove_dir_all(repo.root()).unwrap();
    }
}
