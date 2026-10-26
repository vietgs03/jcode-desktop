//! Worktree creation inside the unified project sidebar. There is no separate
//! worktree mode: every checkout of a project, and the threads (including
//! swarm agents) running in it, share one hierarchy of
//! project, then branch or worktree, then threads.
use super::*;
use jcode_desktop_harness::session_git::{self, Location};

pub(super) const BRANCH_ICON: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><g fill="none" stroke="black" stroke-width="1.6"><circle cx="6" cy="5" r="2.5"/><circle cx="18" cy="5" r="2.5"/><circle cx="6" cy="19" r="2.5"/><path d="M6 7.5v9M18 7.5c0 6-12 2-12 8"/></g></svg>"#;
pub(super) const FOLDER_ICON: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M3 7V5a1 1 0 0 1 1-1h5l2 3h9a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V7Zm0 3h18" fill="none" stroke="black" stroke-width="1.6" stroke-linejoin="round"/></svg>"#;
pub(super) const HIDE_SIDEBAR_ICON: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><g fill="none" stroke="black" stroke-width="1.6" stroke-linejoin="round" stroke-linecap="round"><rect x="3" y="4" width="18" height="16" rx="2.5"/><path d="M9 4v16M16 9.5 13.5 12l2.5 2.5"/></g></svg>"#;

#[derive(Default)]
pub(super) struct State {
    pub(super) creating: bool,
    pub(super) error: Option<String>,
    pub(super) input: Option<Entity<PromptInput>>,
    /// Checkout the new worktree branches from, and the machine it is on.
    pub(super) input_directory: Option<String>,
    pub(super) input_location: Option<Location>,
    creation: Option<gpui::Task<()>>,
}

