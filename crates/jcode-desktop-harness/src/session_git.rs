//! Git for a session's own checkout, wherever that checkout lives.
//!
//! Local sessions run `git` directly. Sessions on a local WSL distribution
//! (`ssh://wsl/...`) run it through `wsl.exe --exec`, so no shell parses the
//! arguments and Linux paths are used as-is. Other SSH hosts are unsupported.

use std::process::Command;

/// Concurrent wsl.exe launches allowed process-wide, and retries after a
/// silent interop failure. Panels, sidebar probes and the bridge share these.
const WSL_CONCURRENCY: usize = 3;
const WSL_RETRIES: usize = 2;

mod wsl_gate {
    use std::sync::{Condvar, Mutex};

    static ACTIVE: Mutex<usize> = Mutex::new(0);
    static FREED: Condvar = Condvar::new();

    pub(super) struct Permit;

    pub(super) fn acquire() -> Permit {
        let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        while *active >= super::WSL_CONCURRENCY {
            active = FREED.wait(active).unwrap_or_else(|e| e.into_inner());
        }
        *active += 1;
        Permit
    }

    impl Drop for Permit {
        fn drop(&mut self) {
            let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
            *active -= 1;
            FREED.notify_one();
        }
    }

    #[cfg(test)]
    pub(super) fn active() -> usize {
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Where a session's checkout lives.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Location {
    Local,
    /// A WSL distribution, `None` for the default one.
    Wsl(Option<String>),
}

impl Location {
    /// Resolve a session ID. Returns `None` for SSH hosts Desktop cannot reach.
    pub fn of_session(session_id: &str) -> Option<Self> {
        match crate::harness::remote_host(session_id) {
            None => Some(Self::Local),
            Some(host) => jcode_sdk::wsl_distro(&host)
                .map(|distro| Self::Wsl(distro.map(str::to_owned))),
        }
    }

    /// The SSH-style host used to create sessions here, if remote.
    pub fn host(&self) -> Option<String> {
        match self {
            Self::Local => None,
            Self::Wsl(None) => Some("wsl".into()),
            Self::Wsl(Some(distro)) => Some(format!("{distro}.wsl")),
        }
    }

    pub fn command(&self, directory: &str, program: &str) -> Command {
        let mut command = match self {
            Self::Local => {
                let mut command = Command::new(program);
                command.current_dir(directory);
                command
            }
            Self::Wsl(distro) => {
                let mut command = Command::new("wsl.exe");
                if let Some(distro) = distro {
                    command.arg("--distribution").arg(distro);
                }
                command.arg("--cd").arg(directory).arg("--exec").arg(program);
                command
            }
        };
        crate::platform::no_console_window(&mut command);
        command
    }

    /// Run a command built by [`Location::command`]. WSL launches share a
    /// small global gate, because many concurrent wsl.exe interop sessions
    /// time out (`UtilAcceptVsock ... accept4 failed 110`) and exit non-zero
    /// with no output. Such silent failures are retried after a short pause.
    /// Real tool failures explain themselves on stderr and are never retried.
    pub fn output(&self, command: &mut Command) -> std::io::Result<std::process::Output> {
        if *self == Self::Local {
            return command.output();
        }
        let mut attempt = 0;
        loop {
            let output = {
                let _permit = wsl_gate::acquire();
                command.output()?
            };
            let silent = !output.status.success()
                && output.stdout.iter().all(u8::is_ascii_whitespace)
                && output.stderr.iter().all(u8::is_ascii_whitespace);
            if !silent || attempt == WSL_RETRIES {
                return Ok(output);
            }
            attempt += 1;
            std::thread::sleep(std::time::Duration::from_millis(300 * attempt as u64));
        }
    }

