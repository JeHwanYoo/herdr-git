use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::{
    BlameInfo, BranchStatus, ChangeSection, ChangedPath, Commit, CommitRef, CommitRefKind,
    DiffSummary, LineHistoryCommit, WorkingChange, WorktreeReport,
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
                graph: super::GraphPrefix::plain("*"),
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

#[cfg(test)]
pub(super) fn parse_graph(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let (lanes, sha) = line.split_once('\u{1e}')?;
            Some((sha.trim().to_owned(), lanes.trim_end().to_owned()))
        })
        .collect()
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
