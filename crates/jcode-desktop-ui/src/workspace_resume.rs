//! A transient, keyboard-first session browser. It never becomes a runtime slot.
use super::*;
use crate::resume_content_search as content_search;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) enum Filter {
    #[default]
    All,
    Active,
    Saved,
    /// Sessions whose process died mid-work, so they can be recovered.
    Crashed,
}

/// Legacy is only a decode fallback. Every new snapshot explicitly records
/// Closed or Open so --resume cannot reopen a deliberately dismissed picker.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(super) enum Snapshot {
    #[default]
    Legacy,
    Closed,
    Open {
        search: PromptInputSnapshot,
        query: String,
        selected: Option<String>,
        filter: Filter,
        start_on_close: bool,
    },
}

pub(super) struct State {
    pub(super) search: Entity<PromptInput>,
    query: String,
    selected: Option<String>,
    filter: Filter,
    scroll: ScrollHandle,
    pub(super) start_on_close: bool,
    preview_id: Option<String>,
    /// The real chat panel in transcript-only mode, hydrated through the same
    /// `load_history` path a resumed session uses, so the preview is identical.
    preview: Option<Result<Entity<Panel>, String>>,
    preview_task: Option<gpui::Task<()>>,
    /// Sessions the daemon is generating for right now, from its streaming
    /// markers. Refreshed off the UI thread while the picker is open.
    streaming: HashSet<String>,
    _presence_task: Option<gpui::Task<()>>,
    /// Transcript keyword hits for `content_query`, with an excerpt each.
    content_hits: content_search::Hits,
    content_query: Option<String>,
    content_cancel: Arc<AtomicBool>,
    content_task: Option<gpui::Task<()>>,
    /// Machine and repository filters, and grouping rows by repository.
    machine: Machine,
    repo: Option<String>,
    repo_menu: bool,
    group_by_repo: bool,
    /// Stored-record details by session ID, read off the UI thread.
    meta: HashMap<String, Result<SessionMeta, String>>,
    meta_task: Option<gpui::Task<()>>,
    /// Git branch by working directory (`None` outside Git), probed in batches.
    branches: HashMap<String, Option<String>>,
    branch_task: Option<gpui::Task<()>>,
    /// A remote preview is waiting for `Update::ProvisionalHistory`.
    peek_pending: Option<String>,
    /// The remote preview asked for the full stored history.
    preview_full: bool,
    /// Child index of each match in the results list (headers shift them).
    row_children: Vec<usize>,
}

pub(super) use jcode_desktop_harness::session_meta::SessionMeta;

/// Messages a remote preview asks for, and for "Load full history".
const PEEK_PREVIEW: u32 = 60;
const PEEK_FULL: u32 = 5_000;
const PEEK_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Machine {
    #[default]
    All,
    Wsl,
    Local,
}

/// Where a session runs, as the browser labels it.
fn session_machine(session_id: &str) -> (Machine, String) {
    match harness::remote_host(session_id) {
        None if cfg!(windows) => (Machine::Local, "Windows".into()),
        None => (Machine::Local, "Local".into()),
        Some(host) => match jcode_sdk::wsl_distro(&host) {
            Some(Some(distro)) => (Machine::Wsl, format!("WSL: {distro}")),
            Some(None) => (Machine::Wsl, "WSL".into()),
            None => (Machine::All, host),
        },
    }
}

fn local_machine_label() -> &'static str {
    if cfg!(windows) { "Windows" } else { "Local" }
}

/// The repository a session belongs to: its folder name.
fn session_repo(session: &jcode_sdk::SessionInfo) -> Option<String> {
    let dir = session.working_dir.as_deref()?.trim().trim_end_matches(['/', '\\']);
    let name = dir.rsplit(['/', '\\']).next()?;
    (!name.is_empty()).then(|| name.to_owned())
}

fn branch_key(location: &jcode_desktop_harness::session_git::Location, dir: &str) -> String {
    format!("{location:?}\u{1f}{dir}")
}

/// `2026-10-23 14:02 UTC` from an RFC 3339 timestamp.
fn short_timestamp(value: &str) -> String {
    match value.get(..16) {
        Some(head) if value.ends_with('Z') => format!("{} UTC", head.replace('T', " ")),
        Some(head) => head.replace('T', " "),
        None => value.to_owned(),
    }
}

/// Let typing settle before scanning every stored transcript.
const CONTENT_SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);

const PRESENCE_REFRESH: Duration = Duration::from_millis(1500);

/// How a row relates to live work, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Liveness {
    Working,
    Open,
    /// Not running, and its last owner died without a clean shutdown.
    Crashed,
    Inactive,
}

fn status_is_crashed(status: &str) -> bool {
    status == "crashed"
}

fn status_is_working(status: &str) -> bool {
    matches!(
        status,
        "running"
            | "working"
            | "busy"
            | "streaming"
            | "generating"
            | "thinking"
            | "running_tools"
            | "compacting"
            | "processing"
    )
}

/// Streaming markers written by every live jcode process, filtered to
/// sessions whose owning process is still alive.
fn streaming_sessions() -> HashSet<String> {
    if cfg!(test) || harness::screenshot_mode() {
        return HashSet::new();
    }
    jcode_base::session::session_presence()
        .into_iter()
        .filter(|presence| presence.streaming)
        .map(|presence| presence.session_id)
        .collect()
}

#[cfg(test)]
use crate::commands::is_resume_command;