/// A compact pill button used by sidebar project and checkout headers. It acts
/// on mouse-down and stops propagation so the header never toggles as well.
pub(super) fn header_action(
    id: gpui::SharedString,
    tooltip: &'static str,
    content: gpui::AnyElement,
    visible: bool,
    group: &'static str,
) -> gpui::Stateful<gpui::Div> {
    let selector = id.to_string();
    div()
        .id(id)
        .debug_selector(move || selector.clone())
        .flex_none()
        .size(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .text_size(px(13.0))
        .text_color(Theme::global().TEXT_DIM)
        .opacity(if visible { 1.0 } else { 0.0 })
        .group_hover(group, |el| el.opacity(1.0))
        .hover(|el| {
            el.bg(Theme::global().PANEL_BG)
                .text_color(Theme::global().TEXT)
        })
        .tooltip(move |_, cx| cx.new(|_| remotes::HeaderTooltip(tooltip.into())).into())
        .child(content)
}

/// Hover-only header actions. They float over the right end of the row, so the
/// label and branch pill use the full width until the pointer arrives. The
/// parent must be `relative()`; `right` leaves room for trailing content.
pub(super) fn hover_actions(group: &'static str, right: f32) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right(px(right))
        .flex()
        .items_center()
        .gap_1()
        .pl_1()
        .rounded_full()
        .bg(Theme::global().TOOL_BG)
        .opacity(0.0)
        .group_hover(group, |el| el.opacity(1.0))
}

pub(super) fn icon(data: &'static [u8], size: f32) -> gpui::AnyElement {
    gpui::svg()
        .data(data)
        .size(px(size))
        .flex_none()
        .text_color(Theme::global().TEXT_DIM)
        .into_any_element()
}

impl Workspace {
    pub(super) fn init_worktree_fixture(&mut self, cx: &mut Context<Self>) {
        if !harness::screenshot_mode()
            || std::env::var_os("JCODE_DESKTOP_SCREENSHOT_WORKTREES").is_none()
        {
            return;
        }
        // Real `.git` markers so the sidebar exercises its filesystem probe.
        let root =
            std::env::temp_dir().join(format!("jcode-worktree-fixture-{}", std::process::id()));
        let main = root.join("example");
        let _ = std::fs::create_dir_all(main.join(".git/worktrees/search"));
        let _ = std::fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n");
        let search = root.join("example-search");
        let _ = std::fs::create_dir_all(&search);
        let gitdir = main.join(".git/worktrees/search");
        let _ = std::fs::write(
            search.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        );
        let _ = std::fs::write(gitdir.join("commondir"), "../..\n");
        let _ = std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature/search\n");
        let main = main.to_string_lossy().into_owned();
        let search = search.to_string_lossy().into_owned();
        if let Some(first) = self.sessions.first_mut() {
            first.working_dir = Some(main.clone());
        }
        for slot in &self.slots {
            slot.panel
                .update(cx, |panel, _| panel.working_dir = Some(main.clone()));
        }
        let mut session = self.sessions[0].clone();
        session.session_id = "worktree-fixture-search".into();
        session.title = Some("Build project search".into());
        session.working_dir = Some(search.clone());
        session.updated_at_ms = Some(unix_now_ms() - 120_000);
        self.sessions.push(session.clone());
        session.session_id = "worktree-fixture-index".into();
        session.title = Some("Index workspace files".into());
        session.updated_at_ms = Some(unix_now_ms() - 7_200_000);
        self.sessions.push(session);
    }

    /// Open a chat in `directory` on `location`: a local draft, or a WSL
    /// draft whose session the bridge creates on that distribution.
    pub(super) fn open_session_in(
        &mut self,
        location: &Location,
        directory: String,
        title: String,
        cx: &mut Context<Self>,
    ) {
        match location.host() {
            None => self.open_local_draft(Some(directory), cx),
            Some(host) => {
                let request_id = pending::next_remote_draft_id(&host);
                self.mount_session_draft(request_id.clone(), Some(directory.clone()), title, cx);
                self.bridge.send(Command::CreateRemoteSession {
                    host,
                    working_dir: Some(directory),
                    request_id: Some(request_id),
                });
            }
        }
    }

    /// Shared by `/worktree` and the sidebar: create `branch` from HEAD of
    /// `directory` in its own checkout on the same machine (Git runs on a
    /// background thread, through wsl.exe for WSL), then open a chat there.
    /// `done` sees the result, `Ok(path)` after the chat was opened.
    pub(super) fn create_worktree_session(
        &mut self,
        location: Location,
        directory: String,
        branch: String,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, Result<String, String>, &mut Context<Self>) + 'static,
    ) -> gpui::Task<()> {
        let task = cx.background_executor().spawn({
            let location = location.clone();
            let branch = branch.clone();
            async move { session_git::create_worktree(&location, &directory, &branch) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(path) = &result {
                    // The new thread is what makes the worktree appear
                    // under its project, so always open one.
                    this.focus_pending = true;
                    this.open_session_in(&location, path.clone(), format!("{branch} · worktree"), cx);
                }
                done(this, result, cx);
                cx.notify();
            });
        })
    }

    pub(super) fn begin_worktree(
        &mut self,
        location: Location,
        directory: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.worktrees.creating {
            return;
        }
        let weak = cx.weak_entity();
        let cancel = weak.clone();
        let input = cx.new(|cx| {
            PromptInput::new(
                cx,
                "New branch name, e.g. feature/search",
                move |branch, _, _, app| {
                    let _ = weak.update(app, |this, cx| this.create_worktree(&branch, cx));
                },
            )
            .with_on_overlay_cancel(move |app| {
                let _ = cancel.update(app, |this, cx| this.cancel_worktree(cx));
                true
            })
        });
        window.focus(&input.read(cx).focus_handle.clone(), cx);
        self.focus_pending = false;
        self.worktrees.input = Some(input);
        self.worktrees.input_directory = Some(directory);
        self.worktrees.input_location = Some(location);
        self.worktrees.error = None;
        cx.notify();
    }

    fn cancel_worktree(&mut self, cx: &mut Context<Self>) {
        if !self.worktrees.creating {
            self.worktrees.input = None;
            self.worktrees.input_directory = None;
            self.worktrees.input_location = None;
            self.worktrees.error = None;
            self.focus_pending = true;
            cx.notify();
        }
    }

    pub(super) fn create_worktree(&mut self, branch: &str, cx: &mut Context<Self>) {
        if self.worktrees.creating {
            return;
        }
        let Some(directory) = self.worktrees.input_directory.clone() else {
            return;
        };
        let branch = branch.trim().to_owned();
        if branch.is_empty() {
            self.worktrees.error = Some("Enter a new branch name.".into());
            cx.notify();
            return;
        }
        self.worktrees.creating = true;
        self.worktrees.error = None;
        let location = self
            .worktrees
            .input_location
            .clone()
            .unwrap_or(Location::Local);
        self.worktrees.creation = Some(self.create_worktree_session(
            location,
            directory,
            branch,
            cx,
            |this, result, _| {
                this.worktrees.creating = false;
                match result {
                    Ok(_) => {
                        this.worktrees.input = None;
                        this.worktrees.input_directory = None;
                        this.worktrees.input_location = None;
                    }
                    Err(error) => this.worktrees.error = Some(error),
                }
            },
        ));
        cx.notify();
    }

    /// Inline form shown above the thread list while naming a new worktree.
    pub(super) fn render_worktree_form(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let input = self.worktrees.input.clone()?;
        let machine = self
            .worktrees
            .input_location
            .as_ref()
            .and_then(Location::host)
            .map(|host| format!(" on {host}"))
            .unwrap_or_default();
        let origin = self
            .worktrees
            .input_directory
            .as_deref()
            .map(|dir| {
                let name = dir
                    .trim_end_matches(['/', '\\'])
                    .rsplit(['/', '\\'])
                    .next()
                    .filter(|name| !name.is_empty())
                    .unwrap_or(dir);
                format!("{name}{machine}")
            })
            .unwrap_or_default();
        Some(
            div()
                .id("worktree-create-form")
                .debug_selector(|| "worktree-create-form".into())
                .flex_none()
                .mx_2()
                .mb_2()
                .p_2()
                .rounded_xl()
                .bg(Theme::global().PANEL_BG)
                .flex()
                .flex_col()
                .gap_2()
                .capture_action(cx.listener(|this, _: &crate::input::Submit, _, cx| {
                    cx.stop_propagation();
                    if let Some(input) = &this.worktrees.input {
                        let branch = input.read(cx).content.to_string();
                        this.create_worktree(&branch, cx);
                    }
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_size(px(11.0))
                        .text_color(Theme::global().TEXT_DIM)
                        .child(icon(BRANCH_ICON, 12.0))
                        .child(if self.worktrees.creating {
                            format!("Creating worktree in {origin}…")
                        } else {
                            format!("New worktree in {origin}")
                        }),
                )
                .child(input)
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(Theme::global().TEXT_DIM)
                        .child("Branches from HEAD. Uncommitted edits stay put. Enter to create, Esc to cancel."),
                )
                .children(self.worktrees.error.clone().map(|error| {
                    div()
                        .id("worktree-error")
                        .debug_selector(|| "worktree-error".into())
                        .text_size(px(11.0))
                        .text_color(Theme::global().ERROR)
                        .child(error)
                }))
                .into_any_element(),
        )
    }

    /// Always-visible entry points at the top of the thread list.
    pub(super) fn render_sidebar_quick_actions(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let pill = |id: &'static str, label: &'static str, glyph: gpui::AnyElement| {
            let tip = match id {
                "sidebar-open-project" => "Open a project folder",
                _ => "New thread in the default directory",
            };
            div()
                .id(id)
                .debug_selector(move || id.into())
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .h(px(26.0))
                .px_2()
                .flex()
                .items_center()
                .justify_center()
                .gap_1()
                .rounded_full()
                .cursor_pointer()
                .bg(Theme::global().TOOL_BG)
                .text_size(px(11.0))
                .text_color(Theme::global().TEXT_DIM)
                .hover(|el| {
                    el.bg(Theme::global().PANEL_BG)
                        .text_color(Theme::global().TEXT)
                })
                .tooltip(move |_, cx| cx.new(|_| remotes::HeaderTooltip(tip.into())).into())
                .child(glyph)
                .child(div().min_w_0().truncate().child(label))
        };
        div()
            .flex_none()
            .w_full()
            .min_w_0()
            .pl_2()
            .pr(px(crate::scrollbar::GUTTER + 4.0))
            .pt_2()
            .flex()
            .gap_2()
            .child(
                pill(
                    "sidebar-new-thread",
                    "Thread",
                    div().child("+").into_any_element(),
                )
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                        this.missed("new_panel", cx);
                        this.open_new_session(cx);
                    }),
                ),
            )
            .child(
                pill("sidebar-open-project", "Project", icon(FOLDER_ICON, 12.0)).on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                        this.open_folder(&OpenFolder, window, cx);
                    }),
                ),
            )
            .child(
                // Opens the session browser (same as /resume, Alt+R).
                div()
                    .id("sidebar-all-sessions")
                    .debug_selector(|| "sidebar-all-sessions".into())
                    .flex_none()
                    .size(px(26.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .bg(Theme::global().TOOL_BG)
                    .text_size(px(12.0))
                    .text_color(Theme::global().TEXT_DIM)
                    .hover(|el| el.bg(Theme::global().PANEL_BG))
                    .tooltip(|_, cx| {
                        cx.new(|_| remotes::HeaderTooltip("All sessions (Alt+R)".into())).into()
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.open_resume(&OpenResume, window, cx);
                        }),
                    )
                    .child("☰"),
            )
            .child(
                // Icon-only, like hide: four labelled pills do not fit 224px.
                div()
                    .id("sidebar-new-collection")
                    .debug_selector(|| "sidebar-new-collection".into())
                    .flex_none()
                    .size(px(26.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .bg(Theme::global().TOOL_BG)
                    .hover(|el| el.bg(Theme::global().PANEL_BG))
                    .tooltip(|_, cx| {
                        cx.new(|_| {
                            remotes::HeaderTooltip(
                                "New collection: group chats your own way".into(),
                            )
                        })
                        .into()
                    })
                    .child(icon(sidebar_collections::COLLECTION_ICON, 13.0))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.begin_new_collection(None, window, cx);
                        }),
                    ),
            )
            .child(
                div()
                    .id("sidebar-hide")
                    .debug_selector(|| "sidebar-hide".into())
                    .flex_none()
                    .size(px(26.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .bg(Theme::global().TOOL_BG)
                    .hover(|el| el.bg(Theme::global().PANEL_BG))
                    .tooltip(|_, cx| {
                        cx.new(|_| remotes::HeaderTooltip("Hide sidebar (Ctrl+B)".into()))
                            .into()
                    })
                    .child(icon(HIDE_SIDEBAR_ICON, 14.0))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                            if this.compact_sidebar_open {
                                // Narrow windows show the sidebar as a drawer:
                                // just dismiss it and keep the menu button.
                                this.compact_sidebar_open = false;
                                this.focus_pending = true;
                                cx.notify();
                            } else {
                                this.toggle_sidebar(&ToggleSidebar, window, cx);
                            }
                        }),
                    ),
            )
            .into_any_element()
    }
}

/// Removes a worktree and its branch that a test created, even on panic.
#[cfg(test)]
pub(super) struct WslWorktreeCleanup {
    pub repo: String,
    pub path: Option<String>,
    pub branch: String,
}

#[cfg(test)]
impl Drop for WslWorktreeCleanup {
    fn drop(&mut self) {
        let wsl = jcode_desktop_harness::session_git::Location::Wsl(None);
        if let Some(path) = &self.path {
            let _ = wsl.git(&self.repo, &["worktree", "remove", "--", path]);
        }
        let _ = wsl.git(&self.repo, &["branch", "-d", "--", &self.branch]);
    }
}

#[cfg(test)]
#[path = "sidebar_worktrees_tests.rs"]
mod tests;
