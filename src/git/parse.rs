use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::{
    BlameInfo, BranchStatus, ChangeSection, ChangedPath, Commit, CommitRef, CommitRefKind,
    DiffSummary, LineChange, LineHistoryCommit, WorkingChange, WorktreeReport,
};

pub(super) fn parse_history(output: &str) -> Result<Vec<Commit>, String> {
    output
        .split('\u{1e}')
        .filter(|record| !record.trim().is_empty())
        .map(|record| {
            let mut fields = record.trim_matches('\n').splitn(8, '\u{1f}');
            let sha = required(&mut fields, "SHA")?;
            let parents = required(&mut fields, "parents")?
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let author_name = required(&mut fields, "author name")?;
            let author_email = required(&mut fields, "author email")?;
            let author_time = required(&mut fields, "author time")?;
            let refs = required(&mut fields, "refs")?
                .split(", ")
                .filter(|value| !value.is_empty())
                .flat_map(parse_refs)
                .collect();
            let subject = required(&mut fields, "subject")?;
            let body = fields.next().unwrap_or_default().trim_end().to_owned();
            Ok(Commit {
                sha,
                parents,
                author_name,
                author_email,
                author_time,
                refs,
                subject,
                body,
            })
        })
        .collect()
}

fn required<'a>(fields: &mut impl Iterator<Item = &'a str>, name: &str) -> Result<String, String> {
    fields
        .next()
        .map(str::to_owned)
        .ok_or_else(|| format!("Git returned a commit without {name}"))
}

fn parse_refs(value: &str) -> Vec<CommitRef> {
    let value = value.trim();
    if let Some(name) = value.strip_prefix("HEAD -> refs/heads/") {
        return vec![
            CommitRef {
                name: "HEAD".to_owned(),
                kind: CommitRefKind::Head,
            },
            CommitRef {
                name: name.to_owned(),
                kind: CommitRefKind::LocalBranch,
            },
        ];
    }
    vec![parse_ref(value)]
}

fn parse_ref(value: &str) -> CommitRef {
    let value = value.trim();
    let (name, kind) = if let Some(name) = value.strip_prefix("refs/heads/") {
        (name, CommitRefKind::LocalBranch)
    } else if let Some(name) = value.strip_prefix("refs/remotes/") {
        let kind = if name.ends_with("/HEAD") {
            CommitRefKind::RemoteHead
        } else {
            CommitRefKind::RemoteBranch
        };
        (name, kind)
    } else if let Some(name) = value.strip_prefix("tag: refs/tags/") {
        (name, CommitRefKind::Tag)
    } else if let Some(name) = value.strip_prefix("refs/tags/") {
        (name, CommitRefKind::Tag)
    } else if value == "HEAD" {
        (value, CommitRefKind::Head)
    } else {
        (value, CommitRefKind::Other)
    };
    CommitRef {
        name: name.to_owned(),
        kind,
    }
}

pub(super) fn parse_iso_time(value: &str) -> Option<i64> {
    let value = value.trim();
    let (date, rest) = value.split_once('T')?;
    let offset_at = rest
        .char_indices()
        .find(|(index, character)| *index > 0 && matches!(character, '+' | '-' | 'Z' | 'z'))
        .map(|(index, _)| index)?;
    let (time, offset) = rest.split_at(offset_at);
    let mut date_fields = date.split('-');
    let year: i64 = date_fields.next()?.parse().ok()?;
    let month: i64 = date_fields.next()?.parse().ok()?;
    let day: i64 = date_fields.next()?.parse().ok()?;
    if date_fields.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut time_fields = time.split(':');
    let hour: i64 = time_fields.next()?.parse().ok()?;
    let minute: i64 = time_fields.next()?.parse().ok()?;
    let second: i64 = time_fields.next()?.split('.').next()?.parse().ok()?;
    if time_fields.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let offset_seconds = parse_utc_offset(offset)?;
    Some(
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
            - offset_seconds,
    )
}