fn filtered_sessions(
    mut sessions: Vec<jcode_sdk::SessionInfo>,
    query: &str,
    filter: Filter,
    working: &HashSet<String>,
    content: &content_search::Hits,
) -> Vec<jcode_sdk::SessionInfo> {
    let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut metadata_hits = HashSet::new();
    sessions.retain(|session| {
        let text = format!(
            "{} {} {} {} {} {}",
            session.session_id,
            session.title.as_deref().unwrap_or_default(),
            session.save_label.as_deref().unwrap_or_default(),
            session.working_dir.as_deref().unwrap_or_default(),
            session.status,
            session.agent_label.as_deref().unwrap_or_default()
        )
        .to_lowercase();
        !session.archived
            && match filter {
                Filter::All => true,
                Filter::Active => {
                    working.contains(&session.session_id) || status_is_working(&session.status)
                }
                Filter::Saved => session.saved,
                Filter::Crashed => {
                    status_is_crashed(&session.status) && !working.contains(&session.session_id)
                }
            }
            && if words.iter().all(|word| text.contains(word)) {
                metadata_hits.insert(session.session_id.clone());
                true
            } else {
                content.contains_key(&session.session_id)
            }
    });
    // Title, folder, and ID matches outrank sessions found only by transcript.
    sessions.sort_by(|a, b| {
        metadata_hits
            .contains(&b.session_id)
            .cmp(&metadata_hits.contains(&a.session_id))
            .then_with(|| b.saved.cmp(&a.saved))
            .then_with(|| {
                b.last_active_at_ms
                    .or(b.updated_at_ms)
                    .cmp(&a.last_active_at_ms.or(a.updated_at_ms))
            })
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    let mut seen = HashSet::new();
    sessions.retain(|session| seen.insert(session.session_id.clone()));
    sessions
}

impl Workspace {
    pub(super) fn resume_snapshot(&self, cx: &App) -> Snapshot {
        match &self.resume {
            None => Snapshot::Closed,
            Some(state) => Snapshot::Open {
                search: state.search.read(cx).snapshot(),
                query: state.query.clone(),
                selected: state.selected.clone(),
                filter: state.filter,
                start_on_close: state.start_on_close,
            },
        }
    }

    pub(super) fn restore_resume(
        &mut self,
        snapshot: Option<Snapshot>,
        resume_requested: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resume = None;
        match snapshot {
            Some(Snapshot::Open {
                search,
                query,
                selected,
                filter,
                start_on_close,
            }) => {
                self.open_resume(&OpenResume, window, cx);
                let state = self.resume.as_mut().unwrap();
                state
                    .search
                    .update(cx, |input, cx| input.restore(search, cx));
                state.query = query;
                state.selected = selected;
                state.filter = filter;
                state.start_on_close = start_on_close;
                self.refresh_resume_content(cx);
            }
            None | Some(Snapshot::Legacy) if resume_requested => {
                // Old linked hosts omitted picker state. Recover only while the
                // active slot is still the unstarted startup draft, not a chat.
                if snapshot.is_none()
                    || self
                        .slots
                        .get(self.active)
                        .is_some_and(|slot| slot.panel.read(cx).is_startup_draft())
                {
                    self.open_resume(&OpenResume, window, cx);
                    self.resume.as_mut().unwrap().start_on_close = true;
                }
            }
            _ => {}
        }
    }

    fn resume_sessions(&self, cx: &App) -> Vec<jcode_sdk::SessionInfo> {
        let mut sessions = self.sessions.clone();
        // Keep local drafts and already-open chats reachable when switching the
        // standalone window's primary conversation. Never discard their editors.
        for slot in &self.slots {
            let panel = slot.panel.read(cx);
            if !slot.closing
                && panel.supports_voice()
                && !sessions.iter().any(|s| s.session_id == panel.session_id)
            {
                sessions.push(jcode_sdk::SessionInfo {
                    user_connected: false,
                    session_id: panel.session_id.clone(),
                    title: Some(panel.title.to_string()),
                    working_dir: panel.working_dir.clone(),
                    status: "open".into(),
                    transcript_bytes: None,
                    saved: false,
                    updated_at_ms: None,
                    last_active_at_ms: None,
                    archived: false,
                    archived_at_ms: None,
                    save_label: None,
                    parent_session_id: None,
                    agent_label: None,
                    swarm_status: None,
                    edit_stats: None,
                });
            }
        }
        sessions
    }

    /// Sessions open in this window, and whether each is mid-turn.
    fn resume_open_activity(&self, cx: &App) -> HashMap<String, bool> {
        self.slots
            .iter()
            .filter(|slot| !slot.closing)
            .map(|slot| {
                let panel = slot.panel.read(cx);
                (panel.session_id.clone(), panel.sidebar_activity().is_some())
            })
            .collect()
    }

    fn resume_working(&self, cx: &App) -> HashSet<String> {
        let mut working = self
            .resume
            .as_ref()
            .map(|state| state.streaming.clone())
            .unwrap_or_default();
        working.extend(
            self.resume_open_activity(cx)
                .into_iter()
                .filter_map(|(id, active)| active.then_some(id)),
        );
        working
    }

    fn resume_matches(&self, cx: &App) -> Vec<jcode_sdk::SessionInfo> {
        let Some(state) = &self.resume else {
            return Vec::new();
        };
        let mut sessions = filtered_sessions(
            self.resume_sessions(cx),
            &state.query,
            state.filter,
            &self.resume_working(cx),
            state
                .content_query
                .as_ref()
                .filter(|query| **query == state.query)
                .map(|_| &state.content_hits)
                .unwrap_or(&content_search::Hits::new()),
        );
        sessions.retain(|session| {
            (state.machine == Machine::All
                || session_machine(&session.session_id).0 == state.machine)
                && state
                    .repo
                    .as_ref()
                    .is_none_or(|repo| session_repo(session).as_ref() == Some(repo))
        });
        if state.group_by_repo {
            // Stable: recency order is kept inside each repository group.
            sessions.sort_by_cached_key(|session| {
                session_repo(session).map(|repo| repo.to_lowercase())
                    .unwrap_or_else(|| "\u{10ffff}".into())
            });
        }
        sessions
    }

    /// Repositories in the catalog after the machine filter, by name.
    fn resume_repos(&self, cx: &App) -> Vec<String> {
        let machine = self.resume.as_ref().map(|s| s.machine).unwrap_or_default();
        let mut repos: Vec<String> = self
            .resume_sessions(cx)
            .iter()
            .filter(|s| !s.archived)
            .filter(|s| machine == Machine::All || session_machine(&s.session_id).0 == machine)
            .filter_map(session_repo)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        repos.sort_by_key(|repo| repo.to_lowercase());
        repos
    }

    /// Whether the transcript scan for the current query is still running.
    fn resume_content_pending(&self) -> bool {
        self.resume.as_ref().is_some_and(|state| {
            !content_search::query_words(&state.query).is_empty()
                && state.content_query.as_ref() != Some(&state.query)
        })
    }

    /// Restart the transcript scan for the current query, cancelling the
    /// previous one. Results only apply while the query is unchanged.
    fn refresh_resume_content(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.resume.as_mut() else {
            return;
        };
        state.content_cancel.store(true, Ordering::Relaxed);
        state.content_task = None;
        let words = content_search::query_words(&state.query);
        let dir = (!cfg!(test)).then(content_search::sessions_dir).flatten();
        let (false, Some(dir)) = (words.is_empty(), dir) else {
            state.content_hits.clear();
            state.content_query = Some(state.query.clone());
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        state.content_cancel = cancel.clone();
        let query = state.query.clone();
        state.content_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(CONTENT_SEARCH_DEBOUNCE)
                .await;
            let scan_cancel = cancel.clone();
            let Some(hits) = cx
                .background_executor()
                .spawn(async move { content_search::scan(&dir, &words, &scan_cancel) })
                .await
            else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(state) = this.resume.as_mut()
                    && state.query == query
                    && !cancel.load(Ordering::Relaxed)
                {
                    state.content_hits = hits;
                    state.content_query = Some(query);
                    cx.notify();
                }
            });
        }));
    }

    /// Poll the daemon's streaming markers while the picker stays open.
    /// Dropping the picker state drops this task and stops the loop.
    fn watch_resume_presence(cx: &mut Context<Self>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let streaming = cx
                    .background_executor()
                    .spawn(async { streaming_sessions() })
                    .await;
                let open = this
                    .update(cx, |this, cx| {
                        let Some(state) = this.resume.as_mut() else {
                            return false;
                        };
                        if state.streaming != streaming {
                            state.streaming = streaming;
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !open {
                    return;
                }
                cx.background_executor().timer(PRESENCE_REFRESH).await;
            }
        })
    }

    pub(super) fn open_resume(
        &mut self,
        _: &OpenResume,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &self.resume {
            window.focus(&state.search.focus_handle(cx), cx);
            return;
        }
        let submit = cx.weak_entity();
        let change = submit.clone();
        let search = cx.new(|cx| {
            PromptInput::new(
                cx,
                "Search sessions by title, folder, ID, or conversation…",
                move |_, _, window, app| {
                    let _ = submit.update(app, |this, cx| this.resume_selected(window, cx));
                },
            )
            .with_on_change(move |query, app| {
                let _ = change.update(app, |this, cx| {
                    if let Some(state) = &mut this.resume {
                        state.query = query.to_owned();
                        state.selected = None;
                        state.scroll.scroll_to_item(0);
                    }
                    this.refresh_resume_content(cx);
                    cx.notify();
                });
            })
        });
        window.focus(&search.focus_handle(cx), cx);
        self.focus_pending = false;
        self.resume = Some(State {
            search,
            query: String::new(),
            selected: None,
            filter: Filter::All,
            scroll: ScrollHandle::new(),
            start_on_close: false,
            preview_id: None,
            preview: None,
            preview_task: None,
            streaming: HashSet::new(),
            _presence_task: None,
            content_hits: content_search::Hits::new(),
            content_query: None,
            content_cancel: Arc::new(AtomicBool::new(false)),
            content_task: None,
            machine: Machine::All,
            repo: None,
            repo_menu: false,
            group_by_repo: false,
            meta: HashMap::new(),
            meta_task: None,
            branches: HashMap::new(),
            branch_task: None,
            peek_pending: None,
            preview_full: false,
            row_children: Vec::new(),
        });
        let presence = Self::watch_resume_presence(cx);
        if let Some(state) = self.resume.as_mut() {
            state._presence_task = Some(presence);
        }
        cx.notify();
    }

    fn close_resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let start_on_close = self.resume.take().is_some_and(|state| state.start_on_close);
        if start_on_close && (self.connected || self.remotes.default_host.is_some()) {
            self.start_default_startup(cx);
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn resume_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.resume_selected_in(false, window, cx);
    }

    /// Open the selected row. `separate` (Ctrl+Enter) keeps the current chat:
    /// a standalone window opens the session in its own window, while the
    /// panel strip already adds it next to the current panel.
    fn resume_selected_in(&mut self, separate: bool, window: &mut Window, cx: &mut Context<Self>) {
        let matches = self.resume_matches(cx);
        let selected = self.resume.as_ref().and_then(|s| s.selected.as_deref());
        let Some(session) = matches
            .iter()
            .find(|s| Some(s.session_id.as_str()) == selected)
            .or(matches.first())
            .cloned()
        else {
            return;
        };
        self.resume = None;
        if separate && self.single_panel {
            self.open_fork_window(&session.session_id);
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        self.resume_session(session, window, cx);
    }

    /// Single click selects and previews. Double click opens.
    fn click_resume_row(
        &mut self,
        id: &str,
        event: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = &mut self.resume else { return };
        state.selected = Some(id.to_owned());
        state.repo_menu = false;
        if event.click_count() >= 2 {
            self.resume_selected(window, cx);
        } else {
            window.focus(&state.search.focus_handle(cx), cx);
            cx.notify();
        }
    }

    /// Open a session named on the command line. The catalog has not loaded
    /// yet, so the title and directory arrive with the next session list.
    pub(super) fn open_requested_session(
        &mut self,
        session_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = self
            .sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .cloned()
            .unwrap_or(jcode_sdk::SessionInfo {
                user_connected: false,
                edit_stats: None,
                session_id,
                parent_session_id: None,
                agent_label: None,
                swarm_status: None,
                working_dir: None,
                title: None,
                status: "idle".into(),
                transcript_bytes: None,
                saved: false,
                save_label: None,
                updated_at_ms: None,
                last_active_at_ms: None,
                archived: false,
                archived_at_ms: None,
            });
        self.resume_session(session, window, cx);
    }

    fn resume_session(
        &mut self,
        session: jcode_sdk::SessionInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_session(session, window, cx);
        if self.slots[self.active].panel.read(cx).is_startup_draft()
            && (self.connected || self.remotes.default_host.is_some())
        {
            self.start_default_startup(cx);
        }
        if self.single_panel {
            // Slot zero is the standalone chat anchor. Utilities must return to
            // the resumed chat, not resurrect the previous chat as a back page.
            self.slots.swap(0, self.active);
            self.active = 0;
            self.active_row = 0;
            self.show_sidebar = false;
            self.overview = false;
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn resume_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                if self.resume.as_ref().is_some_and(|s| s.repo_menu) {
                    self.resume.as_mut().unwrap().repo_menu = false;
                    cx.notify();
                } else {
                    self.close_resume(window, cx)
                }
            }
            "enter" => self.resume_selected_in(
                event.keystroke.modifiers.control || event.keystroke.modifiers.platform,
                window,
                cx,
            ),
            "1" | "2" | "3" | "4" if event.keystroke.modifiers.control => {
                self.set_resume_filter(
                    match key {
                        "2" => Filter::Active,
                        "3" => Filter::Saved,
                        "4" => Filter::Crashed,
                        _ => Filter::All,
                    },
                    cx,
                );
            }
            "up" | "down" | "pageup" | "pagedown" => self.move_resume_selection(key, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn move_resume_selection(&mut self, key: &str, cx: &mut Context<Self>) {
        let matches = self.resume_matches(cx);
        if let Some(state) = &mut self.resume {
            let current = matches
                .iter()
                .position(|s| Some(&s.session_id) == state.selected.as_ref())
                .unwrap_or(0);
            let next = match key {
                "up" => current.saturating_sub(1),
                "pageup" => current.saturating_sub(8),
                "pagedown" => (current + 8).min(matches.len().saturating_sub(1)),
                _ => (current + 1).min(matches.len().saturating_sub(1)),
            };
            state.selected = matches.get(next).map(|s| s.session_id.clone());
            let child = state.row_children.get(next).copied().unwrap_or(next);
            state.scroll.scroll_to_item(child);
            cx.notify();
        }
    }

    fn set_resume_machine(&mut self, machine: Machine, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.resume {
            state.machine = machine;
            state.repo = None;
            state.selected = None;
            state.scroll.scroll_to_item(0);
        }
        cx.notify();
    }

    fn set_resume_repo(&mut self, repo: Option<String>, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.resume {
            state.repo = repo;
            state.repo_menu = false;
            state.selected = None;
            state.scroll.scroll_to_item(0);
        }
        cx.notify();
    }

    fn set_resume_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.resume {
            state.filter = filter;
            state.selected = None;
            state.scroll.scroll_to_item(0);
        }
        cx.notify();
    }

    pub(super) fn render_resume(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let matches = self.resume_matches(cx);
        let working = self.resume_working(cx);
        let open_activity = self.resume_open_activity(cx);
        let open_spinners: HashMap<String, gpui::AnyView> = self
            .slots
            .iter()
            .filter_map(|slot| {
                let panel = slot.panel.read(cx);
                panel
                    .sidebar_activity()
                    .map(|view| (panel.session_id.clone(), view))
            })
            .collect();
        let liveness = |session: &jcode_sdk::SessionInfo| {
            if working.contains(&session.session_id) || status_is_working(&session.status) {
                Liveness::Working
            } else if open_activity.contains_key(&session.session_id) {
                Liveness::Open
            } else if status_is_crashed(&session.status) {
                Liveness::Crashed
            } else {
                Liveness::Inactive
            }
        };
        let working_count = matches
            .iter()
            .filter(|session| liveness(session) == Liveness::Working)
            .count();
        let content_pending = self.resume_content_pending();
        let state = self.resume.as_mut().expect("resume picker is open");
        let excerpts = if state.content_query.as_ref() == Some(&state.query) {
            state.content_hits.clone()
        } else {
            content_search::Hits::new()
        };
        // The session catalog arrives asynchronously after a reload. Do not
        // discard the restored selection while only local slots are available.
        if (state.selected.is_none() || !self.sessions.is_empty())
            && !matches
                .iter()
                .any(|s| Some(&s.session_id) == state.selected.as_ref())
        {
            state.selected = matches.first().map(|s| s.session_id.clone());
        }
        let selected = state.selected.clone();
        let search = state.search.clone();
        let scroll = state.scroll.clone();
        let filter = state.filter;
        let group_by_repo = state.group_by_repo;
        let machine_filter = state.machine;
        let repo_filter = state.repo.clone();
        let repo_menu = state.repo_menu;
        self.load_resume_preview(selected.as_deref(), cx);
        self.load_resume_branches(&matches, cx);
        let repos = if repo_menu { self.resume_repos(cx) } else { Vec::new() };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        let theme = Theme::global();
        let mut row_children = Vec::with_capacity(matches.len());
        let mut child_count = 0usize;
        let mut last_group: Option<Option<String>> = None;
        let mut rows = div()
            .id("resume-results")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&scroll);
        for (index, session) in matches.iter().enumerate() {
            let id = session.session_id.clone();
            let is_selected = selected.as_ref() == Some(&id);
            let (icon, title) = sidebar_session_title(session);
            let saved_marker = match session.save_label.as_deref() {
                Some(label) if session.saved => format!("★ {label} · "),
                _ if session.saved => "★ ".to_string(),
                _ => String::new(),
            };
            let live = liveness(session);
            let status = match live {
                Liveness::Working => "working".to_string(),
                Liveness::Open => "open".to_string(),
                Liveness::Crashed | Liveness::Inactive => session.status.clone(),
            };
            let repo = session_repo(session);
            if group_by_repo && last_group.as_ref() != Some(&repo) {
                let header = repo.clone().unwrap_or_else(|| "No repository".into());
                let group = child_count;
                rows = rows.child(
                    div()
                        .id(("resume-group", group))
                        .debug_selector(move || format!("resume-group-{group}"))
                        .px_3()
                        .pt_2()
                        .pb_1()
                        .text_size(px(11.0))
                        .text_color(theme.TEXT_FAINT)
                        .text_ellipsis()
                        .child(header),
                );
                child_count += 1;
                last_group = Some(repo.clone());
            }
            row_children.push(child_count);
            child_count += 1;
            let meta = self
                .resume
                .as_ref()
                .and_then(|s| s.meta.get(&session.session_id))
                .and_then(|m| m.as_ref().ok());
            let when = session
                .last_active_at_ms
                .or(session.updated_at_ms)
                .filter(|ms| *ms > 0)
                .map(|ms| format_time_ago(ms as u64, now_ms));
            let place = [
                repo.clone().or_else(|| {
                    session.working_dir.as_deref().map(compact_working_dir)
                }),
                self.resume_branch(session),
                when,
                Some(session_machine(&session.session_id).1),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            let counts = match meta {
                Some(meta) => [
                    Some(format!(
                        "{} prompt{}",
                        meta.user_messages,
                        if meta.user_messages == 1 { "" } else { "s" }
                    )),
                    meta.model.clone(),
                    Some(format_estimated_tokens(meta.estimated_tokens())),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · "),
                None => sidebar_session_meta(session).unwrap_or_else(|| "Session history".into()),
            };
            let metadata = format!("{saved_marker}{status} · {counts}");
            let badge =
                resume_liveness_badge(live, open_spinners.get(&session.session_id).cloned(), index);
            let excerpt = excerpts.get(&session.session_id).cloned();
            rows = rows.child(
                div()
                    .id(("resume-row", index))
                    .debug_selector(move || format!("resume-row-{index}"))
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .rounded_md()
                    .when(is_selected, |el| el.bg(theme.TOOL_BG))
                    .hover(|el| el.bg(theme.TOOL_BG))
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        this.click_resume_row(&id, event, window, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_ellipsis()
                                    .child(format!("{icon} {title}")),
                            )
                            .children(badge),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.TEXT_DIM)
                            .text_ellipsis()
                            .child(if place.is_empty() {
                                "No working directory".to_string()
                            } else {
                                place
                            }),
                    )
                    .child(
                        div()
                            .debug_selector(move || format!("resume-row-meta-{index}"))
                            .text_size(px(11.0))
                            .text_color(theme.TEXT_FAINT)
                            .text_ellipsis()
                            .child(metadata),
                    )
                    .children(excerpt.map(|excerpt| {
                        div()
                            .debug_selector(move || format!("resume-row-excerpt-{index}"))
                            .text_size(px(11.0))
                            .text_color(theme.TEXT_DIM)
                            .text_ellipsis()
                            .child(excerpt)
                    })),
            );
        }
        if let Some(state) = self.resume.as_mut() {
            state.row_children = row_children;
        }
        if matches.is_empty() {
            rows = rows.child(div().p_4().text_color(theme.TEXT_DIM).child(
                if self.sessions.is_empty() {
                    "No sessions available yet."
                } else if content_pending {
                    "Searching conversations…"
                } else {
                    "No matching sessions. Try another search or filter."
                },
            ));
        }
        let mut preview = div()
            .id("resume-preview")
            .min_h_0()
            .overflow_hidden()
            .debug_selector(|| "resume-preview".into())
            .flex_1()
            .min_w_0()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .bg(theme.TOOL_BG)
            .rounded_lg();
        if let Some(session) = matches
            .iter()
            .find(|s| Some(&s.session_id) == selected.as_ref())
        {
            let (icon, title) = sidebar_session_title(session);
            let info = self.resume_info_lines(session, now_ms);
            let preview_ready = matches!(
                self.resume.as_ref().and_then(|s| s.preview.as_ref()),
                Some(Ok(_))
            );
            let can_load_full = harness::remote_host(&session.session_id).is_some()
                && preview_ready
                && !self.resume.as_ref().is_some_and(|s| s.preview_full);
            let detail = [
                Some(
                    session
                        .working_dir
                        .as_deref()
                        .map(compact_working_dir)
                        .unwrap_or_else(|| "No working directory".into()),
                ),
                Some(format!(
                    "{}{}",
                    match liveness(session) {
                        Liveness::Working => "working now".to_string(),
                        Liveness::Open => "open in Desktop".to_string(),
                        Liveness::Crashed | Liveness::Inactive => session.status.clone(),
                    },
                    if session.saved { " · saved" } else { "" }
                )),
                sidebar_session_meta(session),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            preview = preview.child(
                div()
                    .flex_none()
                    .px_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(16.0))
                                    .text_ellipsis()
                                    .child(format!("{icon} {title}")),
                            )
                            .when(can_load_full, |el| {
                                el.child(
                                    div()
                                        .id("resume-load-full")
                                        .debug_selector(|| "resume-load-full".into())
                                        .flex_none()
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .text_size(px(12.0))
                                        .text_color(theme.TEXT_DIM)
                                        .hover(|el| el.bg(theme.PANEL_BG))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.load_full_resume_preview(cx)
                                        }))
                                        .child("Load full history"),
                                )
                            })
                            .child(
                                div()
                                    .id("resume-open")
                                    .debug_selector(|| "resume-open".into())
                                    .flex_none()
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .bg(theme.PANEL_BG)
                                    .hover(|el| el.opacity(0.85))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.resume_selected(window, cx)
                                    }))
                                    .child("Resume ⏎"),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.TEXT_DIM)
                            .text_ellipsis()
                            .child(detail),
                    )
                    .children(info.into_iter().enumerate().map(|(line, text)| {
                        div()
                            .debug_selector(move || format!("resume-info-{line}"))
                            .text_size(px(11.0))
                            .text_color(theme.TEXT_DIM)
                            .text_ellipsis()
                            .child(text)
                    })),
            );
            let transcript = match self
                .resume
                .as_ref()
                .and_then(|state| state.preview.as_ref())
            {
                None if harness::remote_host(&session.session_id).is_some() => {
                    resume_preview_notice("Loading conversation from the remote machine…")
                }
                None => resume_preview_notice("Loading conversation…"),
                Some(Err(error)) => resume_preview_notice(error.clone()),
                Some(Ok(panel)) if panel.read(cx).items.is_empty() => {
                    resume_preview_notice("No conversation messages yet.")
                }
                Some(Ok(panel)) => panel.clone().into_any_element(),
            };
            preview = preview.child(
                div()
                    .debug_selector(|| "resume-preview-transcript".into())
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .rounded_md()
                    .bg(theme.PANEL_BG)
                    .child(transcript),
            );
        } else {
            preview = preview.child("Select a session to see its details.");
        }
        div()
            .debug_selector(|| "resume-panel".into())
            .size_full()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .bg(theme.PANEL_BG)
            .text_color(theme.TEXT)
            .font_family(theme.FONT_UI)
            .text_size(px(14.0))
            .track_focus(&self.focus_handle)
            .capture_key_down(cx.listener(Self::resume_key_down))
            .capture_action(cx.listener(|this, _: &crate::input::Clear, window, cx| {
                this.close_resume(window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &crate::input::Submit, window, cx| {
                this.resume_selected(window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &crate::input::HistoryPrev, _, cx| {
                this.move_resume_selection("up", cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &crate::input::HistoryNext, _, cx| {
                this.move_resume_selection("down", cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_size(px(22.0)).child("Resume session"))
                    .child(
                        div()
                            .id("resume-close")
                            .debug_selector(|| "resume-close".into())
                            .cursor_pointer()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .hover(|el| el.bg(theme.TOOL_BG))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_resume(window, cx)),
                            )
                            .child("Back · Esc"),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "resume-search".into())
                    .child(search),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .children(
                        [
                            (Filter::All, "All", "resume-all"),
                            (Filter::Active, "Active", "resume-active"),
                            (Filter::Saved, "Saved", "resume-saved"),
                            (Filter::Crashed, "Crashed", "resume-crashed"),
                        ]
                        .into_iter()
                        .map(|(value, label, id)| {
                            div()
                                .id(id)
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .when(value == filter, |el| el.bg(theme.TOOL_BG))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_resume_filter(value, cx)
                                }))
                                .child(label)
                        }),
                    )
                    .child(div().text_color(theme.TEXT_DIM).child(if content_pending {
                        format!("{} sessions · searching conversations…", matches.len())
                    } else if working_count > 0 {
                        format!("{} sessions · {working_count} working", matches.len())
                    } else {
                        format!("{} sessions", matches.len())
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.0))
                    .child(div().text_color(theme.TEXT_DIM).child("Machine"))
                    .children(
                        [
                            (Machine::All, "All", "resume-machine-all"),
                            (Machine::Wsl, "WSL", "resume-machine-wsl"),
                            (Machine::Local, local_machine_label(), "resume-machine-local"),
                        ]
                        .into_iter()
                        .map(|(value, label, id)| {
                            div()
                                .id(id)
                                .debug_selector(move || id.into())
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .when(value == machine_filter, |el| el.bg(theme.TOOL_BG))
                                .hover(|el| el.bg(theme.TOOL_BG))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_resume_machine(value, cx)
                                }))
                                .child(label)
                        }),
                    )
                    .child(div().text_color(theme.TEXT_DIM).ml_2().child("Repo"))
                    .child(
                        div()
                            .id("resume-repo")
                            .debug_selector(|| "resume-repo".into())
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(theme.TOOL_BG)
                            .max_w(px(220.0))
                            .text_ellipsis()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(state) = this.resume.as_mut() {
                                    state.repo_menu = !state.repo_menu;
                                }
                                cx.notify();
                            }))
                            .child(format!(
                                "{} ▾",
                                repo_filter.clone().unwrap_or_else(|| "any".into())
                            )),
                    )
                    .child(
                        div()
                            .id("resume-group-toggle")
                            .debug_selector(|| "resume-group-toggle".into())
                            .ml_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .when(group_by_repo, |el| el.bg(theme.TOOL_BG))
                            .hover(|el| el.bg(theme.TOOL_BG))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(state) = this.resume.as_mut() {
                                    state.group_by_repo = !state.group_by_repo;
                                }
                                cx.notify();
                            }))
                            .child(if group_by_repo { "☑ Group by repo" } else { "☐ Group by repo" }),
                    ),
            )
            .when(repo_menu, |el| {
                el.child(
                    div()
                        .id("resume-repo-menu")
                        .debug_selector(|| "resume-repo-menu".into())
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .p_2()
                        .rounded_md()
                        .bg(theme.TOOL_BG)
                        .text_size(px(12.0))
                        .max_h(px(140.0))
                        .overflow_y_scroll()
                        .child(
                            div()
                                .id("resume-repo-any")
                                .debug_selector(|| "resume-repo-any".into())
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|el| el.bg(theme.PANEL_BG))
                                .on_click(cx.listener(|this, _, _, cx| this.set_resume_repo(None, cx)))
                                .child("any"),
                        )
                        .children(repos.into_iter().enumerate().map(|(i, repo)| {
                            let label = repo.clone();
                            div()
                                .id(("resume-repo-option", i))
                                .debug_selector(move || format!("resume-repo-option-{i}"))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .when(repo_filter.as_ref() == Some(&repo), |el| el.bg(theme.PANEL_BG))
                                .hover(|el| el.bg(theme.PANEL_BG))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_resume_repo(Some(repo.clone()), cx)
                                }))
                                .child(label)
                        })),
                )
            })
            .child(
                div()
                    .flex()
                    .gap_4()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .w(relative(0.4))
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(rows),
                    )
                    .child(preview),
            )
            .child(div().text_size(px(11.0)).text_color(theme.TEXT_DIM).child(
                "↑ ↓ Select   Page Up / Down   Enter / double-click Resume   Ctrl+Enter New panel   Esc Back   Ctrl+1/2/3/4 Filters",
            ))
    }

    fn load_resume_preview(&mut self, selected: Option<&str>, cx: &mut Context<Self>) {
        let Some(id) = selected else { return };
        let state = self.resume.as_mut().expect("resume picker is open");
        if state.preview_id.as_deref() == Some(id) {
            return;
        }
        state.preview_id = Some(id.to_owned());
        state.preview = None;
        state.preview_full = false;
        state.peek_pending = None;
        state.preview_task = None;
        self.load_resume_meta(id, cx);
        if harness::remote_host(id).is_some() && !harness::screenshot_mode() {
            self.peek_resume_preview(id.to_owned(), PEEK_PREVIEW, cx);
            return;
        }
        let state = self.resume.as_mut().expect("resume picker is open");
        let id = id.to_owned();
        let requested_id = id.clone();
        let request = cx
            .background_executor()
            .spawn(async move { load_preview_history(&id) });
        // Replacing this task cancels the previous selection's UI delivery.
        state.preview_task = Some(cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |this, cx| {
                let current = this
                    .resume
                    .as_ref()
                    .is_some_and(|state| state.preview_id.as_ref() == Some(&requested_id));
                if !current {
                    return;
                }
                let preview = result.map(|(messages, images)| {
                    preview_panel(requested_id.clone(), messages, images, cx)
                });
                if let Some(state) = this.resume.as_mut() {
                    state.preview = Some(preview);
                }
                cx.notify();
            });
        }));
    }

    /// Remote records live on the other machine: ask the bridge for the
    /// stored tail without attaching (`Command::PeekRemote`). The reply
    /// arrives as `Update::ProvisionalHistory`, see [`Self::resume_peek_reply`].
    fn peek_resume_preview(&mut self, id: String, limit: u32, cx: &mut Context<Self>) {
        let Some(state) = self.resume.as_mut() else { return };
        state.peek_pending = Some(id.clone());
        self.bridge.send(Command::PeekRemote {
            session_id: id.clone(),
            limit,
        });
        // The bridge stays silent when a peek fails. Do not spin forever.
        state.preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PEEK_TIMEOUT).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(state) = this.resume.as_mut()
                    && state.peek_pending.as_ref() == Some(&id)
                {
                    state.peek_pending = None;
                    if state.preview.is_none() {
                        state.preview = Some(Err(
                            "Preview unavailable: the remote session did not answer. Resume to load it."
                                .into(),
                        ));
                    }
                    cx.notify();
                }
            });
        }));
    }

    /// Feed a remote peek reply into the browser preview. Returns whether the
    /// reply was the browser's, so it is not also offered to panels.
    pub(super) fn resume_peek_reply(
        &mut self,
        session_id: &str,
        messages: &[jcode_sdk::HistoryMessage],
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(state) = self.resume.as_mut() else {
            return false;
        };
        if state.peek_pending.as_deref() != Some(session_id)
            || state.preview_id.as_deref() != Some(session_id)
        {
            return false;
        }
        state.peek_pending = None;
        state.preview_task = None;
        let panel = preview_panel(session_id.to_owned(), messages.to_vec(), Vec::new(), cx);
        if let Some(state) = self.resume.as_mut() {
            state.preview = Some(Ok(panel));
        }
        cx.notify();
        true
    }

    fn load_full_resume_preview(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.resume.as_mut() else { return };
        let Some(id) = state.preview_id.clone() else { return };
        state.preview_full = true;
        state.preview = None;
        self.peek_resume_preview(id, PEEK_FULL, cx);
        cx.notify();
    }

    /// Read the stored record's details off the UI thread, once per session
    /// version (the harness also caches by `updated_at`).
    fn load_resume_meta(&mut self, id: &str, cx: &mut Context<Self>) {
        let updated = self
            .sessions
            .iter()
            .find(|s| s.session_id == id)
            .and_then(|s| s.updated_at_ms);
        let Some(state) = self.resume.as_mut() else { return };
        if state.meta.contains_key(id) || cfg!(test) || harness::screenshot_mode() {
            return;
        }
        let id = id.to_owned();
        let key = id.clone();
        let request = cx
            .background_executor()
            .spawn(async move { jcode_desktop_harness::session_meta::load(&key, updated) });
        state.meta_task = Some(cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(state) = this.resume.as_mut() {
                    state.meta.insert(id, result);
                    cx.notify();
                }
            });
        }));
    }

    /// Probe git branches for the listed working directories in the
    /// background, one batch per machine. Never blocks rendering.
    fn load_resume_branches(&mut self, matches: &[jcode_sdk::SessionInfo], cx: &mut Context<Self>) {
        let Some(state) = self.resume.as_mut() else { return };
        if state.branch_task.is_some() || cfg!(test) || harness::screenshot_mode() {
            return;
        }
        let mut wanted: HashMap<jcode_desktop_harness::session_git::Location, Vec<String>> =
            HashMap::new();
        for session in matches.iter().take(80) {
            let Some(dir) = session.working_dir.clone().filter(|d| !d.trim().is_empty()) else {
                continue;
            };
            let Some(location) =
                jcode_desktop_harness::session_git::Location::of_session(&session.session_id)
            else {
                continue;
            };
            let key = branch_key(&location, &dir);
            if state.branches.contains_key(&key) {
                continue;
            }
            let dirs = wanted.entry(location).or_default();
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        if wanted.is_empty() {
            return;
        }
        for (location, dirs) in &wanted {
            for dir in dirs {
                // Mark as probing so the next frame does not ask again.
                state.branches.insert(branch_key(location, dir), None);
            }
        }
        let request = cx.background_executor().spawn(async move {
            let mut found = Vec::new();
            for (location, dirs) in wanted {
                let infos = jcode_desktop_harness::session_git::checkout_infos(&location, &dirs);
                for (dir, info) in dirs.into_iter().zip(infos) {
                    found.push((branch_key(&location, &dir), info.map(|info| info.branch)));
                }
            }
            found
        });
        state.branch_task = Some(cx.spawn(async move |this, cx| {
            let found = request.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(state) = this.resume.as_mut() {
                    state.branches.extend(found);
                    state.branch_task = None;
                    cx.notify();
                }
            });
        }));
    }

    /// The info header under the preview title, like the CLI `/session`.
    fn resume_info_lines(&self, session: &jcode_sdk::SessionInfo, now_ms: u64) -> Vec<String> {
        let join = |parts: Vec<Option<String>>| {
            parts.into_iter().flatten().collect::<Vec<_>>().join(" · ")
        };
        let mut lines = Vec::new();
        let (_, machine) = session_machine(&session.session_id);
        lines.push(join(vec![
            session.working_dir.as_deref().map(compact_working_dir),
            self.resume_branch(session).map(|b| format!("⎇ {b}")),
            Some(machine),
        ]));
        let meta = self
            .resume
            .as_ref()
            .and_then(|s| s.meta.get(&session.session_id));
        let meta = match meta {
            None => {
                lines.push("Reading session details…".into());
                None
            }
            Some(Err(error)) => {
                lines.push(error.clone());
                None
            }
            Some(Ok(meta)) => Some(meta),
        };
        let last_active = session
            .last_active_at_ms
            .or(session.updated_at_ms)
            .filter(|ms| *ms > 0)
            .map(|ms| format!("active {}", format_time_ago(ms as u64, now_ms)));
        let created = meta
            .and_then(|m| m.created_at.as_deref())
            .map(|c| format!("created {}", short_timestamp(c)));
        let when = join(vec![created, last_active]);
        if !when.is_empty() {
            lines.push(when);
        }
        if let Some(meta) = meta {
            let model = match (&meta.model, &meta.provider) {
                (Some(model), Some(provider)) => Some(format!("{model} ({provider})")),
                (Some(model), None) => Some(model.clone()),
                (None, provider) => provider.clone(),
            };
            lines.push(join(vec![
                Some(format!(
                    "{} prompts · {} replies",
                    meta.user_messages, meta.assistant_messages
                )),
                model,
                Some(format!("~{}", format_estimated_tokens(meta.estimated_tokens()))),
            ]));
        }
        let saved = session.saved.then(|| match &session.save_label {
            Some(label) => format!("★ saved as {label}"),
            None => "★ saved".into(),
        });
        let swarm = session.agent_label.as_ref().map(|label| match &session.swarm_status {
            Some(status) => format!("swarm {label} ({status})"),
            None => format!("swarm {label}"),
        });
        let agents = self
            .sessions
            .iter()
            .filter(|s| s.parent_session_id.as_deref() == Some(session.session_id.as_str()))
            .count();
        let agents = (agents > 0).then(|| format!("coordinator of {agents} agents"));
        let extra = join(vec![saved, swarm, agents]);
        if !extra.is_empty() {
            lines.push(extra);
        }
        if let Some(prompt) = meta.and_then(|m| m.first_prompt.as_deref()) {
            let prompt: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
            let prompt: String = prompt.chars().take(200).collect();
            lines.push(format!("“{prompt}”"));
        }
        lines.retain(|l| !l.is_empty());
        lines
    }

    fn resume_branch(&self, session: &jcode_sdk::SessionInfo) -> Option<String> {
        let state = self.resume.as_ref()?;
        let dir = session.working_dir.as_deref()?;
        let location =
            jcode_desktop_harness::session_git::Location::of_session(&session.session_id)?;
        state.branches.get(&branch_key(&location, dir))?.clone()
    }

    pub(super) fn init_resume_fixture(&mut self, cx: &App) {
        if !harness::screenshot_mode()
            || std::env::var_os("JCODE_DESKTOP_SCREENSHOT_RESUME_FIXTURE").is_none()
        {
            return;
        }
        let Some(base) = self.resume_sessions(cx).first().cloned() else {
            return;
        };
        for (index, name) in ["alpha", "beta"].into_iter().enumerate() {
            let mut session = base.clone();
            session.session_id = format!("resume-fixture-{name}");
            session.title = Some(format!("Resume acceptance {name}"));
            session.working_dir = Some(format!("/workspace/resume-{name}"));
            session.saved = index == 0;
            session.status = if index == 1 { "running" } else { "idle" }.into();
            session.transcript_bytes = Some(16_800);
            session.updated_at_ms = Some(100 - index as i64);
            self.sessions.push(session);
        }
    }
}

