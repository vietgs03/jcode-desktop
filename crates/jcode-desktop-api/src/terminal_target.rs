//! Where a host terminal starts, encoded in the ABI's `working_dir` string.
//!
//! `terminal_create` only carries one optional directory. A plain path means a
//! directory on the host machine. A WSL checkout is written as
//! `wsl://<distribution>/<absolute linux path>`, with an empty distribution
//! for the default one (`wsl:///home/me/repo`) and an empty path for the Linux
//! home directory (`wsl://Ubuntu`). The host parses and validates it, and runs
//! `wsl.exe` with separate arguments, so no shell ever sees the path.

pub const WSL_SCHEME: &str = "wsl://";

/// A parsed terminal destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalTarget {
    /// A directory on the host machine, or its default when `None`.
    Local(Option<String>),
    /// A WSL distribution (`None` for the default) and a Linux directory
    /// (`None` for the user's home).
    Wsl {
        distribution: Option<String>,
        directory: Option<String>,
    },
}

/// Distribution names are registry keys: letters, digits, `.`, `-` and `_`.
fn valid_distribution(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['-', '.'])
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn valid_linux_directory(path: &str) -> bool {
    path.starts_with('/') && !path.chars().any(char::is_control)
}

/// Encode a WSL destination. Returns `None` for an invalid distribution or a
/// directory that is not an absolute Linux path.
pub fn encode_wsl(distribution: Option<&str>, directory: Option<&str>) -> Option<String> {
    let distribution = distribution.unwrap_or_default();
    if !distribution.is_empty() && !valid_distribution(distribution) {
        return None;
    }
    let directory = directory.map(str::trim).filter(|dir| !dir.is_empty());
    if directory.is_some_and(|dir| !valid_linux_directory(dir)) {
        return None;
    }
    Some(format!(
        "{WSL_SCHEME}{distribution}{}",
        directory.unwrap_or_default()
    ))
}

impl TerminalTarget {
    /// Parse the ABI string. Malformed `wsl://` strings are rejected rather
    /// than treated as a local directory named `wsl:`.
    pub fn parse(working_dir: Option<&str>) -> Result<Self, String> {
        let Some(value) = working_dir.filter(|dir| !dir.is_empty()) else {
            return Ok(Self::Local(None));
        };
        let Some(rest) = value.strip_prefix(WSL_SCHEME) else {
            return Ok(Self::Local(Some(value.to_owned())));
        };
        let (distribution, directory) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        if !distribution.is_empty() && !valid_distribution(distribution) {
            return Err(format!("Invalid WSL distribution in terminal target: {distribution:?}"));
        }
        // `wsl://Ubuntu/` names the root, not home.
        if !directory.is_empty() && !valid_linux_directory(directory) {
            return Err("Invalid WSL directory in terminal target".into());
        }
        Ok(Self::Wsl {
            distribution: (!distribution.is_empty()).then(|| distribution.to_owned()),
            directory: (!directory.is_empty()).then(|| directory.to_owned()),
        })
    }
}

/// A process to run in the terminal PTY: program, arguments and directory.
/// Hosts translate it into their PTY library's command type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalLaunch {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
}

/// The Windows terminal for a target. Windows has no `sh`, so local targets
/// use PowerShell in the requested directory. A WSL target opens the
/// distribution's login shell in that Linux directory. `default_wsl` is the
/// configured default machine (`wsl` or `<Distro>.wsl`), used only when the
/// UI did not ask for any directory, which keeps older UIs opening WSL.
pub fn windows_launch(target: &TerminalTarget, default_wsl: Option<&str>) -> TerminalLaunch {
    let wsl = |distribution: Option<&str>, directory: Option<&str>| {
        let mut args = Vec::new();
        if let Some(distribution) = distribution {
            args.push("--distribution".to_owned());
            args.push(distribution.to_owned());
        }
        args.push("--cd".to_owned());
        args.push(directory.unwrap_or("~").to_owned());
        TerminalLaunch {
            program: "wsl.exe".into(),
            args,
            cwd: None,
        }
    };
    let default = default_wsl.and_then(wsl_host);
    match (target, default) {
        (
            TerminalTarget::Wsl {
                distribution,
                directory,
            },
            _,
        ) => wsl(distribution.as_deref(), directory.as_deref()),
        (TerminalTarget::Local(None), Some(distribution)) => wsl(distribution, None),
        (TerminalTarget::Local(directory), _) => TerminalLaunch {
            program: "powershell.exe".into(),
            args: vec!["-NoLogo".into()],
            cwd: directory.clone(),
        },
    }
}