    /// Run git read-only with locks, pagers and colour disabled.
    pub fn git(&self, directory: &str, args: &[&str]) -> Result<String, String> {
        let mut command = self.command(directory, "git");
        command
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["--no-pager", "-c", "color.ui=false", "-c", "core.fsmonitor=false"])
            .args(args);
        let output = self
            .output(&mut command)
            .map_err(|error| format!("Could not run git: {error}"))?;
        if !output.status.success() {
            // wsl.exe reports its own failures on stdout in UTF-16, leaving
            // stderr empty. Silent failures (git grep without matches) keep
            // the bare `failed: ` form callers match on.
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if stderr.is_empty() {
                String::from_utf8_lossy(&output.stdout)
                    .chars()
                    .filter(|c| *c != '\0')
                    .take(400)
                    .collect::<String>()
                    .trim()
                    .to_owned()
            } else {
                stderr
            };
            return Err(format!(
                "git {} failed: {detail}",
                args.first().copied().unwrap_or_default(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn exists(&self, path: &str) -> bool {
        match self {
            Self::Local => std::fs::symlink_metadata(path).is_ok(),
            // A missing path is a silent non-zero exit, so gate without retry.
            Self::Wsl(_) => {
                let _permit = wsl_gate::acquire();
                self.command("/", "test")
                    .args(["-e", path])
                    .status()
                    .is_ok_and(|status| status.success())
            }
        }
    }
}

/// Largest patch kept from one git call, so a huge diff cannot stall the UI.
pub const PATCH_LIMIT: usize = 2 * 1024 * 1024;
/// Untracked files whose lines are counted for the summary.
const UNTRACKED_COUNT_LIMIT: usize = 300;
/// Untracked files whose content is shown, and lines shown per file.
const UNTRACKED_CONTENT_FILES: usize = 100;
pub const UNTRACKED_CONTENT_LINES: usize = 2000;
/// Largest total untracked content kept.
const UNTRACKED_CONTENT_LIMIT: usize = 1024 * 1024;

/// Which part of the working tree a change belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Area {
    Staged,
    Unstaged,
    Untracked,
}

impl Area {
    pub const ALL: [Area; 3] = [Area::Staged, Area::Unstaged, Area::Untracked];

    pub fn label(self) -> &'static str {
        match self {
            Area::Staged => "Staged",
            Area::Unstaged => "Unstaged",
            Area::Untracked => "Untracked",
        }
    }
}

/// One changed path in one area. A path staged and then edited again appears
/// once as Staged and once as Unstaged, like `git status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub area: Area,
    /// Repository-relative path, `/`-separated.
    pub path: String,
    pub previous_path: Option<String>,
    /// `M`, `A`, `D`, `R`, `C`, `T`, `U` (conflict) or `?` (untracked).
    pub status: char,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchInfo {
    /// `None` when detached.
    pub head: Option<String>,
    /// Abbreviated commit, `None` before the first commit.
    pub oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkingTree {
    pub branch: BranchInfo,
    pub files: Vec<ChangedFile>,
}

impl WorkingTree {
    pub fn totals(&self) -> (usize, usize) {
        self.files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.added, d + f.removed))
    }

    pub fn count(&self, area: Area) -> usize {
        self.files.iter().filter(|f| f.area == area).count()
    }

    pub fn is_clean(&self) -> bool {
        self.files.is_empty()
    }
}

/// Parse `git status --porcelain=v2 -z --branch`.
pub fn parse_status_v2(raw: &str) -> WorkingTree {
    let mut tree = WorkingTree::default();
    let mut records = raw.split('\0');
    while let Some(record) = records.next() {
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => {
                    tree.branch.oid = Some(value.chars().take(8).collect())
                }
                "branch.head" if value != "(detached)" => {
                    tree.branch.head = Some(value.to_owned())
                }
                "branch.upstream" => tree.branch.upstream = Some(value.to_owned()),
                "branch.ab" => {
                    for part in value.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+') {
                            tree.branch.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            tree.branch.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let Some((kind, rest)) = record.split_once(' ') else {
            continue;
        };
        match kind {
            "?" => tree.files.push(changed(Area::Untracked, rest, None, '?')),
            "1" | "2" | "u" => {
                // Fixed fields precede the path, which may contain spaces.
                let fields = match kind {
                    "1" => 7,
                    "2" => 8,
                    _ => 9,
                };
                let mut parts = rest.splitn(fields + 1, ' ');
                let xy = parts.next().unwrap_or("..");
                let Some(path) = parts.nth(fields - 1) else {
                    continue;
                };
                let previous = (kind == "2").then(|| records.next()).flatten();
                let mut letters = xy.chars();
                let (x, y) = (letters.next().unwrap_or('.'), letters.next().unwrap_or('.'));
                if kind == "u" {
                    tree.files.push(changed(Area::Unstaged, path, None, 'U'));
                    continue;
                }
                for (area, letter) in [(Area::Staged, x), (Area::Unstaged, y)] {
                    if letter != '.' {
                        let previous = previous.filter(|_| matches!(letter, 'R' | 'C'));
                        tree.files.push(changed(area, path, previous, letter));
                    }
                }
            }
            _ => {}
        }
    }
    tree
}

fn changed(area: Area, path: &str, previous: Option<&str>, status: char) -> ChangedFile {
    ChangedFile {
        area,
        path: path.to_owned(),
        previous_path: previous.map(str::to_owned),
        status,
        added: 0,
        removed: 0,
        binary: false,
    }
}

/// One `git diff --numstat -z` entry. Counts are `None` for binary files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumstatEntry {
    pub path: String,
    pub previous_path: Option<String>,
    pub added: Option<usize>,
    pub removed: Option<usize>,
}

/// Parse `git diff --numstat -z`. Renames carry an empty path field followed
/// by the old and new paths as separate NUL-terminated tokens.
pub fn parse_numstat(raw: &str) -> Vec<NumstatEntry> {
    let mut entries = Vec::new();
    let mut tokens = raw.split('\0');
    while let Some(token) = tokens.next() {
        let token = token.trim_start_matches('\n');
        let mut fields = token.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (path, previous_path) = if path.is_empty() {
            let (Some(old), Some(new)) = (tokens.next(), tokens.next()) else {
                break;
            };
            (new.to_owned(), Some(old.to_owned()))
        } else {
            (path.to_owned(), None)
        };
        entries.push(NumstatEntry {
            path,
            previous_path,
            added: added.parse().ok(),
            removed: removed.parse().ok(),
        });
    }
    entries
}

/// Parse `git grep -c -z` output: `path\0count\n` per file with a match.
fn parse_grep_counts(raw: &str) -> Vec<(String, usize)> {
    raw.lines()
        .filter_map(|line| {
            let (path, count) = line.rsplit_once('\0')?;
            Some((path.to_owned(), count.trim().parse().ok()?))
        })
        .collect()
}

fn literal_pathspecs<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
    paths.map(|path| format!(":(top,literal){path}")).collect()
}

/// Search untracked files with `git grep`, which skips binary files with `-I`,
/// works the same through WSL and needs no shell. An empty pattern matches
/// every line, so `-c` counts lines and `-n` lists them.
fn grep_untracked(
    location: &Location,
    directory: &str,
    mode: &[&str],
    paths: &[&str],
    text_only: bool,
) -> Result<String, String> {
    if paths.is_empty() {
        return Ok(String::new());
    }
    let specs = literal_pathspecs(paths.iter().copied());
    let mut args = vec!["grep", "--untracked", "--no-color", "-z", "--full-name"];
    if text_only {
        args.push("-I");
    }
    args.extend_from_slice(mode);
    args.extend(["-e", "", "--"]);
    args.extend(specs.iter().map(String::as_str));
    match location.git(directory, &args) {
        Ok(output) => Ok(output),
        // `git grep` exits 1 when nothing matched, e.g. only empty files.
        Err(error) if error.ends_with("failed: ") => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn numstat(location: &Location, directory: &str, staged: bool) -> Result<Vec<NumstatEntry>, String> {
    let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "-M", "--numstat", "-z"];
    if staged {
        args.push("--cached");
    }
    location.git(directory, &args).map(|raw| parse_numstat(&raw))
}

fn index_of(tree: &WorkingTree, area: Area) -> std::collections::HashMap<String, usize> {
    tree.files
        .iter()
        .enumerate()
        .filter(|(_, file)| file.area == area)
        .map(|(index, file)| (file.path.clone(), index))
        .collect()
}

fn apply_numstat(tree: &mut WorkingTree, area: Area, entries: &[NumstatEntry]) {
    let index = index_of(tree, area);
    for entry in entries {
        if let Some(&at) = index.get(&entry.path) {
            let file = &mut tree.files[at];
            file.added = entry.added.unwrap_or(0);
            file.removed = entry.removed.unwrap_or(0);
            file.binary = entry.added.is_none();
        }
    }
}

/// Count lines of untracked files. Files `-I` skips are binary.
fn apply_untracked_counts(location: &Location, directory: &str, tree: &mut WorkingTree) -> Result<(), String> {
    let paths: Vec<&str> = tree
        .files
        .iter()
        .filter(|file| file.area == Area::Untracked)
        .take(UNTRACKED_COUNT_LIMIT)
        .map(|file| file.path.as_str())
        .collect();
    if paths.is_empty() {
        return Ok(());
    }
    let (text, any) = std::thread::scope(|scope| {
        let any = scope.spawn(|| grep_untracked(location, directory, &["-c"], &paths, false));
        let text = grep_untracked(location, directory, &["-c"], &paths, true);
        (text, any.join().unwrap_or_else(|_| Err("git grep panicked".into())))
    });
    let text: std::collections::HashMap<String, usize> =
        parse_grep_counts(&text?).into_iter().collect();
    let any = parse_grep_counts(&any?);
    let index = index_of(tree, Area::Untracked);
    for (path, _) in any {
        if let Some(&at) = index.get(&path) {
            let file = &mut tree.files[at];
            match text.get(&path) {
                Some(&lines) => file.added = lines,
                None => file.binary = true,
            }
        }
    }
    Ok(())
}

/// Status of a session's checkout with per-file line counts. Untracked
/// files count as added lines. Read-only: no index refresh or locks.
pub fn working_tree(location: &Location, directory: &str) -> Result<WorkingTree, String> {
    // Each wsl.exe launch has fixed latency, so independent reads overlap.
    let (status, unstaged, staged) = std::thread::scope(|scope| {
        let unstaged = scope.spawn(|| numstat(location, directory, false));
        let staged = scope.spawn(|| numstat(location, directory, true));
        let status = location.git(
            directory,
            &["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"],
        );
        (
            status,
            unstaged.join().unwrap_or_else(|_| Err("git diff panicked".into())),
            staged.join().unwrap_or_else(|_| Err("git diff panicked".into())),
        )
    });
    let mut tree = parse_status_v2(&status?);
    // A repository without commits has no HEAD to diff the index against.
    apply_numstat(&mut tree, Area::Staged, &staged.unwrap_or_default());
    apply_numstat(&mut tree, Area::Unstaged, &unstaged?);
    apply_untracked_counts(location, directory, &mut tree)?;
    Ok(tree)
}

/// Unified patches for every area of a working tree, each bounded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkingTreePatches {
    pub staged: String,
    pub unstaged: String,
    /// Untracked text files rendered as new-file patches.
    pub untracked: String,
    /// Some content was omitted to respect the size limits.
    pub truncated: bool,
}

/// The tracked diff as a unified patch with `a/` and `b/` prefixes regardless
/// of user configuration, cut at a file boundary past [`PATCH_LIMIT`].
pub fn diff_patch(location: &Location, directory: &str, staged: bool) -> Result<(String, bool), String> {
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "-M",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    if staged {
        args.push("--cached");
    }
    Ok(clip_patch(location.git(directory, &args)?))
}

/// Cut a patch to [`PATCH_LIMIT`] at the last whole file.
pub fn clip_patch(patch: String) -> (String, bool) {
    if patch.len() <= PATCH_LIMIT {
        return (patch, false);
    }
    let mut end = PATCH_LIMIT;
    while !patch.is_char_boundary(end) {
        end -= 1;
    }
    let cut = patch[..end].rfind("\ndiff --git ").map_or(end, |index| index + 1);
    (patch[..cut].to_owned(), true)
}

/// Build new-file patches from `git grep -n -z` output (`path\0line\0text`).
fn untracked_patch(raw: &str, counts: &std::collections::HashMap<&str, usize>) -> (String, bool) {
    let mut grouped: Vec<(String, Vec<&str>)> = Vec::new();
    for line in raw.split('\n') {
        let mut parts = line.splitn(3, '\0');
        let (Some(path), Some(_), Some(text)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        match grouped.last_mut() {
            Some((last, lines)) if last == path => lines.push(text),
            _ => grouped.push((path.to_owned(), vec![text])),
        }
    }
    let mut patch = String::new();
    let mut truncated = false;
    for (path, lines) in grouped {
        if patch.len() > UNTRACKED_CONTENT_LIMIT {
            truncated = true;
            break;
        }
        let total = counts.get(path.as_str()).copied().unwrap_or(lines.len());
        let lines = &lines[..lines.len().min(UNTRACKED_CONTENT_LINES)];
        truncated |= lines.len() < total;
        patch.push_str(&format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
            lines.len()
        ));
        for text in lines {
            patch.push('+');
            patch.push_str(text.strip_suffix('\r').unwrap_or(text));
            patch.push('\n');
        }
    }
    (patch, truncated)
}

/// Load the patches behind a [`WorkingTree`] for review.
pub fn working_tree_patches(
    location: &Location,
    directory: &str,
    tree: &WorkingTree,
) -> Result<WorkingTreePatches, String> {
    let untracked: Vec<&ChangedFile> = tree
        .files
        .iter()
        .filter(|file| file.area == Area::Untracked && !file.binary)
        .collect();
    let shown: Vec<&str> = untracked
        .iter()
        .take(UNTRACKED_CONTENT_FILES)
        .map(|file| file.path.as_str())
        .collect();
    let counts = untracked
        .iter()
        .map(|file| (file.path.as_str(), file.added))
        .collect();
    let has = |area| tree.files.iter().any(|file: &ChangedFile| file.area == area);
    let (staged, unstaged, raw_untracked) = std::thread::scope(|scope| {
        let staged = scope.spawn(|| {
            if has(Area::Staged) {
                diff_patch(location, directory, true)
            } else {
                Ok(Default::default())
            }
        });
        let unstaged = scope.spawn(|| {
            if has(Area::Unstaged) {
                diff_patch(location, directory, false)
            } else {
                Ok(Default::default())
            }
        });
        // `-m` (git 2.38+) bounds huge files. Older git rejects it, so retry.
        let max = UNTRACKED_CONTENT_LINES.to_string();
        let untracked = grep_untracked(location, directory, &["-n", "-m", &max], &shown, true)
            .or_else(|_| grep_untracked(location, directory, &["-n"], &shown, true));
        let join = |handle: std::thread::ScopedJoinHandle<'_, Result<(String, bool), String>>| {
            handle.join().unwrap_or_else(|_| Err("git diff panicked".into()))
        };
        (join(staged), join(unstaged), untracked)
    });
    let omitted_files = shown.len() < untracked.len();
    let (staged, staged_cut) = staged?;
    let (unstaged, unstaged_cut) = unstaged?;
    let (untracked, untracked_cut) = untracked_patch(&raw_untracked?, &counts);
    Ok(WorkingTreePatches {
        staged,
        unstaged,
        untracked,
        truncated: staged_cut || unstaged_cut || untracked_cut || omitted_files,
    })
}

