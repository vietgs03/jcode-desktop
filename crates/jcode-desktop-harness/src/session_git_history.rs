//! Read-only git history for the Review view: commit log with a lane graph,
//! single commits, branch comparisons and the default base branch.
//!
//! Every function runs git through [`Location`], so WSL checkouts work the
//! same as local ones. Callers must stay off the UI thread.

use crate::session_git::{Location, clip_patch};

/// Commits loaded per page of the History tab.
pub const LOG_PAGE: usize = 200;

const FIELD: char = '\u{1f}';
const RECORD: char = '\u{1e}';

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    pub parents: Vec<String>,
    /// Decorations such as `HEAD -> main`, `origin/main`, `tag: v1`.
    pub refs: Vec<String>,
    pub subject: String,
    pub author: String,
    /// Author time, seconds since the epoch.
    pub time: i64,
}

impl Commit {
    pub fn short(&self) -> &str {
        &self.hash[..self.hash.len().min(8)]
    }
}

const LOG_FORMAT: &str = "--format=%H%x1f%P%x1f%D%x1f%s%x1f%an%x1f%at%x1e";

/// Parse `git log` records produced with [`LOG_FORMAT`].
pub fn parse_log(raw: &str) -> Vec<Commit> {
    raw.split(RECORD)
        .filter_map(|record| {
            let record = record.trim_start_matches(['\n', '\r']);
            if record.trim().is_empty() {
                return None;
            }
            let mut fields = record.split(FIELD);
            let hash = fields.next()?.trim().to_owned();
            if hash.len() < 7 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let parents = fields
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let refs = fields
                .next()
                .unwrap_or_default()
                .split(", ")
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(str::to_owned)
                .collect();
            let subject = fields.next().unwrap_or_default().to_owned();
            let author = fields.next().unwrap_or_default().to_owned();
            let time = fields
                .next()
                .unwrap_or_default()
                .trim()
                .parse()
                .unwrap_or_default();
            Some(Commit {
                hash,
                parents,
                refs,
                subject,
                author,
                time,
            })
        })
        .collect()
}

/// One page of history across all local and remote branches, newest first
/// in date order (children always above parents).
pub fn log_graph(
    location: &Location,
    directory: &str,
    skip: usize,
    limit: usize,
) -> Result<Vec<Commit>, String> {
    let skip = format!("--skip={skip}");
    let limit = format!("--max-count={limit}");
    let raw = location.git(
        directory,
        &[
            "log",
            "--date-order",
            "--branches",
            "--remotes",
            "--tags",
            "HEAD",
            "--decorate=short",
            LOG_FORMAT,
            &skip,
            &limit,
        ],
    );
    match raw {
        Ok(raw) => Ok(parse_log(&raw)),
        // A repository without commits has no HEAD.
        Err(error) if error.contains("does not have any commits") || error.contains("unknown revision") => {
            Ok(Vec::new())
        }
        Err(error) => Err(error),
    }
}

/// Where a line of the graph starts or ends inside its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Top,
    Middle,
    Bottom,
}

/// A straight line inside one graph row between two lanes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub from_lane: usize,
    pub from: Edge,
    pub to_lane: usize,
    pub to: Edge,
    /// Colour index: the lane that owns the line.
    pub color: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphRow {
    /// Lane of the commit dot.
    pub lane: usize,
    /// Lanes in use in this row, for sizing.
    pub width: usize,
    pub segments: Vec<Segment>,
}

