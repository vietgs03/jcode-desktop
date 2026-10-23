//! Git for a session's own checkout, wherever that checkout lives.
//!
//! Local sessions run `git` directly. Sessions on a local WSL distribution
//! (`ssh://wsl/...`) run it through `wsl.exe --exec`, so no shell parses the
//! arguments and Linux paths are used as-is. Other SSH hosts are unsupported.

use std::process::Command;

/// Where a session's checkout lives.
#[derive(Clone, Debug, PartialEq, Eq)]
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

    /// Run git read-only with locks, pagers and colour disabled.
    pub fn git(&self, directory: &str, args: &[&str]) -> Result<String, String> {
        let mut command = self.command(directory, "git");
        command
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["--no-pager", "-c", "color.ui=false", "-c", "core.fsmonitor=false"])
            .args(args);
        let output = command
            .output()
            .map_err(|error| format!("Could not run git: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn exists(&self, path: &str) -> bool {
        match self {
            Self::Local => std::fs::symlink_metadata(path).is_ok(),
            Self::Wsl(_) => self
                .command("/", "test")
                .args(["-e", path])
                .status()
                .is_ok_and(|status| status.success()),
        }
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