#[cfg(test)]
mod working_tree_tests {
    use super::*;

    #[test]
    fn porcelain_v2_parses_branch_areas_renames_and_spaces() {
        let raw = [
            "# branch.oid b1561e434d8f725caa1bbdb67b1ffdb598cb6de8",
            "# branch.head feature/x",
            "# branch.upstream origin/feature/x",
            "# branch.ab +3 -1",
            "1 MM N... 100644 100644 100644 aaaa bbbb src/main.rs",
            "1 A. N... 000000 100644 100644 0000 cccc new dir/file name.txt",
            "1 .D N... 100644 100644 000000 dddd dddd gone.rs",
            "2 R. N... 100644 100644 100644 eeee eeee R100 renamed.rs",
            "old name.rs",
            "u UU N... 100644 100644 100644 100644 a b c conflict.rs",
            "? sub/new file.txt",
            "? we*rd.txt",
            "",
        ]
        .join("\0");
        let tree = parse_status_v2(&raw);
        assert_eq!(tree.branch.head.as_deref(), Some("feature/x"));
        assert_eq!(tree.branch.oid.as_deref(), Some("b1561e43"));
        assert_eq!(tree.branch.upstream.as_deref(), Some("origin/feature/x"));
        assert_eq!((tree.branch.ahead, tree.branch.behind), (3, 1));
        let summary: Vec<_> = tree
            .files
            .iter()
            .map(|f| (f.area, f.status, f.path.as_str(), f.previous_path.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (Area::Staged, 'M', "src/main.rs", None),
                (Area::Unstaged, 'M', "src/main.rs", None),
                (Area::Staged, 'A', "new dir/file name.txt", None),
                (Area::Unstaged, 'D', "gone.rs", None),
                (Area::Staged, 'R', "renamed.rs", Some("old name.rs")),
                (Area::Unstaged, 'U', "conflict.rs", None),
                (Area::Untracked, '?', "sub/new file.txt", None),
                (Area::Untracked, '?', "we*rd.txt", None),
            ]
        );
        assert_eq!(tree.count(Area::Staged), 3);
        assert_eq!(tree.count(Area::Unstaged), 3);
        assert_eq!(tree.count(Area::Untracked), 2);
    }