/// Assign lanes so each commit sits on the line of the child that expects it.
/// Commits must be in an order where children come before parents.
pub fn layout_graph(commits: &[Commit]) -> Vec<GraphRow> {
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    for commit in commits {
        let mut segments = Vec::new();
        let matching: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, hash)| hash.as_deref() == Some(commit.hash.as_str()))
            .map(|(i, _)| i)
            .collect();
        let lane = match matching.first() {
            Some(&lane) => lane,
            None => match lanes.iter().position(Option::is_none) {
                Some(free) => free,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            },
        };
        // Upper half: pass-through lines and lines converging on the dot.
        for (i, hash) in lanes.iter().enumerate() {
            if hash.is_none() {
                continue;
            }
            if matching.contains(&i) {
                segments.push(Segment {
                    from_lane: i,
                    from: Edge::Top,
                    to_lane: lane,
                    to: Edge::Middle,
                    color: i,
                });
            } else {
                segments.push(Segment {
                    from_lane: i,
                    from: Edge::Top,
                    to_lane: i,
                    to: Edge::Middle,
                    color: i,
                });
            }
        }
        for &i in &matching {
            lanes[i] = None;
        }
        // Lower half: the commit's lane continues to its first parent, other
        // parents join an existing lane or open a new one.
        let mut opened = Vec::new();
        for (index, parent) in commit.parents.iter().enumerate() {
            let target = if index == 0 {
                match lanes.iter().position(|h| h.as_deref() == Some(parent.as_str())) {
                    Some(existing) => existing,
                    None => {
                        lanes[lane] = Some(parent.clone());
                        lane
                    }
                }
            } else {
                match lanes.iter().position(|h| h.as_deref() == Some(parent.as_str())) {
                    Some(existing) => existing,
                    None => {
                        let slot = match lanes.iter().position(Option::is_none) {
                            Some(free) => free,
                            None => {
                                lanes.push(None);
                                lanes.len() - 1
                            }
                        };
                        lanes[slot] = Some(parent.clone());
                        slot
                    }
                }
            };
            opened.push(target);
            segments.push(Segment {
                from_lane: lane,
                from: Edge::Middle,
                to_lane: target,
                to: Edge::Bottom,
                color: if index == 0 { lane } else { target },
            });
        }
        for (i, hash) in lanes.iter().enumerate() {
            if hash.is_some() && !opened.contains(&i) {
                segments.push(Segment {
                    from_lane: i,
                    from: Edge::Middle,
                    to_lane: i,
                    to: Edge::Bottom,
                    color: i,
                });
            }
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        let width = segments
            .iter()
            .map(|s| s.from_lane.max(s.to_lane) + 1)
            .max()
            .unwrap_or(0)
            .max(lane + 1);
        rows.push(GraphRow {
            lane,
            width,
            segments,
        });
    }
    rows
}

/// A commit's metadata and its patch against the first parent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitDetail {
    pub commit: Commit,
    pub body: String,
    pub patch: String,
    pub truncated: bool,
}