fn parse_utc_offset(offset: &str) -> Option<i64> {
    if matches!(offset, "Z" | "z") {
        return Some(0);
    }
    let sign = match offset.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = offset[1..]
        .chars()
        .filter(|character| *character != ':')
        .collect();
    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3_600 + minutes * 60))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub(super) fn relative_time(seconds_ago: i64) -> String {
    fn count(value: i64, unit: &str) -> String {
        if value == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{value} {unit}s ago")
        }
    }

    if seconds_ago < 0 {
        return "in the future".to_owned();
    }
    if seconds_ago < 90 {
        return count(seconds_ago, "second");
    }
    let minutes = (seconds_ago + 30) / 60;
    if minutes < 90 {
        return count(minutes, "minute");
    }
    let hours = (minutes + 30) / 60;
    if hours < 36 {
        return count(hours, "hour");
    }
    let days = (hours + 12) / 24;
    if days < 14 {
        return count(days, "day");
    }
    if days < 70 {
        return count((days + 3) / 7, "week");
    }
    if days < 365 {
        return count((days + 15) / 30, "month");
    }
    let years = days / 365;
    let months = (days % 365 + 15) / 30;
    if months == 0 {
        count(years, "year")
    } else {
        let years = if years == 1 {
            "1 year".to_owned()
        } else {
            format!("{years} years")
        };
        let months = if months == 1 {
            "1 month".to_owned()
        } else {
            format!("{months} months")
        };
        format!("{years}, {months} ago")
    }
}

pub(super) fn parse_worktree_report(output: &str) -> Result<WorktreeReport, String> {
    let mut oid = None;
    let mut head = None;
    let mut upstream = None;
    let mut ahead_behind = None;
    let mut changed_paths = 0;
    for line in output.lines() {
        if let Some(header) = line.strip_prefix("# ") {
            match header.split_once(' ') {
                Some(("branch.oid", value)) => oid = Some(value),
                Some(("branch.head", value)) => head = Some(value),
                Some(("branch.upstream", value)) => upstream = Some(value.to_owned()),
                Some(("branch.ab", value)) => ahead_behind = Some(value),
                _ => {}
            }
        } else if line.starts_with(['1', '2', 'u', '?']) {
            changed_paths += 1;
        }
    }
    let head = head.ok_or_else(|| "Git returned a status without a branch head".to_owned())?;
    let checked_out = if head == "(detached)" {
        let oid =
            oid.ok_or_else(|| "Git returned a detached status without a commit".to_owned())?;
        format!("Detached {}", oid.get(..8).unwrap_or(oid))
    } else {
        head.to_owned()
    };
    let (ahead, behind) = match ahead_behind {
        Some(value) => {
            let mut counts = value.split_whitespace();
            let ahead = counts
                .next()
                .and_then(|value| value.strip_prefix('+'))
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| "Git returned an invalid ahead count".to_owned())?;
            let behind = counts
                .next()
                .and_then(|value| value.strip_prefix('-'))
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| "Git returned an invalid behind count".to_owned())?;
            (ahead, behind)
        }
        None => {
            upstream = None;
            (0, 0)
        }
    };
    let mut hasher = DefaultHasher::new();
    output.hash(&mut hasher);
    Ok(WorktreeReport {
        branch_status: BranchStatus {
            checked_out,
            upstream,
            ahead,
            behind,
        },
        changed_paths,
        has_untracked: output.lines().any(|line| line.starts_with("? ")),
        fingerprint: hasher.finish(),
    })
}

pub(super) fn parse_changes(output: &str) -> Vec<ChangedPath> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let status = fields.next()?.to_owned();
            let first = fields.next()?.to_owned();
            let path = fields.next().unwrap_or(&first).to_owned();
            Some(ChangedPath { status, path })
        })
        .collect()
}

pub(super) fn parse_working_changes(output: &str, section: ChangeSection) -> Vec<WorkingChange> {
    parse_changes(output)
        .into_iter()
        .map(|change| WorkingChange {
            section,
            status: change.status,
            path: change.path,
        })
        .collect()
}

pub(super) fn parse_numstat(output: &str) -> DiffSummary {
    output
        .lines()
        .fold(DiffSummary::default(), |mut total, line| {
            let mut fields = line.splitn(3, '\t');
            let additions = fields.next();
            let deletions = fields.next();
            let path = fields.next();
            if path.is_some() {
                total.files += 1;
                total.additions += additions.and_then(|value| value.parse().ok()).unwrap_or(0);
                total.deletions += deletions.and_then(|value| value.parse().ok()).unwrap_or(0);
            }
            total
        })
}