    #[test]
    fn porcelain_v2_handles_initial_and_detached_heads() {
        let tree = parse_status_v2("# branch.oid (initial)\0# branch.head (detached)\0");
        assert_eq!(tree.branch, BranchInfo::default());
        assert!(tree.is_clean());
        assert_eq!(parse_status_v2(""), WorkingTree::default());
    }

    #[test]
    fn numstat_parses_counts_binaries_and_renames() {
        let raw = "1\t0\tt.txt\0-\t-\timg.png\0" .to_owned()
            + "3\t2\t\0old name.rs\0new name.rs\0";
        assert_eq!(
            parse_numstat(&raw),
            vec![
                NumstatEntry { path: "t.txt".into(), previous_path: None, added: Some(1), removed: Some(0) },
                NumstatEntry { path: "img.png".into(), previous_path: None, added: None, removed: None },
                NumstatEntry {
                    path: "new name.rs".into(),
                    previous_path: Some("old name.rs".into()),
                    added: Some(3),
                    removed: Some(2),
                },
            ]
        );
        assert!(parse_numstat("").is_empty());
    }

    #[test]
    fn numstat_fills_counts_per_area() {
        let mut tree = parse_status_v2(
            "1 MM N... 100644 100644 100644 a b x.rs\0? bin.dat\0",
        );
        apply_numstat(&mut tree, Area::Staged, &parse_numstat("4\t1\tx.rs\0"));
        apply_numstat(&mut tree, Area::Unstaged, &parse_numstat("2\t0\tx.rs\0"));
        assert_eq!((tree.files[0].added, tree.files[0].removed), (4, 1));
        assert_eq!((tree.files[1].added, tree.files[1].removed), (2, 0));
        assert_eq!(tree.totals(), (6, 1));
    }