const PATCH_ARGS: [&str; 5] = [
    "--no-ext-diff",
    "--no-textconv",
    "-M",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

/// Split `git show --no-patch` output (log fields plus `%b`) into commit and body.
pub fn parse_show_meta(raw: &str) -> Option<(Commit, String)> {
    let record = raw.split(RECORD).next()?;
    let fields: Vec<&str> = record.splitn(7, FIELD).collect();
    let body = fields.get(6).copied().unwrap_or_default().trim().to_owned();
    let head = fields[..fields.len().min(6)].join(&FIELD.to_string());
    let commit = parse_log(&format!("{head}{RECORD}")).into_iter().next()?;
    Some((commit, body))
}

/// `git show` for one commit. Merges are shown against their first parent.
pub fn show_commit(location: &Location, directory: &str, sha: &str) -> Result<CommitDetail, String> {
    if sha.is_empty() || sha.starts_with('-') {
        return Err("Invalid commit".into());
    }
    let meta = location.git(
        directory,
        &[
            "show",
            "--no-patch",
            "--format=%H%x1f%P%x1f%D%x1f%s%x1f%an%x1f%at%x1f%b%x1e",
            sha,
        ],
    )?;
    let (commit, body) = parse_show_meta(&meta).ok_or_else(|| format!("Commit {sha} not found"))?;
    let patch = match commit.parents.first() {
        Some(parent) => {
            let mut args = vec!["diff"];
            args.extend(PATCH_ARGS);
            args.push(parent);
            args.push(&commit.hash);
            location.git(directory, &args)?
        }
        None => {
            let mut args = vec!["show", "--format="];
            args.extend(PATCH_ARGS);
            args.push(&commit.hash);
            location.git(directory, &args)?
        }
    };
    let (patch, truncated) = clip_patch(patch);
    Ok(CommitDetail {
        commit,
        body,
        patch,
        truncated,
    })
}

/// Patch from the merge base of `base` and HEAD to `head`, or to the working
/// tree when `head` is `None` (what a pull request would show, plus local
/// edits). Untracked files are not included.
pub fn diff_range(
    location: &Location,
    directory: &str,
    base: &str,
    head: Option<&str>,
) -> Result<(String, bool), String> {
    if base.is_empty() || base.starts_with('-') || head.is_some_and(|h| h.starts_with('-')) {
        return Err("Invalid revision".into());
    }
    let merge_base = location
        .git(directory, &["merge-base", base, head.unwrap_or("HEAD")])?
        .trim()
        .to_owned();
    let mut args = vec!["diff"];
    args.extend(PATCH_ARGS);
    args.push(&merge_base);
    if let Some(head) = head {
        args.push(head);
    }
    Ok(clip_patch(location.git(directory, &args)?))
}

/// Local and remote-tracking branch names, local first, without `origin/HEAD`.
pub fn branches(location: &Location, directory: &str) -> Result<Vec<String>, String> {
    let raw = location.git(
        directory,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "--sort=-committerdate",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    Ok(parse_branches(&raw))
}

pub fn parse_branches(raw: &str) -> Vec<String> {
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for name in raw.lines().map(str::trim).filter(|n| !n.is_empty()) {
        if name.ends_with("/HEAD") {
            continue;
        }
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            local.push(branch.to_owned());
        } else if let Some(branch) = name.strip_prefix("refs/remotes/") {
            remote.push(branch.to_owned());
        }
    }
    local.extend(remote);
    local
}

/// The branch a feature branch is usually compared with.
pub fn pick_base_branch(branches: &[String], current: Option<&str>) -> Option<String> {
    ["main", "master", "origin/main", "origin/master", "develop", "origin/develop"]
        .iter()
        .find(|name| branches.iter().any(|b| b == *name) && Some(**name) != current)
        .map(|name| (*name).to_owned())
}

pub fn default_base_branch(location: &Location, directory: &str) -> Result<Option<String>, String> {
    let current = location
        .git(directory, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .map(|s| s.trim().to_owned());
    Ok(pick_base_branch(&branches(location, directory)?, current.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(hash: &str, parents: &[&str]) -> Commit {
        Commit {
            hash: hash.repeat(8),
            parents: parents.iter().map(|p| p.repeat(8)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn parses_log_records() {
        let raw = "aaaaaaaa1\u{1f}bbbbbbbb2 cccccccc3\u{1f}HEAD -> main, origin/main\u{1f}Merge x\u{1f}Viet\u{1f}1700000000\u{1e}\nbbbbbbbb2\u{1f}\u{1f}\u{1f}root\u{1f}A\u{1f}1\u{1e}\n";
        let log = parse_log(raw);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].parents.len(), 2);
        assert_eq!(log[0].refs, vec!["HEAD -> main", "origin/main"]);
        assert_eq!(log[0].subject, "Merge x");
        assert_eq!(log[0].time, 1_700_000_000);
        assert!(log[1].parents.is_empty());
        assert!(log[1].refs.is_empty());
        assert_eq!(log[0].short(), "aaaaaaaa");
    }

    #[test]
    fn linear_history_stays_in_one_lane() {
        let rows = layout_graph(&[commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])]);
        assert!(rows.iter().all(|r| r.lane == 0 && r.width == 1));
        assert_eq!(rows[2].segments.len(), 1, "root only has the incoming line");
    }

    #[test]
    fn merge_opens_and_closes_a_lane() {
        // m merges f into b; f branched from a.
        let rows = layout_graph(&[
            commit("m", &["b", "f"]),
            commit("f", &["a"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ]);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[0].width, 2);
        assert_eq!(rows[1].lane, 1);
        assert_eq!(rows[2].lane, 0);
        // b's parent a is already expected on lane 1, so b's line joins it.
        assert!(rows[2].segments.iter().any(|s| s.from_lane == 0 && s.to_lane == 1));
        assert_eq!(rows[3].lane, 1);
        assert_eq!(rows[3].width, 2);
    }

    #[test]
    fn branches_put_local_first_and_pick_main() {
        let list = parse_branches(
            "refs/heads/feat/x\nrefs/remotes/origin/main\nrefs/heads/main\nrefs/remotes/origin/HEAD\n",
        );
        assert_eq!(list, vec!["feat/x", "main", "origin/main"]);
        let (commit, body) = parse_show_meta(
            "abcdef12\u{1f}\u{1f}\u{1f}Subject\u{1f}A\u{1f}5\u{1f}Body line\n\u{1f}more\n\u{1e}\n",
        )
        .unwrap();
        assert_eq!(commit.subject, "Subject");
        assert_eq!(body, "Body line\n\u{1f}more");
        assert_eq!(pick_base_branch(&list, Some("feat/x")).as_deref(), Some("main"));
        assert_eq!(pick_base_branch(&list, Some("main")).as_deref(), Some("origin/main"));
        assert_eq!(pick_base_branch(&["x".into()], None), None);
    }

    #[test]
    fn rejects_option_like_revisions() {
        assert!(show_commit(&Location::Local, ".", "--output=x").is_err());
        assert!(diff_range(&Location::Local, ".", "-x", None).is_err());
    }
}