/// `wsl` is the default distribution, `<Distro>.wsl` a named one.
pub fn wsl_host(host: &str) -> Option<Option<&str>> {
    if host.eq_ignore_ascii_case("wsl") {
        return Some(None);
    }
    let stem = host.len().checked_sub(4)?;
    (host.is_char_boundary(stem)
        && host[stem..].eq_ignore_ascii_case(".wsl")
        && valid_distribution(&host[..stem]))
    .then(|| Some(&host[..stem]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_targets_round_trip_and_reject_malformed_input() {
        let repo = encode_wsl(None, Some("/home/me/my repo")).unwrap();
        assert_eq!(repo, "wsl:///home/me/my repo");
        assert_eq!(
            TerminalTarget::parse(Some(&repo)),
            Ok(TerminalTarget::Wsl {
                distribution: None,
                directory: Some("/home/me/my repo".into())
            })
        );
        let home = encode_wsl(Some("Ubuntu-22.04"), None).unwrap();
        assert_eq!(
            TerminalTarget::parse(Some(&home)),
            Ok(TerminalTarget::Wsl {
                distribution: Some("Ubuntu-22.04".into()),
                directory: None
            })
        );
        assert_eq!(encode_wsl(Some("bad name"), None), None);
        assert_eq!(encode_wsl(Some("-x"), None), None);
        assert_eq!(encode_wsl(None, Some("relative")), None);
        assert_eq!(encode_wsl(None, Some("C:\\Users")), None);
        assert!(TerminalTarget::parse(Some("wsl://a;b/home")).is_err());
        assert!(TerminalTarget::parse(Some("wsl:///home/\nx")).is_err());
        assert_eq!(
            TerminalTarget::parse(Some("C:\\Users\\me")),
            Ok(TerminalTarget::Local(Some("C:\\Users\\me".into())))
        );
        assert_eq!(TerminalTarget::parse(None), Ok(TerminalTarget::Local(None)));
        assert_eq!(TerminalTarget::parse(Some("")), Ok(TerminalTarget::Local(None)));
    }

    #[test]
    fn windows_launch_never_builds_a_shell_string() {
        let target = TerminalTarget::parse(Some("wsl://Ubuntu/home/me/$(rm -rf ~)")).unwrap();
        assert_eq!(
            windows_launch(&target, None),
            TerminalLaunch {
                program: "wsl.exe".into(),
                args: vec![
                    "--distribution".into(),
                    "Ubuntu".into(),
                    "--cd".into(),
                    "/home/me/$(rm -rf ~)".into()
                ],
                cwd: None,
            }
        );
        let local = TerminalTarget::Local(Some("C:\\work".into()));
        let launch = windows_launch(&local, Some("wsl"));
        assert_eq!(launch.program, "powershell.exe");
        assert_eq!(launch.cwd.as_deref(), Some("C:\\work"));
        // Older UIs send no directory: the configured WSL default still wins.
        let fallback = windows_launch(&TerminalTarget::Local(None), Some("Debian.wsl"));
        assert_eq!(fallback.args, ["--distribution", "Debian", "--cd", "~"]);
        assert_eq!(
            windows_launch(&TerminalTarget::Local(None), Some("server")).program,
            "powershell.exe"
        );
    }

    #[test]
    fn wsl_hosts_match_the_sdk_names() {
        assert_eq!(wsl_host("WSL"), Some(None));
        assert_eq!(wsl_host("Ubuntu.wsl"), Some(Some("Ubuntu")));
        assert_eq!(wsl_host(".wsl"), None);
        assert_eq!(wsl_host("user@host.wsl"), None);
        assert_eq!(wsl_host("server"), None);
    }

    /// Windows with WSL, opt-in: `JCODE_WSL_TEST_REPO=/home/me/repo`. Runs the
    /// exact program and arguments the host would, with `--exec pwd` appended
    /// so the shell reports where it started and exits.
    #[cfg(windows)]
    #[test]
    fn wsl_launch_starts_in_the_session_directory() {
        let Ok(repo) = std::env::var("JCODE_WSL_TEST_REPO") else {
            return;
        };
        let target = TerminalTarget::parse(Some(&encode_wsl(None, Some(&repo)).unwrap())).unwrap();
        let launch = windows_launch(&target, None);
        let output = std::process::Command::new(&launch.program)
            .args(&launch.args)
            .args(["--exec", "pwd"])
            .output()
            .expect("run wsl.exe");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), repo);
    }
}