    #[test]
    fn grep_output_becomes_new_file_patches() {
        assert_eq!(
            parse_grep_counts("sub/new file.txt\x003\nwe*rd.txt\x001\n"),
            vec![("sub/new file.txt".into(), 3), ("we*rd.txt".into(), 1)]
        );
        let raw = "e.txt\x001\x00a\ne.txt\x002\x00\ne.txt\x003\x00b:c\r\nx.txt\x001\x00only\n";
        let counts = [("e.txt", 3), ("x.txt", 5)].into_iter().collect();
        let (patch, truncated) = untracked_patch(raw, &counts);
        assert!(truncated, "x.txt shows 1 of 5 lines");
        assert_eq!(
            patch,
            "diff --git a/e.txt b/e.txt\nnew file mode 100644\n--- /dev/null\n+++ b/e.txt\n@@ -0,0 +1,3 @@\n+a\n+\n+b:c\n\
             diff --git a/x.txt b/x.txt\nnew file mode 100644\n--- /dev/null\n+++ b/x.txt\n@@ -0,0 +1,1 @@\n+only\n"
        );
        let preview = jcode_desktop_model::diff_model::from_patch(&patch).unwrap();
        assert_eq!(preview.files.len(), 2);
        assert_eq!(preview.files[0].kind, "Add");
        assert_eq!(preview.files[0].counts(), (3, 0));
    }

