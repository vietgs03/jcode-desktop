//! Which machine and directory a new terminal opens in.
//!
//! On Windows a terminal follows the active panel's session: a WSL chat
//! (`ssh://wsl/...` or `ssh://<Distro>.wsl/...`) opens that distribution's
//! shell in the chat's Linux working directory, and a local chat opens
//! PowerShell in its Windows directory. The destination travels to the host
//! as a validated `wsl://` string (see `jcode_desktop_api::terminal_target`).
//! Other platforms keep the launch working directory.
use super::*;
use jcode_desktop_api::terminal_target::{WSL_SCHEME, encode_wsl, wsl_host};

/// Pure resolution, testable on every platform. `session_host` is the SSH
/// host of the session (or of its pending draft), `default_host` the default
/// machine for new chats, `fallback` the launch working directory.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn terminal_working_dir(
    session_host: Option<&str>,
    working_dir: Option<&str>,
    default_host: Option<&str>,
    fallback: Option<String>,
) -> Option<String> {
    let directory = working_dir.map(str::trim).filter(|dir| !dir.is_empty());
    // A terminal opened from a terminal starts where that one did.
    if let Some(dir) = directory.filter(|dir| dir.starts_with(WSL_SCHEME)) {
        return Some(dir.to_owned());
    }
    match session_host {
        Some(host) => {
            if let Some(distribution) = wsl_host(host) {
                return encode_wsl(distribution, directory)
                    .or_else(|| encode_wsl(distribution, None));
            }
        }
        None => {
            if let Some(dir) = directory {
                return Some(dir.to_owned());
            }
        }
    }
    // No session directory (empty workspace, service panels, other SSH
    // hosts): a WSL default machine still means the work lives in WSL.
    if let Some(distribution) = default_host.and_then(wsl_host) {
        return encode_wsl(distribution, None);
    }
    fallback
}

impl Workspace {
    pub(super) fn new_terminal_working_dir(&self, cx: &App) -> Option<String> {
        if !cfg!(windows) {
            return self.default_working_dir();
        }
        let panel = self
            .slots
            .get(self.active)
            .filter(|slot| !slot.closing)
            .map(|slot| slot.panel.read(cx));
        let session_id = panel.map(|panel| panel.session_id.as_str()).unwrap_or_default();
        let host = harness::remote_host(session_id)
            .or_else(|| pending::remote_draft_host(session_id).map(str::to_owned));
        terminal_working_dir(
            host.as_deref(),
            panel.and_then(|panel| panel.working_dir.as_deref()),
            self.remotes.default_host.as_deref(),
            self.default_working_dir(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_follow_the_active_session_machine() {
        let home = Some("C:\\Users\\me".to_owned());
        let resolve = |host, dir, default| terminal_working_dir(host, dir, default, home.clone());
        assert_eq!(
            resolve(Some("wsl"), Some("/home/me/repo"), None).as_deref(),
            Some("wsl:///home/me/repo")
        );
        assert_eq!(
            resolve(Some("Ubuntu.wsl"), Some("/home/me/repo"), None).as_deref(),
            Some("wsl://Ubuntu/home/me/repo")
        );
        // A WSL chat without a usable Linux directory opens WSL home.
        assert_eq!(resolve(Some("wsl"), None, None).as_deref(), Some("wsl://"));
        assert_eq!(resolve(Some("wsl"), Some("C:\\x"), None).as_deref(), Some("wsl://"));
        // Local chats keep their Windows directory even with a WSL default.
        assert_eq!(
            resolve(None, Some("C:\\work"), Some("wsl")).as_deref(),
            Some("C:\\work")
        );
        assert_eq!(
            resolve(None, Some("wsl://Ubuntu/srv"), None).as_deref(),
            Some("wsl://Ubuntu/srv")
        );
        assert_eq!(resolve(None, None, Some("wsl")).as_deref(), Some("wsl://"));
        assert_eq!(resolve(Some("server"), Some("/srv"), Some("Debian.wsl")).as_deref(), Some("wsl://Debian"));
        assert_eq!(resolve(Some("server"), Some("/srv"), None), home);
        assert_eq!(resolve(None, None, None), home);
    }

    /// Windows only. Ctrl+` (Win+T belongs to the taskbar there) opens a
    /// terminal for the active WSL chat. With `JCODE_WSL_TEST_REPO` set, the
    /// exact command the host builds is run through wsl.exe to prove the
    /// shell starts in the chat's checkout.
    #[cfg(windows)]
    #[gpui::test]
    fn ctrl_backtick_opens_a_terminal_in_the_active_wsl_session(cx: &mut gpui::TestAppContext) {
        let repo = std::env::var("JCODE_WSL_TEST_REPO")
            .unwrap_or_else(|_| "/home/me/repo".into());
        cx.update(crate::bind_workspace_keys);
        let (workspace, vcx) = cx.add_window_view(|window, cx| {
            let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
            workspace.push_test_panel("local-chat", cx);
            workspace.push_test_panel("ssh://wsl/session_term", cx);
            workspace.slots[0]
                .panel
                .update(cx, |panel, _| panel.working_dir = Some("C:\\work".into()));
            workspace.slots[1]
                .panel
                .update(cx, |panel, _| panel.working_dir = Some(repo.clone()));
            workspace.active = 1;
            workspace.restore_focus(window, cx);
            workspace
        });
        vcx.run_until_parked();
        let terminal_dir = |vcx: &mut gpui::VisualTestContext| {
            workspace.read_with(vcx, |w, cx| {
                let panel = w.slots[w.active].panel.read(cx);
                assert_eq!(panel.session_id, "terminal");
                panel.working_dir.clone()
            })
        };
        let activate = |vcx: &mut gpui::VisualTestContext, index: usize| {
            vcx.update(|window, cx| {
                workspace.update(cx, |w, cx| {
                    let target = w
                        .slots
                        .iter()
                        .position(|slot| {
                            slot.panel.read(cx).session_id
                                == ["local-chat", "ssh://wsl/session_term"][index]
                        })
                        .unwrap();
                    w.set_active(target, cx);
                    w.focus_active(window, cx);
                })
            });
            vcx.run_until_parked();
        };
        for chord in ["ctrl-`", "ctrl-alt-t"] {
            activate(vcx, 1);
            vcx.simulate_keystrokes(chord);
            vcx.run_until_parked();
            assert_eq!(terminal_dir(vcx), Some(format!("wsl://{repo}")), "{chord}");
        }
        activate(vcx, 0);
        vcx.simulate_keystrokes("ctrl-`");
        vcx.run_until_parked();
        assert_eq!(terminal_dir(vcx).as_deref(), Some("C:\\work"));

        if std::env::var_os("JCODE_WSL_TEST_REPO").is_some() {
            use jcode_desktop_api::terminal_target::{TerminalTarget, windows_launch};
            let target = TerminalTarget::parse(Some(&format!("wsl://{repo}"))).unwrap();
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
}