pub(super) fn parse_line_changes(output: &str) -> Vec<LineChange> {
    let mut changes: Vec<LineChange> = Vec::new();
    for line in output.lines() {
        if let Some(header) = line.strip_prefix("@@ ") {
            let mut ranges = header.split_whitespace();
            let (Some(old), Some(new)) = (ranges.next(), ranges.next()) else {
                continue;
            };
            let (old_start, old_count) = hunk_range(old.trim_start_matches('-'));
            let (new_start, new_count) = hunk_range(new.trim_start_matches('+'));
            changes.push(LineChange {
                old_start: if old_count == 0 {
                    old_start + 1
                } else {
                    old_start
                },
                removed: Vec::with_capacity(old_count),
                new_start: if new_count == 0 {
                    new_start + 1
                } else {
                    new_start
                },
                added: new_count,
            });
        } else if let Some(change) = changes.last_mut()
            && let Some(removed) = line.strip_prefix('-')
        {
            change
                .removed
                .push(removed.strip_suffix('\r').unwrap_or(removed).to_owned());
        }
    }
    changes
}

fn hunk_range(range: &str) -> (usize, usize) {
    let mut parts = range.splitn(2, ',');
    let start = parts
        .next()
        .and_then(|start| start.parse().ok())
        .unwrap_or(0);
    let count = parts
        .next()
        .map_or(Some(1), |count| count.parse().ok())
        .unwrap_or(0);
    (start, count)
}

pub(super) fn parse_blame_range(output: &str) -> Result<Vec<BlameInfo>, String> {
    let mut records = Vec::new();
    let mut start = 0;
    for (offset, _) in output.match_indices("\n\t") {
        let end = output[offset + 1..]
            .find('\n')
            .map_or(output.len(), |next| offset + 1 + next + 1);
        records.push(parse_blame(&output[start..end])?);
        start = end;
    }
    Ok(records)
}

pub(super) fn parse_line_history(output: &str) -> Result<Vec<LineHistoryCommit>, String> {
    output
        .split('\u{1e}')
        .filter(|record| !record.trim().is_empty())
        .map(|record| {
            let (header, patch) = record.split_once('\n').unwrap_or((record, ""));
            let mut fields = header.split('\u{1f}');
            let mut field = |name: &str| {
                fields
                    .next()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("Git line history omitted {name}"))
            };
            Ok(LineHistoryCommit {
                sha: field("SHA")?,
                author: field("author")?,
                author_time: field("author time")?.parse().unwrap_or_default(),
                summary: field("summary")?,
                patch: patch.trim_start_matches('\n').to_owned(),
            })
        })
        .collect()
}