    #[test]
    fn clip_patch_cuts_at_a_file_boundary() {
        let file = |name: &str| format!("diff --git a/{name} b/{name}\n+{}\n", "x".repeat(PATCH_LIMIT / 2));
        let (small, cut) = clip_patch(file("a"));
        assert!(!cut && small.starts_with("diff --git a/a"));
        let (clipped, cut) = clip_patch(file("a") + &file("b") + &file("c"));
        assert!(cut);
        assert!(clipped.len() <= PATCH_LIMIT);
        assert!(clipped.ends_with('\n') && !clipped.contains("a/c"));
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Real local repository: staged, unstaged, untracked text, binary, empty.
    #[test]
    fn working_tree_reads_every_area_without_modifying_the_repo() {
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path();
        git(dir, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("b.txt"), "keep\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "--quiet", "-m", "init"]);
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        git(dir, &["add", "a.txt"]);
        std::fs::write(dir.join("b.txt"), "").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/new file.txt"), "x\ny\n").unwrap();
        std::fs::write(dir.join("bin.dat"), b"a\0b").unwrap();
        std::fs::write(dir.join("empty.txt"), "").unwrap();
        let path = dir.to_str().unwrap();
        let tree = working_tree(&Location::Local, path).unwrap();
        assert_eq!(tree.branch.head.as_deref(), Some("main"));
        let find = |area, name: &str| {
            tree.files
                .iter()
                .find(|f| f.area == area && f.path == name)
                .unwrap_or_else(|| panic!("{area:?} {name} in {:?}", tree.files))
        };
        let a = find(Area::Staged, "a.txt");
        assert_eq!((a.status, a.added, a.removed), ('M', 2, 1));
        let b = find(Area::Unstaged, "b.txt");
        assert_eq!((b.status, b.added, b.removed), ('M', 0, 1));
        let new = find(Area::Untracked, "sub/new file.txt");
        assert_eq!((new.status, new.added, new.binary), ('?', 2, false));
        assert!(find(Area::Untracked, "bin.dat").binary);
        assert_eq!(find(Area::Untracked, "empty.txt").added, 0);
        let patches = working_tree_patches(&Location::Local, path, &tree).unwrap();
        assert!(patches.staged.contains("+++ b/a.txt"), "{}", patches.staged);
        assert!(patches.unstaged.contains("--- a/b.txt"), "{}", patches.unstaged);
        assert!(patches.untracked.contains("+++ b/sub/new file.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n"));
        assert!(!patches.untracked.contains("bin.dat"));
        assert!(!patches.truncated);
        // Read-only: the index and files are unchanged.
        let after = working_tree(&Location::Local, path).unwrap();
        assert_eq!(after, tree);
    }

    /// Windows only, opt-in: a burst like several panels' Changes pills plus
    /// sidebar probes at once. Every launch succeeds and the gate holds.
    #[test]
    fn wsl_parallel_git_calls_are_bounded_and_all_succeed() {
        let Ok(repo) = std::env::var("JCODE_WSL_TEST_REPO") else {
            return;
        };
        let wsl = Location::Wsl(None);
        let peak = std::sync::atomic::AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..12)
                .map(|_| {
                    scope.spawn(|| {
                        let result = wsl.git(&repo, &["diff", "--numstat"]);
                        peak.fetch_max(wsl_gate::active(), std::sync::atomic::Ordering::Relaxed);
                        result
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        eprintln!("12 parallel WSL git calls in {:?}", started.elapsed());
        for result in &results {
            assert!(result.as_ref().is_ok_and(|out| !out.is_empty()), "{result:?}");
        }
        assert!(peak.into_inner() <= WSL_CONCURRENCY);
    }

    /// Windows only, opt-in: `JCODE_WSL_TEST_REPO=/home/me/repo`.
    #[test]
    fn wsl_working_tree_summarizes_the_pending_change() {
        let Ok(repo) = std::env::var("JCODE_WSL_TEST_REPO") else {
            return;
        };
        let wsl = Location::Wsl(None);
        let started = std::time::Instant::now();
        let tree = working_tree(&wsl, &repo).unwrap();
        eprintln!("wsl working_tree in {:?}: {:?}", started.elapsed(), tree);
        assert!(!tree.is_clean(), "repo needs a pending change");
        assert!(tree.branch.head.is_some());
        let (added, removed) = tree.totals();
        assert!(added + removed > 0);
        let patches = working_tree_patches(&wsl, &repo, &tree).unwrap();
        assert!(!(patches.staged.is_empty() && patches.unstaged.is_empty() && patches.untracked.is_empty()));
    }
}

/// Encode rather than replace separators, so feature/a and feature-a differ.
/// Matches the SDK's local worktree layout.
fn safe_name(branch: &str) -> String {
    let mut name = String::new();
    for byte in branch.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            name.push(char::from(byte));
        } else {
            name.push_str(&format!("%{byte:02X}"));
        }
    }
    name
}

/// Split a POSIX or Windows path into parent and final component.
fn split_path(path: &str) -> Option<(&str, &str)> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let index = trimmed.rfind(['/', '\\'])?;
    let (parent, name) = (&trimmed[..index], &trimmed[index + 1..]);
    (!name.is_empty()).then_some((if parent.is_empty() { "/" } else { parent }, name))
}

/// Create a new branch from HEAD in its own checkout beside the repository,
/// at `<repo>-worktrees/<encoded-branch>`, and return its path. The original
/// checkout, including uncommitted work, is never modified. Nothing is forced,
/// reset or deleted, even on failure.
pub fn create_worktree(location: &Location, directory: &str, branch: &str) -> Result<String, String> {
    let branch = branch.trim();
    if branch.is_empty() || branch.starts_with('-') || branch.chars().any(char::is_whitespace) {
        return Err("Use a single branch name, e.g. `/worktree feature/search`.".into());
    }
    if *location == Location::Local {
        return jcode_sdk::worktrees::create(directory, branch)
            .map(|entry| entry.path)
            .map_err(|error| format!("{error:#}"));
    }
    let checked = location.git(directory, &["check-ref-format", "--branch", branch])?;
    if checked.trim_end() != branch {
        return Err("Branch name must be literal, not Git shorthand.".into());
    }
    let list = location.git(directory, &["worktree", "list", "--porcelain"])?;
    let main = list
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .ok_or("Not inside a Git repository.")?;
    let (parent, name) = split_path(main).ok_or("Repository has no parent directory.")?;
    let root = format!("{}/{name}-worktrees", parent.trim_end_matches('/'));
    let path = format!("{root}/{}", safe_name(branch));
    if location.exists(&path) {
        return Err(format!("Worktree destination already exists: {path}"));
    }
    // `-b`, not `-B`: Git refuses an existing branch atomically.
    location.git(directory, &["worktree", "add", "-b", branch, "--", &path, "HEAD"])?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_paths_match_the_sdk_layout() {
        assert_eq!(safe_name("feature/search"), "feature%2Fsearch");
        assert_eq!(split_path("/home/me/repo"), Some(("/home/me", "repo")));
        assert_eq!(split_path("/repo/"), Some(("/", "repo")));
        assert_eq!(split_path("repo"), None);
    }

    #[test]
    fn local_sessions_resolve_locally() {
        assert_eq!(Location::of_session("session_abc"), Some(Location::Local));
        assert_eq!(Location::Local.host(), None);
        assert_eq!(Location::Wsl(Some("Ubuntu".into())).host().as_deref(), Some("Ubuntu.wsl"));
    }

    /// Windows only, opt-in: `JCODE_WSL_TEST_REPO=/home/me/repo cargo test`.
    /// The repo must have an uncommitted change. Creates one new worktree.
    #[test]
    fn wsl_diff_and_worktree_leave_the_original_checkout_alone() {
        let Ok(repo) = std::env::var("JCODE_WSL_TEST_REPO") else {
            return;
        };
        let wsl = Location::of_session("ssh://wsl/session_x").expect("wsl host");
        assert_eq!(wsl, Location::Wsl(None));
        let status_before = wsl.git(&repo, &["status", "--porcelain"]).unwrap();
        assert!(!status_before.trim().is_empty(), "repo needs a pending change");
        let diff = wsl
            .git(&repo, &["diff", "--src-prefix=a/", "--dst-prefix=b/"])
            .unwrap();
        assert!(diff.contains("--- a/"), "{diff}");
        let branch = format!("wt-test-{}", std::process::id());
        let path = create_worktree(&wsl, &repo, &branch).unwrap();
        assert!(path.ends_with(&format!("-worktrees/{branch}")), "{path}");
        assert!(wsl.exists(&path));
        assert_eq!(wsl.git(&path, &["status", "--porcelain"]).unwrap().trim(), "");
        assert_eq!(wsl.git(&path, &["branch", "--show-current"]).unwrap().trim(), branch);
        assert_eq!(wsl.git(&repo, &["status", "--porcelain"]).unwrap(), status_before);
        let again = create_worktree(&wsl, &repo, &branch).unwrap_err();
        assert!(again.contains("already exists"), "{again}");
        eprintln!("worktree created at {path}");
    }
}