type PreviewHistory = (
    Vec<jcode_sdk::HistoryMessage>,
    Vec<jcode_sdk::RenderedImage>,
);

/// A compact right-aligned tag. Working rows get a tinted pill so they stand
/// out at a glance. Open rows get a quiet label.
fn resume_liveness_badge(
    live: Liveness,
    spinner: Option<gpui::AnyView>,
    index: usize,
) -> Option<gpui::AnyElement> {
    let theme = Theme::global();
    let (label, ink, fill) = match live {
        Liveness::Inactive => return None,
        Liveness::Working => ("Working", theme.OK, Some(gpui::Rgba { a: 0.2, ..theme.OK })),
        Liveness::Open => ("Open", theme.TEXT_DIM, None),
        Liveness::Crashed => (
            "Crashed",
            theme.ERROR,
            Some(gpui::Rgba {
                a: 0.18,
                ..theme.ERROR
            }),
        ),
    };
    Some(
        div()
            .debug_selector(move || format!("resume-row-{index}-{}", label.to_lowercase()))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(5.0))
            .px_2()
            .py(px(1.0))
            .rounded_full()
            .text_size(px(12.0))
            .text_color(ink)
            .when_some(fill, |el, fill| el.bg(fill))
            .when(live == Liveness::Working, |el| match spinner {
                Some(spinner) => el.child(div().flex_none().child(spinner)),
                None => el.child(div().flex_none().size(px(7.0)).rounded_full().bg(ink)),
            })
            .child(label)
            .into_any_element(),
    )
}