pub(super) fn parse_blame(output: &str) -> Result<BlameInfo, String> {
    let mut lines = output.lines();
    let header = lines
        .next()
        .ok_or_else(|| "Git returned empty blame output".to_owned())?;
    let mut header_fields = header.split_whitespace();
    let sha = header_fields
        .next()
        .ok_or_else(|| "Git blame omitted SHA".to_owned())?
        .to_owned();
    let _original_line = header_fields.next();
    let line = header_fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "Git blame omitted final line number".to_owned())?;
    let mut author = String::new();
    let mut author_email = String::new();
    let mut author_time = 0;
    let mut summary = String::new();
    for field in lines {
        if field.starts_with('\t') {
            break;
        }
        if let Some(value) = field.strip_prefix("author ") {
            author = value.to_owned();
        } else if let Some(value) = field.strip_prefix("author-mail ") {
            author_email = value.trim_matches(['<', '>']).to_owned();
        } else if let Some(value) = field.strip_prefix("author-time ") {
            author_time = value.parse().unwrap_or_default();
        } else if let Some(value) = field.strip_prefix("summary ") {
            summary = value.to_owned();
        }
    }
    Ok(BlameInfo {
        sha,
        author,
        author_email,
        author_time,
        summary,
        line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_text_numstat_and_counts_binary_files_without_lines() {
        let summary = parse_numstat("3\t1\tsrc/main.rs\n-\t-\timage.png\n5\t0\tREADME.md\n");
        assert_eq!(summary.files, 3);
        assert_eq!(summary.additions, 8);
        assert_eq!(summary.deletions, 1);
    }

    #[test]
    fn parses_exact_commit_fields() {
        let raw = "\u{1e}abc\u{1f}p1 p2\u{1f}Ada\u{1f}ada@example.com\u{1f}2026-01-02T03:04:05+00:00\u{1f}HEAD -> refs/heads/main, refs/remotes/origin/main, refs/remotes/origin/HEAD, tag: refs/tags/v1\u{1f}Subject\u{1f}Body\n";
        let commits = parse_history(raw).unwrap();

        assert_eq!(commits[0].sha, "abc");
        assert_eq!(commits[0].parents, ["p1", "p2"]);
        assert_eq!(commits[0].author_time, "2026-01-02T03:04:05+00:00");
        let two_hours_later =
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_767_323_045 + 2 * 3_600 + 40);
        assert_eq!(commits[0].author_relative(two_hours_later), "2 hours ago");
        assert_eq!(
            commits[0]
                .refs
                .iter()
                .map(|reference| (reference.name.as_str(), reference.kind))
                .collect::<Vec<_>>(),
            [
                ("HEAD", CommitRefKind::Head),
                ("main", CommitRefKind::LocalBranch),
                ("origin/main", CommitRefKind::RemoteBranch),
                ("origin/HEAD", CommitRefKind::RemoteHead),
                ("v1", CommitRefKind::Tag),
            ]
        );
        assert_eq!(commits[0].body, "Body");
    }

    #[test]
    fn parses_iso_8601_author_times_with_any_offset() {
        assert_eq!(parse_iso_time("1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(parse_iso_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso_time("2026-01-02T03:04:05+00:00"),
            Some(1_767_323_045)
        );
        assert_eq!(
            parse_iso_time("2026-01-02T12:04:05+09:00"),
            Some(1_767_323_045)
        );
        assert_eq!(
            parse_iso_time("2026-01-01T22:04:05-05:00"),
            Some(1_767_323_045)
        );
        assert_eq!(
            parse_iso_time("2024-02-29T23:59:60.5-0130"),
            Some(1_709_256_600)
        );
        assert_eq!(parse_iso_time("1969-12-31T23:59:59Z"), Some(-1));
        assert_eq!(parse_iso_time("yesterday"), None);
        assert_eq!(parse_iso_time("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso_time("2026-01-01T00:00:00"), None);
    }

    #[test]
    fn relative_time_uses_git_wording() {
        assert_eq!(relative_time(-5), "in the future");
        assert_eq!(relative_time(0), "0 seconds ago");
        assert_eq!(relative_time(1), "1 second ago");
        assert_eq!(relative_time(89), "89 seconds ago");
        assert_eq!(relative_time(90), "2 minutes ago");
        assert_eq!(relative_time(25 * 60), "25 minutes ago");
        assert_eq!(relative_time(90 * 60), "2 hours ago");
        assert_eq!(relative_time(35 * 3_600), "35 hours ago");
        assert_eq!(relative_time(36 * 3_600), "2 days ago");
        assert_eq!(relative_time(13 * 86_400), "13 days ago");
        assert_eq!(relative_time(14 * 86_400), "2 weeks ago");
        assert_eq!(relative_time(70 * 86_400), "2 months ago");
        assert_eq!(relative_time(364 * 86_400), "12 months ago");
        assert_eq!(relative_time(365 * 86_400), "1 year ago");
        assert_eq!(relative_time(400 * 86_400), "1 year, 1 month ago");
        assert_eq!(relative_time(800 * 86_400), "2 years, 2 months ago");
    }

    #[test]
    fn parses_status_headers_and_counts_one_entry_per_path() {
        let tracking = "# branch.oid febe1a484621b165273ccddbdeb60f9141dddc05\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 MM N... 100644 100644 100644 7898 422c a.txt\n2 R. N... 100644 100644 100644 7898 7898 R100 new.txt\told.txt\nu UU N... 100644 100644 100644 100644 1111 2222 3333 conflict.txt\n? d/x\n! ignored.txt\n";
        let report = parse_worktree_report(tracking).unwrap();
        assert_eq!(report.branch_status.checked_out, "main");
        assert_eq!(
            report.branch_status.upstream.as_deref(),
            Some("origin/main")
        );
        assert_eq!(report.branch_status.ahead, 2);
        assert_eq!(report.branch_status.behind, 1);
        assert_eq!(report.changed_paths, 4);

        let unfetched = "# branch.oid febe1a484621b165273ccddbdeb60f9141dddc05\n# branch.head main\n# branch.upstream origin/main\n";
        let report = parse_worktree_report(unfetched).unwrap();
        assert_eq!(report.branch_status.upstream, None);
        assert_eq!(report.changed_paths, 0);

        let detached =
            "# branch.oid febe1a484621b165273ccddbdeb60f9141dddc05\n# branch.head (detached)\n";
        let report = parse_worktree_report(detached).unwrap();
        assert_eq!(report.branch_status.checked_out, "Detached febe1a48");
        assert_eq!(report.branch_status.upstream, None);

        let unborn = "# branch.oid (initial)\n# branch.head main\n? a.txt\n";
        assert_eq!(
            parse_worktree_report(unborn)
                .unwrap()
                .branch_status
                .checked_out,
            "main"
        );
        assert_ne!(
            parse_worktree_report(tracking).unwrap().fingerprint,
            parse_worktree_report(unfetched).unwrap().fingerprint
        );
        assert!(parse_worktree_report("? a.txt\n").is_err());
    }

    #[test]
    fn preserves_raw_status_and_uses_destination_path_for_renames_and_copies() {
        let changes =
            parse_changes("M\tsrc/main.rs\nR100\told.rs\tnew.rs\nC098\tsrc/main.rs\tsrc/lib.rs\n");
        assert_eq!(changes[1].status, "R100");
        assert_eq!(changes[1].path, "new.rs");
        assert_eq!(changes[2].status, "C098");
        assert_eq!(changes[2].path, "src/lib.rs");
    }

    #[test]
    fn parses_line_porcelain_blame() {
        let raw = "abc123 4 7 1\nauthor Ada Lovelace\nauthor-mail <ada@example.com>\nauthor-time 1700000000\nsummary Explain engine\nfilename src/lib.rs\n\tlet engine = true;\n";
        let blame = parse_blame(raw).unwrap();
        assert_eq!(blame.sha, "abc123");
        assert_eq!(blame.author, "Ada Lovelace");
        assert_eq!(blame.author_email, "ada@example.com");
        assert_eq!(blame.line, 7);
        assert_eq!(blame.summary, "Explain engine");
    }

    #[test]
    fn splits_line_history_into_commits_with_their_patches() {
        let raw = "\u{1e}aaa\u{1f}Ada\u{1f}1700000000\u{1f}Second\n\ndiff --git a/a.rs b/a.rs\n@@ -1 +1 @@\n-one\n+two\n\u{1e}bbb\u{1f}Bob\u{1f}1600000000\u{1f}First\n\ndiff --git a/a.rs b/a.rs\n@@ -0,0 +1 @@\n+one\n";
        let history = parse_line_history(raw).unwrap();
        assert_eq!(
            history
                .iter()
                .map(|commit| (
                    commit.sha.as_str(),
                    commit.author.as_str(),
                    commit.author_time
                ))
                .collect::<Vec<_>>(),
            [("aaa", "Ada", 1_700_000_000), ("bbb", "Bob", 1_600_000_000)]
        );
        assert!(history[0].patch.starts_with("diff --git"));
        assert!(history[0].patch.ends_with("+two\n"));
        assert!(parse_line_history("").unwrap().is_empty());
    }

    #[test]
    fn parses_one_line_porcelain_record_per_line() {
        let raw = "abc123 4 7 2\nauthor Ada\nsummary First\nfilename a.rs\n\tone\nabc123 5 8\nauthor Ada\nsummary First\nfilename a.rs\n\ttwo\n";
        let blame = parse_blame_range(raw).unwrap();
        assert_eq!(
            blame.iter().map(|blame| blame.line).collect::<Vec<_>>(),
            [7, 8]
        );
        assert!(parse_blame_range("").unwrap().is_empty());
    }
}