fn resume_preview_notice(text: impl Into<gpui::SharedString>) -> gpui::AnyElement {
    div()
        .size_full()
        .p_4()
        .text_size(px(13.0))
        .text_color(Theme::global().TEXT_DIM)
        .child(text.into())
        .into_any_element()
}

/// Build the ordinary chat panel over an inert bridge. It never sends commands
/// or connects its composer, and renders only the transcript.
fn preview_panel(
    id: String,
    messages: Vec<jcode_sdk::HistoryMessage>,
    images: Vec<jcode_sdk::RenderedImage>,
    cx: &mut App,
) -> Entity<Panel> {
    cx.new(|cx| {
        let mut panel = Panel::new(
            format!("resume-preview://{id}"),
            None,
            None,
            crate::harness::spawn_inert(),
            cx,
        );
        panel.transcript_only = true;
        panel.load_history(messages, images, cx);
        panel
    })
}

/// Read only: never attach/watch a runtime just to browse its conversation.
fn load_preview_history(id: &str) -> Result<PreviewHistory, String> {
    if harness::screenshot_mode()
        && std::env::var_os("JCODE_DESKTOP_SCREENSHOT_RESUME_FIXTURE").is_some()
        && id.starts_with("resume-fixture-")
    {
        let message = |role: &str, content: String| jcode_sdk::HistoryMessage {
            response_stats: None,
            role: role.into(),
            content,
        };
        let name = if id.ends_with("alpha") {
            "Alpha"
        } else {
            "Beta"
        };
        return Ok((
            vec![
                message("user", "Please recover the standalone conversation.".into()),
                message(
                    "assistant",
                    format!(
                        "**{name}** conversation preview\n\n- rendered with the chat panel\n- `markdown` included"
                    ),
                ),
            ],
            Vec::new(),
        ));
    }
    // Reject path components and remote IDs rather than reading outside local storage.
    if id.is_empty() || id.contains(['/', '\\']) || id == "." || id == ".." {
        return Err(
            "Preview unavailable for this session. Resume to load its conversation.".into(),
        );
    }
    crate::persisted_history::load(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::session_info;

    fn roles_and_text(history: &PreviewHistory) -> Vec<(String, String)> {
        history
            .0
            .iter()
            .map(|message| (message.role.clone(), message.content.clone()))
            .collect()
    }

    #[test]
    fn resume_preview_uses_full_chat_history_shape() {
        use jcode_base::message::{ContentBlock, Role};
        let mut session =
            jcode_base::session::Session::create_with_id("preview-test".into(), None, None);
        for index in 0..24 {
            session.add_message(
                if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                vec![ContentBlock::Text {
                    text: format!("Actual message {index}"),
                    cache_control: None,
                }],
            );
        }
        let history = crate::persisted_history::render(&session);
        // Identical to the server History payload: no truncation or clipping.
        let expected: Vec<_> = jcode_base::session::render_messages(&session)
            .into_iter()
            .map(|message| (message.role, message.content))
            .collect();
        assert_eq!(roles_and_text(&history), expected);
        assert_eq!(history.0.len(), 24);
    }

    #[gpui::test]
    fn resume_preview_renders_through_the_chat_panel_without_composer(
        cx: &mut gpui::TestAppContext,
    ) {
        let message = |role: &str, content: &str| jcode_sdk::HistoryMessage {
            response_stats: None,
            role: role.into(),
            content: content.into(),
        };
        let (host, vcx) = cx.add_window_view(|_, cx| {
            let panel = preview_panel(
                "abc".into(),
                vec![
                    message("user", "question"),
                    message("assistant", "**answer**"),
                ],
                Vec::new(),
                cx,
            );
            PanelHost(panel)
        });
        vcx.run_until_parked();
        host.read_with(vcx, |host, cx| {
            let panel = host.0.read(cx);
            assert!(panel.transcript_only);
            assert!(panel.history_loaded());
            assert!(matches!(&panel.items[..], [crate::panel::Item::User(q), crate::panel::Item::Assistant(a)]
                if q == "question" && a == "**answer**"));
        });
        assert!(vcx.debug_bounds("panel-meta").is_none());
        assert!(vcx.debug_bounds("fresh-session").is_none());
    }

    struct PanelHost(Entity<Panel>);
    impl Render for PanelHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    #[test]
    fn resume_preview_loads_persisted_messages_without_modifying_storage() {
        use jcode_base::message::{ContentBlock, Role};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preview.json");
        let mut session =
            jcode_base::session::Session::create_with_id("persisted-preview".into(), None, None);
        session.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "Persisted user question".into(),
                cache_control: None,
            }],
        );
        session.add_message(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: "Persisted assistant answer".into(),
                cache_control: None,
            }],
        );
        let bytes = serde_json::to_vec(&session).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            roles_and_text(&crate::persisted_history::load_path(&path).unwrap()),
            vec![
                ("user".into(), "Persisted user question".into()),
                ("assistant".into(), "Persisted assistant answer".into()),
            ]
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert!(
            crate::persisted_history::load_path(&directory.path().join("missing.json")).is_err()
        );
        std::fs::write(&path, b"malformed").unwrap();
        assert!(crate::persisted_history::load_path(&path).is_err());
    }

    #[test]
    fn resume_preview_rejects_nonlocal_paths() {
        for id in [
            "",
            "../secret",
            "/etc/passwd",
            "remote://host/session",
            "..",
            "foo\\bar",
        ] {
            assert!(load_preview_history(id).is_err(), "{id}");
        }
    }

    #[test]
    fn resume_search_matches_save_label() {
        let mut labelled = session_info("session_labelled", Some("Fundraising catch-up"));
        labelled.saved = true;
        labelled.save_label = Some("Investor Catch Up Work".into());
        let other = session_info("session_other", Some("Other"));
        let matches = filtered_sessions(
            vec![labelled, other],
            "investor work",
            Filter::All,
            &HashSet::new(),
            &Default::default(),
        );
        assert_eq!(
            matches
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["session_labelled"]
        );
    }

    #[test]
    fn resume_search_is_case_insensitive_multiword_and_handles_unicode() {
        let mut alpha = session_info("alpha-123", Some("Résumé planner"));
        alpha.working_dir = Some("/workspace/Client".into());
        let beta = session_info("beta", Some("Other task"));
        let sessions = vec![alpha.clone(), beta];
        for query in ["  RÉSUMÉ   client ", "alpha-123", "PLANNER"] {
            assert_eq!(
                filtered_sessions(
                    sessions.clone(),
                    query,
                    Filter::All,
                    &HashSet::new(),
                    &Default::default()
                ),
                vec![alpha.clone()]
            );
        }
        assert!(
            filtered_sessions(
                sessions,
                "not-found",
                Filter::All,
                &HashSet::new(),
                &Default::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn resume_filters_sort_saved_then_recent_exclude_archived_and_deduplicate() {
        let mut saved = session_info("saved", Some("Saved"));
        saved.saved = true;
        saved.updated_at_ms = Some(1);
        let mut running = session_info("running", None);
        running.status = "running".into();
        running.updated_at_ms = Some(20);
        let mut duplicate = running.clone();
        duplicate.updated_at_ms = Some(2);
        let mut archived = session_info("archived", None);
        archived.archived = true;
        let sessions = vec![duplicate, saved.clone(), archived, running.clone()];
        assert_eq!(
            filtered_sessions(
                sessions.clone(),
                "",
                Filter::All,
                &HashSet::new(),
                &Default::default()
            ),
            vec![saved.clone(), running.clone()]
        );
        assert_eq!(
            filtered_sessions(
                sessions.clone(),
                "",
                Filter::Active,
                &HashSet::new(),
                &Default::default()
            ),
            vec![running]
        );
        assert_eq!(
            filtered_sessions(
                sessions,
                "",
                Filter::Saved,
                &HashSet::new(),
                &Default::default()
            ),
            vec![saved]
        );
        assert!(is_resume_command(" /session \n"));
        assert!(is_resume_command("/resume"));
        assert!(is_resume_command("/sessions"));
        assert!(!is_resume_command("/resumeall"));
        assert!(!is_resume_command("/resume unrelated"));
    }

    #[test]
    fn resume_active_filter_includes_daemon_streaming_sessions() {
        let idle = session_info("idle", None);
        let streaming = session_info("streaming", None);
        let working = HashSet::from(["streaming".to_string()]);
        assert_eq!(
            filtered_sessions(
                vec![idle, streaming.clone()],
                "",
                Filter::Active,
                &working,
                &Default::default()
            ),
            vec![streaming]
        );
    }

    #[test]
    fn resume_search_includes_transcript_hits_after_metadata_hits() {
        let mut titled = session_info("titled", Some("Kubernetes rollout"));
        titled.updated_at_ms = Some(1);
        let mut transcript = session_info("transcript", Some("Unrelated title"));
        transcript.updated_at_ms = Some(50);
        let other = session_info("other", Some("Other"));
        let content = content_search::Hits::from([
            (
                "transcript".to_string(),
                "…fix the kubernetes rollout…".to_string(),
            ),
            ("titled".to_string(), "…kubernetes…".to_string()),
        ]);
        let ids = |content: &content_search::Hits| {
            filtered_sessions(
                vec![other.clone(), transcript.clone(), titled.clone()],
                "kubernetes",
                Filter::All,
                &HashSet::new(),
                content,
            )
            .into_iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>()
        };
        assert_eq!(ids(&content), vec!["titled", "transcript"]);
        assert_eq!(ids(&Default::default()), vec!["titled"]);
    }

    #[gpui::test]
    fn resume_rows_mark_working_and_open_sessions(cx: &mut gpui::TestAppContext) {
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut w = Workspace::for_test(learning::Coach::new(), cx);
            w.single_panel = true;
            w.open_startup_draft(cx);
            let mut busy = session_info("busy", Some("Busy"));
            busy.updated_at_ms = Some(30);
            let mut quiet = session_info("quiet", Some("Quiet"));
            quiet.updated_at_ms = Some(20);
            let mut remote = session_info("remote", Some("Remote"));
            remote.updated_at_ms = Some(10);
            w.sessions = vec![busy, quiet, remote];
            w
        });
        workspace.update_in(vcx, |w, window, cx| w.open_resume(&OpenResume, window, cx));
        // Let the first presence poll land before injecting daemon state.
        vcx.run_until_parked();
        workspace.update(vcx, |w, cx| {
            w.resume.as_mut().unwrap().streaming = HashSet::from(["remote".to_string()]);
            cx.notify();
        });
        vcx.run_until_parked();
        let rows = workspace.read_with(vcx, |w, cx| {
            w.resume_matches(cx)
                .into_iter()
                .map(|s| s.session_id)
                .collect::<Vec<_>>()
        });
        let remote = rows.iter().position(|id| id == "remote").unwrap();
        let quiet = rows.iter().position(|id| id == "quiet").unwrap();
        assert!(
            vcx.debug_bounds(format!("resume-row-{remote}-working").leak())
                .is_some()
        );
        assert!(
            vcx.debug_bounds(format!("resume-row-{quiet}-working").leak())
                .is_none()
        );
        assert!(
            vcx.debug_bounds(format!("resume-row-{quiet}-open").leak())
                .is_none()
        );
        // The local startup draft is open in this window but idle.
        let draft = rows
            .iter()
            .position(|id| !["busy", "quiet", "remote"].contains(&id.as_str()));
        if let Some(draft) = draft {
            assert!(
                vcx.debug_bounds(format!("resume-row-{draft}-open").leak())
                    .is_some()
            );
        }
    }

    fn reload_picker(w: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let snapshot = w.snapshot_for_reload(window, cx).unwrap();
        let snapshot = WorkspaceSnapshot::decode(&snapshot.encode().unwrap()).unwrap();
        let picker = snapshot.resume.clone();
        w.resume = None;
        w.apply_snapshot(snapshot, cx);
        w.restore_resume(Some(picker), true, window, cx);
        w.restore_focus(window, cx);
    }

    #[gpui::test]
    fn resume_reload_roundtrip_preserves_search_selection_and_suppresses_connected(
        cx: &mut gpui::TestAppContext,
    ) {
        let (bridge, commands) = harness::spawn_recording();
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut w = Workspace::for_test(learning::Coach::new(), cx);
            w.bridge = bridge;
            w.open_startup_draft(cx);
            w
        });
        workspace.update_in(vcx, |w, window, cx| {
            w.open_resume(&OpenResume, window, cx);
            let state = w.resume.as_mut().unwrap();
            state.search.update(cx, |input, cx| {
                let mut search = input.snapshot();
                search.content = "Résumé planner".into();
                search.selection_start = 2;
                search.selection_end = 5;
                search.selection_reversed = true;
                input.restore(search, cx);
            });
            state.query = "Résumé planner".into();
            state.selected = Some("selected-session".into());
            state.filter = Filter::Saved;
            state.start_on_close = true;
            let expected = w.resume_snapshot(cx);
            reload_picker(w, window, cx);
            assert_eq!(w.resume_snapshot(cx), expected);
            assert!(
                w.resume
                    .as_ref()
                    .unwrap()
                    .search
                    .focus_handle(cx)
                    .is_focused(window)
            );
            w.apply(harness::Update::Connected, cx);
            assert_eq!(w.resume_snapshot(cx), expected);
            assert!(
                commands.try_recv().is_err(),
                "reload must not create or attach a session"
            );
        });
        vcx.run_until_parked();
        workspace.read_with(vcx, |w, _| {
            assert_eq!(
                w.resume.as_ref().unwrap().selected.as_deref(),
                Some("selected-session")
            );
        });
        assert!(commands.try_recv().is_err());
    }

    #[gpui::test]
    fn resume_reload_cancelled_picker_stays_closed(cx: &mut gpui::TestAppContext) {
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut w = Workspace::for_test(learning::Coach::new(), cx);
            w.open_startup_draft(cx);
            w
        });
        workspace.update_in(vcx, |w, window, cx| {
            w.restore_resume(None, true, window, cx);
            w.close_resume(window, cx);
            assert!(w.slots[w.active].panel.read(cx).is_startup_draft());
            assert_eq!(w.resume_snapshot(cx), Snapshot::Closed);
            reload_picker(w, window, cx);
            assert!(
                w.resume.is_none(),
                "--resume must not reopen an intentionally closed picker"
            );
        });
    }

    #[gpui::test]
    fn resume_reload_selected_picker_stays_closed(cx: &mut gpui::TestAppContext) {
        let (bridge, commands) = harness::spawn_recording();
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut w = Workspace::for_test(learning::Coach::new(), cx);
            w.bridge = bridge;
            w.open_startup_draft(cx);
            w.sessions = vec![session_info("target", Some("Target"))];
            w
        });
        workspace.update_in(vcx, |w, window, cx| {
            w.restore_resume(None, true, window, cx);
            w.resume.as_mut().unwrap().selected = Some("target".into());
            w.resume_selected(window, cx);
            reload_picker(w, window, cx);
            assert!(w.resume.is_none());
            assert_eq!(w.slots[w.active].panel.read(cx).session_id, "target");
            while commands.try_recv().is_ok() {}
            w.apply(harness::Update::Connected, cx);
            assert!(
                commands.try_recv().is_err(),
                "must not start the hidden startup draft"
            );
        });
    }

    #[gpui::test]
    fn resume_reload_legacy_recovers_only_requested_active_startup(cx: &mut gpui::TestAppContext) {
        let (bridge, commands) = harness::spawn_recording();
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut w = Workspace::for_test(learning::Coach::new(), cx);
            w.bridge = bridge;
            w.open_startup_draft(cx);
            w
        });
        workspace.update_in(vcx, |w, window, cx| {
            let mut json =
                serde_json::to_value(w.snapshot_for_reload(window, cx).unwrap()).unwrap();
            json.as_object_mut().unwrap().remove("resume");
            let legacy = WorkspaceSnapshot::decode(&serde_json::to_vec(&json).unwrap()).unwrap();
            assert_eq!(legacy.resume, Snapshot::Legacy);
            w.apply_snapshot(legacy, cx);
            w.restore_resume(Some(Snapshot::Legacy), false, window, cx);
            assert!(w.resume.is_none());
            w.restore_resume(Some(Snapshot::Legacy), true, window, cx);
            assert!(w.resume.as_ref().unwrap().start_on_close);
            assert!(
                w.resume
                    .as_ref()
                    .unwrap()
                    .search
                    .focus_handle(cx)
                    .is_focused(window)
            );
            w.apply(harness::Update::Connected, cx);
            assert!(commands.try_recv().is_err());
            w.sessions = vec![session_info("target", Some("Target"))];
            w.resume.as_mut().unwrap().selected = Some("target".into());
            w.resume_selected(window, cx);
            w.restore_resume(Some(Snapshot::Legacy), true, window, cx);
            assert!(
                w.resume.is_none(),
                "legacy --resume must not replace an active chat"
            );
        });
    }

    #[gpui::test]
    fn resume_startup_connected_does_not_create_until_cancel(cx: &mut gpui::TestAppContext) {
        let (bridge, commands) = harness::spawn_recording();
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut w = Workspace::for_test(learning::Coach::new(), cx);
            w.single_panel = true;
            w.bridge = bridge;
            w.open_startup_draft(cx);
            w
        });
        workspace.update_in(vcx, |w, window, cx| {
            w.open_resume(&OpenResume, window, cx);
            w.apply(harness::Update::Connected, cx);
            assert!(
                commands.try_recv().is_err(),
                "browsing must not create or attach a session"
            );
            w.close_resume(window, cx);
            assert!(
                commands.try_recv().is_ok(),
                "cancel starts the normal draft"
            );
        });
    }

    #[gpui::test]
    fn resume_selection_before_connection_does_not_create_hidden_startup_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        let (bridge, commands) = harness::spawn_recording();
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut w = Workspace::for_test(learning::Coach::new(), cx);
            w.single_panel = true;
            w.bridge = bridge;
            w.open_startup_draft(cx);
            w.sessions = vec![session_info("target", Some("Existing conversation"))];
            w
        });
        workspace.update_in(vcx, |w, window, cx| {
            w.open_resume(&OpenResume, window, cx);
            w.resume.as_mut().unwrap().selected = Some("target".into());
            w.resume_selected(window, cx);
            while commands.try_recv().is_ok() {}
            w.apply(harness::Update::Connected, cx);
            assert!(
                commands.try_recv().is_err(),
                "late connection must not create hidden startup draft"
            );
        });
    }

    #[gpui::test]
    fn resume_cancel_preserves_draft_and_never_watches_a_picker(cx: &mut gpui::TestAppContext) {
        let (bridge, commands) = harness::spawn_recording();
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
            workspace.single_panel = true;
            workspace.bridge = bridge;
            workspace.open_startup_draft(cx);
            workspace
        });
        workspace.update_in(vcx, |w, window, cx| {
            let input = w.slots[0].panel.read(cx).input.clone();
            input.update(cx, |input, cx| {
                let mut draft = input.snapshot();
                draft.content = "keep this composer draft".into();
                input.restore(draft, cx);
            });
            w.open_resume(&OpenResume, window, cx);
        });
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("resume-panel").is_some());
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        workspace.read_with(vcx, |w, cx| {
            assert!(w.resume.is_none());
            assert_eq!(w.slots.len(), 1);
            assert_eq!(
                w.slots[0].panel.read(cx).input.read(cx).snapshot().content,
                "keep this composer draft"
            );
        });
        assert!(
            commands.try_recv().is_err(),
            "picker must not Watch/Unwatch a runtime session"
        );
    }

    #[gpui::test]
    fn resume_single_panel_reuses_entities_preserves_drafts_and_primary_anchor(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut w = Workspace::for_test(learning::Coach::new(), cx);
            w.single_panel = true;
            w.open_startup_draft(cx);
            w.sessions = vec![session_info("target", Some("Resumed target"))];
            w
        });
        let original = workspace.read_with(vcx, |w, _| w.slots[0].panel.clone());
        workspace.update_in(vcx, |w, window, cx| {
            w.open_resume(&OpenResume, window, cx);
            w.resume.as_mut().unwrap().selected = Some("target".into());
            w.resume_selected(window, cx);
            assert!(w.single_panel);
            assert_eq!(w.active, 0);
            assert_eq!(w.slots[0].panel.read(cx).session_id, "target");
            assert_eq!(w.slots.len(), 2);
            assert!(
                w.resume_sessions(cx)
                    .iter()
                    .any(|s| s.session_id == original.read(cx).session_id)
            );
            let target = w.slots[0].panel.clone();
            w.open_resume(&OpenResume, window, cx);
            w.resume.as_mut().unwrap().selected = Some("target".into());
            w.resume_selected(window, cx);
            assert_eq!(w.slots[0].panel, target);
            assert_eq!(w.slots.len(), 2);
            w.open_resume(&OpenResume, window, cx);
            w.resume.as_mut().unwrap().selected = Some(original.read(cx).session_id.clone());
            w.resume_selected(window, cx);
            assert_eq!(w.slots[0].panel, original);
        });
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("single-panel-root").is_some());
        assert!(vcx.debug_bounds("single-panel-back").is_none());
        assert!(vcx.debug_bounds("sidebar").is_none());
        assert!(vcx.debug_bounds("workspace-canvas").is_none());
    }

    #[gpui::test]
    fn resume_keyboard_navigation_search_empty_and_catalog_refresh_are_safe(
        cx: &mut gpui::TestAppContext,
    ) {
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut w = Workspace::for_test(learning::Coach::new(), cx);
            w.single_panel = true;
            w.open_startup_draft(cx);
            w.sessions = vec![
                session_info("a", Some("Alpha")),
                session_info("b", Some("Beta")),
            ];
            w
        });
        workspace.update_in(vcx, |w, window, cx| w.open_resume(&OpenResume, window, cx));
        vcx.run_until_parked();
        vcx.simulate_keystrokes("down");
        workspace.read_with(vcx, |w, _| {
            assert_eq!(w.resume.as_ref().unwrap().selected.as_deref(), Some("b"))
        });
        vcx.simulate_keystrokes("up");
        workspace.read_with(vcx, |w, _| {
            assert_eq!(w.resume.as_ref().unwrap().selected.as_deref(), Some("a"))
        });
        vcx.simulate_input("no such session");
        vcx.run_until_parked();
        vcx.simulate_keystrokes("pagedown enter");
        workspace.read_with(vcx, |w, _| assert!(w.resume.is_some()));
        vcx.simulate_keystrokes("ctrl-a");
        vcx.simulate_input("Beta");
        vcx.run_until_parked();
        workspace.update(vcx, |w, cx| {
            w.sessions.clear();
            cx.notify();
        });
        vcx.run_until_parked();
        vcx.simulate_keystrokes("enter");
        workspace.read_with(vcx, |w, _| {
            assert!(
                w.resume.is_some(),
                "stale selection must not resume removed session"
            )
        });
    }
    #[gpui::test]
    fn resume_aliases_open_locally_from_startup_in_both_modes(cx: &mut gpui::TestAppContext) {
        for single_panel in [false, true] {
            let (workspace, vcx) = cx.add_window_view(|_, cx| {
                let mut w = Workspace::for_test(learning::Coach::new(), cx);
                w.single_panel = single_panel;
                w.open_startup_draft(cx);
                w
            });
            for command in ["/resume", "/sessions", "/session"] {
                workspace.update_in(vcx, |w, window, cx| w.focus_active(window, cx));
                vcx.run_until_parked();
                vcx.simulate_input(command);
                vcx.simulate_keystrokes("enter");
                vcx.run_until_parked();
                workspace.read_with(vcx, |w, _| {
                    assert!(w.resume.is_some(), "{command} should open locally");
                    assert_eq!(w.single_panel, single_panel);
                });
                vcx.simulate_keystrokes("escape");
                vcx.run_until_parked();
            }
        }
    }

    fn watched(commands: &std::sync::mpsc::Receiver<harness::Command>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let harness::Command::Watch { session_id } = command {
                out.push(session_id);
            }
        }
        out
    }

    fn browser_with(
        cx: &mut gpui::TestAppContext,
        sessions: Vec<jcode_sdk::SessionInfo>,
    ) -> (
        Entity<Workspace>,
        &mut gpui::VisualTestContext,
        std::sync::mpsc::Receiver<harness::Command>,
    ) {
        let (bridge, commands) = harness::spawn_recording();
        let (workspace, vcx) = cx.add_window_view(|_, cx| {
            let mut w = Workspace::for_test(learning::Coach::new(), cx);
            w.bridge = bridge;
            w.open_startup_draft(cx);
            w.sessions = sessions;
            w
        });
        workspace.update_in(vcx, |w, window, cx| w.open_resume(&OpenResume, window, cx));
        vcx.run_until_parked();
        (workspace, vcx, commands)
    }

    fn click_row(vcx: &mut gpui::VisualTestContext, row: usize, count: usize) {
        let bounds = vcx
            .debug_bounds(Box::leak(format!("resume-row-{row}").into_boxed_str()))
            .expect("row is rendered");
        let position = bounds.center();
        vcx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position,
            click_count: count,
            modifiers: Default::default(),
            first_mouse: false,
        });
        vcx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position,
            click_count: count,
            modifiers: Default::default(),
        });
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn browser_single_click_selects_without_opening(cx: &mut gpui::TestAppContext) {
        let (workspace, vcx, commands) = browser_with(
            cx,
            vec![session_info("first", Some("First")), session_info("second", Some("Second"))],
        );
        while commands.try_recv().is_ok() {}
        click_row(vcx, 1, 1);
        workspace.read_with(vcx, |w, _| {
            let state = w.resume.as_ref().expect("browser stays open");
            assert_eq!(state.selected.as_deref(), Some("second"));
            assert_eq!(state.preview_id.as_deref(), Some("second"));
        });
        assert!(watched(&commands).is_empty(), "single click must not open");
        assert!(vcx.debug_bounds("resume-open").is_some());
    }

    #[gpui::test]
    fn browser_double_click_opens(cx: &mut gpui::TestAppContext) {
        let (workspace, vcx, _commands) = browser_with(
            cx,
            vec![session_info("first", Some("First")), session_info("second", Some("Second"))],
        );
        click_row(vcx, 1, 1);
        click_row(vcx, 1, 2);
        workspace.read_with(vcx, |w, cx| {
            assert!(w.resume.is_none());
            assert_eq!(w.slots[w.active].panel.read(cx).session_id, "second");
        });
    }

    #[gpui::test]
    fn browser_enter_and_resume_button_open(cx: &mut gpui::TestAppContext) {
        let (workspace, vcx, _commands) = browser_with(
            cx,
            vec![session_info("first", Some("First")), session_info("second", Some("Second"))],
        );
        vcx.simulate_keystrokes("down enter");
        vcx.run_until_parked();
        workspace.read_with(vcx, |w, cx| {
            assert!(w.resume.is_none());
            assert_eq!(w.slots[w.active].panel.read(cx).session_id, "second");
        });
        workspace.update_in(vcx, |w, window, cx| w.open_resume(&OpenResume, window, cx));
        vcx.run_until_parked();
        let button = vcx.debug_bounds("resume-open").expect("resume button");
        vcx.simulate_click(button.center(), gpui::Modifiers::default());
        vcx.run_until_parked();
        workspace.read_with(vcx, |w, _| assert!(w.resume.is_none()));
    }

    #[gpui::test]
    fn browser_wsl_preview_uses_peek_remote(cx: &mut gpui::TestAppContext) {
        let id = "ssh://wsl/session_fox_1";
        let mut wsl = session_info(id, Some("Fox"));
        wsl.working_dir = Some("/home/v/go-chathub".into());
        let (workspace, vcx, commands) = browser_with(cx, vec![wsl]);
        let mut peeked = None;
        while let Ok(command) = commands.try_recv() {
            if let harness::Command::PeekRemote { session_id, limit } = command {
                peeked = Some((session_id, limit));
            }
        }
        assert_eq!(peeked, Some((id.to_string(), PEEK_PREVIEW)));
        workspace.update_in(vcx, |w, _, cx| {
            w.apply(
                harness::Update::ProvisionalHistory {
                    session_id: id.into(),
                    messages: vec![
                        jcode_sdk::HistoryMessage {
                            response_stats: None,
                            role: "user".into(),
                            content: "fix the ladder".into(),
                        },
                        jcode_sdk::HistoryMessage {
                            response_stats: None,
                            role: "assistant".into(),
                            content: "done".into(),
                        },
                    ],
                },
                cx,
            );
            let Some(Ok(panel)) = w.resume.as_ref().unwrap().preview.as_ref() else {
                panic!("preview should hold the peeked transcript");
            };
            assert!(!panel.read(cx).items.is_empty());
        });
        vcx.run_until_parked();
        let full = vcx.debug_bounds("resume-load-full").expect("load full button");
        vcx.simulate_click(full.center(), gpui::Modifiers::default());
        vcx.run_until_parked();
        let mut full_limit = None;
        while let Ok(command) = commands.try_recv() {
            if let harness::Command::PeekRemote { limit, .. } = command {
                full_limit = Some(limit);
            }
        }
        assert_eq!(full_limit, Some(PEEK_FULL));
        assert!(watched(&commands).is_empty());
    }

    #[gpui::test]
    fn browser_header_shows_metadata(cx: &mut gpui::TestAppContext) {
        let mut session = session_info("session_fox_1", Some("Fox"));
        session.working_dir = Some("/home/v/go-chathub".into());
        session.saved = true;
        session.save_label = Some("ladder".into());
        let (workspace, vcx, _commands) = browser_with(cx, vec![session]);
        let lines = workspace.update_in(vcx, |w, _, _| {
            let state = w.resume.as_mut().unwrap();
            state.meta.insert(
                "session_fox_1".into(),
                Ok(SessionMeta {
                    created_at: Some("2026-10-23T14:02:00Z".into()),
                    model: Some("claude-opus".into()),
                    provider: Some("claude".into()),
                    user_messages: 38,
                    assistant_messages: 104,
                    first_prompt: Some("Fix the PvP ladder".into()),
                    bytes: 324_000,
                    ..Default::default()
                }),
            );
            let session = w.sessions[0].clone();
            w.resume_info_lines(&session, 0)
        });
        let text = lines.join("\n");
        assert!(text.contains("38 prompts · 104 replies"), "{text}");
        assert!(text.contains("claude-opus (claude)"), "{text}");
        assert!(text.contains("created 2026-10-23 14:02 UTC"), "{text}");
        assert!(text.contains("saved as ladder"), "{text}");
        assert!(text.contains("Fix the PvP ladder"), "{text}");
        vcx.run_until_parked();
        assert!(vcx.debug_bounds("resume-info-0").is_some());
    }

    #[gpui::test]
    fn browser_machine_and_repo_filters(cx: &mut gpui::TestAppContext) {
        let mut a = session_info("ssh://wsl/session_a", Some("A"));
        a.working_dir = Some("/home/v/alpha".into());
        let mut b = session_info("session_b", Some("B"));
        b.working_dir = Some("/home/v/beta".into());
        let mut c = session_info("ssh://wsl/session_c", Some("C"));
        c.working_dir = Some("/home/v/beta".into());
        let (workspace, vcx, _commands) = browser_with(cx, vec![a, b, c]);
        let ids = |w: &Workspace, cx: &App| {
            w.resume_matches(cx)
                .into_iter()
                .map(|s| s.session_id)
                .collect::<Vec<_>>()
        };
        let wsl = vcx.debug_bounds("resume-machine-wsl").expect("machine filter");
        vcx.simulate_click(wsl.center(), gpui::Modifiers::default());
        vcx.run_until_parked();
        workspace.read_with(vcx, |w, cx| {
            assert_eq!(ids(w, cx), vec!["ssh://wsl/session_a", "ssh://wsl/session_c"]);
            assert_eq!(w.resume_repos(cx), vec!["alpha", "beta"]);
        });
        workspace.update_in(vcx, |w, _, cx| w.set_resume_repo(Some("beta".into()), cx));
        workspace.read_with(vcx, |w, cx| {
            assert_eq!(ids(w, cx), vec!["ssh://wsl/session_c"]);
        });
        workspace.update_in(vcx, |w, _, cx| {
            w.set_resume_machine(Machine::All, cx);
            w.resume.as_mut().unwrap().group_by_repo = true;
        });
        vcx.run_until_parked();
        workspace.read_with(vcx, |w, cx| {
            // The open startup draft has no repository, so it groups last.
            assert_eq!(
                ids(w, cx)[..3],
                ["ssh://wsl/session_a", "session_b", "ssh://wsl/session_c"]
            );
            assert_eq!(w.resume.as_ref().unwrap().row_children[..3], [1, 3, 4]);
        });
        assert!(vcx.debug_bounds("resume-group-0").is_some());
    }
}
