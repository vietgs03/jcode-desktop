//! Review pane: the review column docked beside a conversation. One header
//! with tabs (Changes, Agent turns, History) and a scope selector, a file
//! tree on the left and every file diff stacked on the right.
//!
//! All git runs off the UI thread through `session_git::Location`, so WSL
//! checkouts work like local ones. One scoped load is in flight per pane.
use super::*;

use crate::diff_model::{DiffFile, DiffPreview};
use crate::diff_view::AttachHandler;
use crate::review_stack::StackedDiff;
use crate::review_turns::{AgentTurn, agent_turns};
use jcode_desktop_harness::session_git::Location;
use jcode_desktop_harness::session_git_history::{self as history, Commit, Edge, GraphRow};

use super::changes::{ChangesRow, status_color, summary_label, unique_file_count};

const HISTORY_ROW: f32 = 30.;
const LANE_WIDTH: f32 = 12.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Changes,
    Turns,
    History,
}

/// What the Changes tab compares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChangesScope {
    Uncommitted,
    /// Merge base with this branch up to the working tree.
    Branch(String),
}

/// The diff currently loaded into the scoped view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Branch(String),
    Turn(usize),
    Commit(String),
}

pub(crate) struct ScopedView {
    pub scope: Scope,
    pub files: Vec<DiffFile>,
    pub stack: StackedDiff,
    pub truncated: bool,
    pub caption: String,
}

pub(crate) struct PaneState {
    pub tab: Tab,
    pub changes_scope: ChangesScope,
    pub split: bool,
    pub attach: Option<AttachHandler>,
    /// The conversation this review belongs to, for agent turns.
    pub source: Option<gpui::WeakEntity<Panel>>,
    pub branches: Vec<String>,
    pub base: Option<String>,
    pub branch_menu: bool,
    pub scoped: Option<ScopedView>,
    pub scoped_loading: Option<Scope>,
    pub scoped_pending: bool,
    pub scoped_error: Option<String>,
    pub turns: Vec<AgentTurn>,
    pub turn_selected: Option<usize>,
    pub turn_expanded: Option<usize>,
    pub commits: Vec<Commit>,
    pub graph: Vec<GraphRow>,
    pub history_list: ListState,
    pub history_loading: bool,
    pub history_done: bool,
    pub history_error: Option<String>,
    pub commit_selected: Option<usize>,
    /// Refreshes the pane when the source conversation finishes a turn.
    pub source_watch: Option<gpui::Subscription>,
    pub source_busy: bool,
}

impl PaneState {
    pub(crate) fn new() -> Self {
        Self {
            tab: Tab::Changes,
            changes_scope: ChangesScope::Uncommitted,
            split: false,
            attach: None,
            source: None,
            branches: Vec::new(),
            base: None,
            branch_menu: false,
            scoped: None,
            scoped_loading: None,
            scoped_pending: false,
            scoped_error: None,
            turns: Vec::new(),
            turn_selected: None,
            turn_expanded: None,
            commits: Vec::new(),
            graph: Vec::new(),
            history_list: ListState::new(0, ListAlignment::Top, px(HISTORY_ROW)),
            history_loading: false,
            history_done: false,
            history_error: None,
            commit_selected: None,
            source_watch: None,
            source_busy: false,
        }
    }

    /// The scoped diff the visible tab wants, `None` for the live working tree.
    pub(crate) fn wanted_scope(&self) -> Option<Scope> {
        match self.tab {
            Tab::Changes => match &self.changes_scope {
                ChangesScope::Uncommitted => None,
                ChangesScope::Branch(branch) => Some(Scope::Branch(branch.clone())),
            },
            Tab::Turns => self
                .turn_selected
                .and_then(|index| self.turns.get(index))
                .map(|turn| Scope::Turn(turn.number)),
            Tab::History => self
                .commit_selected
                .and_then(|index| self.commits.get(index))
                .map(|commit| Scope::Commit(commit.hash.clone())),
        }
    }
}

fn file_letter(file: &DiffFile) -> char {
    match file.kind.as_str() {
        "Add" | "Added" => 'A',
        "Delete" | "Deleted" => 'D',
        "Rename" | "Renamed" => 'R',
        _ => 'M',
    }
}

fn lane_color(lane: usize) -> gpui::Rgba {
    let theme = Theme::global();
    let palette = [
        theme.ACCENT,
        theme.OK,
        theme.WARN,
        theme.CODE_TYPE,
        theme.CODE_STRING,
        theme.CODE_KEYWORD,
        theme.ERROR,
    ];
    palette[lane % palette.len()]
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub(crate) fn relative_date(time: i64, now: i64) -> String {
    let seconds = (now - time).max(0);
    match seconds {
        0..60 => "just now".into(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        86_400..2_592_000 => format!("{}d ago", seconds / 86_400),
        2_592_000..31_536_000 => format!("{}mo ago", seconds / 2_592_000),
        _ => format!("{}y ago", seconds / 31_536_000),
    }
}

/// `/review` and `/changes` open the pane. `Some(true)` asks for staged.
pub(super) fn review_command(content: &str) -> Option<bool> {
    if content.trim() == "/review" {
        return Some(false);
    }
    super::changes::changes_command(content)
}

impl Panel {
    pub(crate) fn review_pane(&self) -> Option<&PaneState> {
        self.working_tree_review.as_ref().map(|review| &review.pane)
    }

    /// Wire the pane to its conversation: agent turns and Attach to chat.
    pub(crate) fn set_review_source(
        &mut self,
        source: gpui::WeakEntity<Panel>,
        attach: AttachHandler,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        let same = review
            .pane
            .source
            .as_ref()
            .is_some_and(|current| current.entity_id() == source.entity_id());
        review.pane.attach = Some(attach);
        if !same {
            review.pane.source_watch = source.upgrade().map(|entity| {
                cx.observe(&entity, |panel, source, cx| {
                    let busy = source.read(cx).is_busy();
                    let Some(review) = &mut panel.working_tree_review else {
                        return;
                    };
                    let finished = review.pane.source_busy && !busy;
                    review.pane.source_busy = busy;
                    if finished {
                        panel.refresh_review_pane(cx);
                    }
                })
            });
            review.pane.source = Some(source);
        }
        if review.pane.tab == Tab::Turns {
            self.reload_review_turns(cx);
        }
    }

    pub(super) fn load_review_branches(&mut self, cx: &mut Context<Self>) {
        let Some(review) = &self.working_tree_review else {
            return;
        };
        let location = review.location.clone();
        let directory = review.directory.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let branches = history::branches(&location, &directory)?;
                    let current = location
                        .git(&directory, &["rev-parse", "--abbrev-ref", "HEAD"])
                        .ok()
                        .map(|s| s.trim().to_owned());
                    let base = history::pick_base_branch(&branches, current.as_deref());
                    Ok::<_, String>((branches, base))
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if let (Some(review), Ok((branches, base))) =
                    (panel.working_tree_review.as_mut(), result)
                {
                    review.pane.branches = branches;
                    review.pane.base = base;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn set_review_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        review.pane.tab = tab;
        review.pane.branch_menu = false;
        match tab {
            Tab::Changes => {}
            Tab::Turns => self.reload_review_turns(cx),
            Tab::History => {
                if review.pane.commits.is_empty() && !review.pane.history_loading {
                    self.load_history_page(cx);
                }
            }
        }
        self.sync_scoped_view(cx);
        cx.notify();
    }

    pub(crate) fn set_changes_scope(&mut self, scope: ChangesScope, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        review.pane.tab = Tab::Changes;
        review.pane.branch_menu = false;
        let uncommitted = scope == ChangesScope::Uncommitted;
        review.pane.changes_scope = scope;
        if uncommitted {
            self.refresh_working_tree_review(cx);
        }
        self.sync_scoped_view(cx);
        cx.notify();
    }

    fn reload_review_turns(&mut self, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        let turns = review
            .pane
            .source
            .as_ref()
            .and_then(|source| source.upgrade())
            .map(|source| agent_turns(&source.read(cx).items))
            .unwrap_or_default();
        let previous = review
            .pane
            .turn_selected
            .and_then(|index| review.pane.turns.get(index))
            .map(|turn| turn.number);
        let changed = turns != review.pane.turns;
        review.pane.turn_selected = previous
            .and_then(|number| turns.iter().position(|turn| turn.number == number))
            .or_else(|| (!turns.is_empty()).then_some(0));
        if review.pane.turn_expanded.is_none() {
            review.pane.turn_expanded = review.pane.turn_selected;
        }
        review.pane.turns = turns;
        if changed
            && let Some(scoped) = &review.pane.scoped
            && matches!(scoped.scope, Scope::Turn(_))
        {
            review.pane.scoped = None;
        }
        self.sync_scoped_view(cx);
    }

    pub(crate) fn select_review_turn(&mut self, index: usize, file: Option<usize>, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        if index >= review.pane.turns.len() {
            return;
        }
        review.pane.tab = Tab::Turns;
        review.pane.turn_selected = Some(index);
        if file.is_none() {
            review.pane.turn_expanded = if review.pane.turn_expanded == Some(index) {
                None
            } else {
                Some(index)
            };
        }
        self.sync_scoped_view(cx);
        if let Some(file) = file {
            self.select_scoped_file(file, cx);
        }
        cx.notify();
    }

    pub(crate) fn select_review_commit(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        if index >= review.pane.commits.len() {
            return;
        }
        review.pane.tab = Tab::History;
        review.pane.commit_selected = Some(index);
        self.sync_scoped_view(cx);
        cx.notify();
    }

    pub(crate) fn select_scoped_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(scoped) = self
            .working_tree_review
            .as_mut()
            .and_then(|review| review.pane.scoped.as_mut())
        {
            scoped.stack.select(index);
            cx.notify();
        }
    }

    pub(crate) fn set_review_split(&mut self, split: bool, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        review.pane.split = split;
        if let Some(content) = &mut review.content {
            content.set_split(split, cx);
        }
        if let Some(scoped) = &mut review.pane.scoped {
            scoped.stack.set_split(split, cx);
        }
        cx.notify();
    }

    /// Load the scoped diff the visible tab wants, unless it is already
    /// shown or loading. A change while loading reloads once afterwards.
    fn sync_scoped_view(&mut self, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        let Some(scope) = review.pane.wanted_scope() else {
            return;
        };
        if review.pane.scoped.as_ref().is_some_and(|s| s.scope == scope) {
            return;
        }
        if let Some(loading) = &review.pane.scoped_loading {
            if *loading != scope {
                review.pane.scoped_pending = true;
            }
            return;
        }
        review.pane.scoped_error = None;
        let split = review.pane.split;
        let attach = review.pane.attach.clone();
        if let Scope::Turn(number) = scope {
            let Some(turn) = review.pane.turns.iter().find(|t| t.number == number) else {
                return;
            };
            let (added, removed) = turn.counts();
            let caption = format!(
                "Turn {} · {} file{} · +{added} −{removed}",
                turn.number,
                turn.preview.files.len(),
                if turn.preview.files.len() == 1 { "" } else { "s" }
            );
            let files = turn.preview.files.clone();
            let stack = StackedDiff::new(
                turn.preview.clone(),
                Some(format!("Turn {}", turn.number).into()),
                split,
                attach,
                cx,
            );
            review.pane.scoped = Some(ScopedView {
                scope,
                files,
                stack,
                truncated: false,
                caption,
            });
            cx.notify();
            return;
        }
        review.pane.scoped_loading = Some(scope.clone());
        let location = review.location.clone();
        let directory = review.directory.clone();
        let worker_scope = scope.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_scope(&location, &directory, &worker_scope) })
                .await;
            let _ = this.update(cx, |panel, cx| {
                let Some(review) = panel.working_tree_review.as_mut() else {
                    return;
                };
                review.pane.scoped_loading = None;
                match result {
                    Ok((preview, truncated, caption)) => {
                        let label = match &scope {
                            Scope::Branch(branch) => format!("vs {branch}"),
                            Scope::Commit(sha) => sha[..sha.len().min(8)].to_owned(),
                            Scope::Turn(n) => format!("Turn {n}"),
                        };
                        let files = preview.files.clone();
                        let stack = StackedDiff::new(
                            preview,
                            Some(label.into()),
                            review.pane.split,
                            review.pane.attach.clone(),
                            cx,
                        );
                        review.pane.scoped = Some(ScopedView {
                            scope,
                            files,
                            stack,
                            truncated,
                            caption,
                        });
                    }
                    Err(error) => {
                        review.pane.scoped = None;
                        review.pane.scoped_error = Some(error);
                    }
                }
                let pending = std::mem::take(&mut review.pane.scoped_pending);
                cx.notify();
                if pending {
                    panel.sync_scoped_view(cx);
                }
            });
        })
        .detach();
    }

    fn reload_scoped_view(&mut self, cx: &mut Context<Self>) {
        if let Some(review) = &mut self.working_tree_review {
            if review.pane.scoped_loading.is_some() {
                review.pane.scoped_pending = true;
                return;
            }
            review.pane.scoped = None;
        }
        self.sync_scoped_view(cx);
    }

    /// Refresh whatever the visible tab shows.
    pub(crate) fn refresh_review_pane(&mut self, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        match review.pane.tab {
            Tab::Changes if review.pane.changes_scope == ChangesScope::Uncommitted => {
                self.refresh_working_tree_review(cx)
            }
            Tab::Changes => self.reload_scoped_view(cx),
            Tab::Turns => self.reload_review_turns(cx),
            Tab::History => {
                review.pane.commits.clear();
                review.pane.graph.clear();
                review.pane.history_done = false;
                review.pane.commit_selected = None;
                review.pane.scoped = None;
                self.load_history_page(cx);
            }
        }
    }

    pub(crate) fn load_history_page(&mut self, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        if review.pane.history_loading || review.pane.history_done {
            return;
        }
        review.pane.history_loading = true;
        review.pane.history_error = None;
        let location = review.location.clone();
        let directory = review.directory.clone();
        let skip = review.pane.commits.len();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    history::log_graph(&location, &directory, skip, history::LOG_PAGE)
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                let weak = cx.entity().downgrade();
                let Some(review) = panel.working_tree_review.as_mut() else {
                    return;
                };
                let pane = &mut review.pane;
                pane.history_loading = false;
                match result {
                    Ok(page) => {
                        pane.history_done = page.len() < history::LOG_PAGE;
                        pane.commits.extend(page);
                        pane.graph = history::layout_graph(&pane.commits);
                        let scroll = pane.history_list.logical_scroll_top();
                        pane.history_list =
                            ListState::new(pane.commits.len(), ListAlignment::Top, px(HISTORY_ROW));
                        pane.history_list.scroll_to(scroll);
                        pane.history_list.set_scroll_handler(move |event, _, cx| {
                            if event.visible_range.end + 20 >= event.count {
                                let weak = weak.clone();
                                cx.defer(move |cx| {
                                    let _ = weak.update(cx, |panel, cx| panel.load_history_page(cx));
                                });
                            }
                        });
                        if pane.commit_selected.is_none() && !pane.commits.is_empty() {
                            pane.commit_selected = Some(0);
                        }
                    }
                    Err(error) => pane.history_error = Some(error),
                }
                cx.notify();
                panel.sync_scoped_view(cx);
            });
        })
        .detach();
    }

    pub(super) fn render_working_tree_review(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::global();
        let review = self
            .working_tree_review
            .as_ref()
            .expect("working-tree review checked by caller");
        let pane = &review.pane;
        let body = match pane.tab {
            Tab::Changes if pane.changes_scope == ChangesScope::Uncommitted => {
                self.render_uncommitted(cx)
            }
            Tab::Changes => self.render_scoped_body(None, cx),
            Tab::Turns => {
                let list = self.render_turn_list(cx);
                self.render_scoped_body(Some(list), cx)
            }
            Tab::History => {
                let list = self.render_history_list(cx);
                self.render_scoped_body(Some(list), cx)
            }
        };
        div()
            .id("working-tree-review")
            .debug_selector(|| "working-tree-review".into())
            .absolute()
            .inset_0()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.PANEL_BG)
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(self.render_review_header(cx))
            .child(div().flex_1().min_h_0().flex().child(body))
            .into_any_element()
    }

    fn header_button(
        id: &'static str,
        label: impl Into<SharedString>,
        active: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::global();
        div()
            .id(id)
            .debug_selector(move || id.into())
            .flex_none()
            .px_2()
            .py_1()
            .rounded_md()
            .text_size(px(11.))
            .cursor_pointer()
            .text_color(if active { theme.TEXT } else { theme.TEXT_DIM })
            .when(active, |el| el.bg(theme.ACCENT_DIM))
            .when(!active, |el| el.hover(|el| el.bg(theme.QUOTE_BG)))
            .child(label.into())
    }

    fn render_review_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::global();
        let review = self.working_tree_review.as_ref().expect("checked");
        let pane = &review.pane;
        let loading = review.loading || pane.scoped_loading.is_some() || pane.history_loading;
        let summary = match pane.tab {
            Tab::Changes if pane.changes_scope == ChangesScope::Uncommitted => review
                .tree
                .as_ref()
                .map(summary_label)
                .unwrap_or_else(|| crate::platform_dirs::tilde(&review.directory)),
            _ => pane
                .scoped
                .as_ref()
                .filter(|s| Some(&s.scope) == pane.wanted_scope().as_ref())
                .map(|s| s.caption.clone())
                .unwrap_or_else(|| crate::platform_dirs::tilde(&review.directory)),
        };
        let tab = pane.tab;
        let mut scopes = div().flex().flex_wrap().items_center().gap_1();
        if tab == Tab::Changes {
            let uncommitted = pane.changes_scope == ChangesScope::Uncommitted;
            scopes = scopes.child(
                Self::header_button("review-scope-uncommitted", "Uncommitted", uncommitted)
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.set_changes_scope(ChangesScope::Uncommitted, cx);
                        cx.stop_propagation();
                    })),
            );
            if let Some(base) = pane.base.clone() {
                let active = pane.changes_scope == ChangesScope::Branch(base.clone());
                scopes = scopes.child(
                    Self::header_button("review-scope-base", format!("vs {base}"), active)
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            panel.set_changes_scope(ChangesScope::Branch(base.clone()), cx);
                            cx.stop_propagation();
                        })),
                );
            }
            let other = match &pane.changes_scope {
                ChangesScope::Branch(b) if Some(b) != pane.base.as_ref() => Some(b.clone()),
                _ => None,
            };
            scopes = scopes.child(
                Self::header_button(
                    "review-scope-branch",
                    other
                        .clone()
                        .map(|b| format!("vs {b} ▾"))
                        .unwrap_or_else(|| "vs branch… ▾".into()),
                    other.is_some(),
                )
                .on_click(cx.listener(|panel, _, _, cx| {
                    if let Some(review) = &mut panel.working_tree_review {
                        review.pane.branch_menu = !review.pane.branch_menu;
                        if review.pane.branches.is_empty() {
                            panel.load_review_branches(cx);
                        }
                    }
                    cx.notify();
                    cx.stop_propagation();
                })),
            );
        }
        let split = pane.split;
        let menu = (tab == Tab::Changes && pane.branch_menu).then(|| {
            let mut menu = div()
                .id("review-branch-menu")
                .debug_selector(|| "review-branch-menu".into())
                .absolute()
                .top(px(78.))
                .left(px(12.))
                .w(px(260.))
                .max_h(px(280.))
                .overflow_y_scroll()
                .p_1()
                .rounded_md()
                .border_1()
                .border_color(theme.CODE_BORDER)
                .bg(theme.HEADER_BG)
                .shadow_md()
                .flex()
                .flex_col();
            if pane.branches.is_empty() {
                menu = menu.child(
                    div()
                        .p_2()
                        .text_size(px(11.))
                        .text_color(theme.TEXT_DIM)
                        .child("Loading branches…"),
                );
            }
            for (index, branch) in pane.branches.iter().take(200).enumerate() {
                let name = branch.clone();
                menu = menu.child(
                    div()
                        .id(("review-branch", index))
                        .debug_selector(move || format!("review-branch-{index}").into())
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .truncate()
                        .font_family(theme.FONT_MONO)
                        .text_size(px(11.))
                        .text_color(theme.CODE_TEXT)
                        .cursor_pointer()
                        .hover(|el| el.bg(theme.QUOTE_BG))
                        .child(branch.clone())
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            panel.set_changes_scope(ChangesScope::Branch(name.clone()), cx);
                            cx.stop_propagation();
                        })),
                );
            }
            menu
        });
        div()
            .flex_none()
            .relative()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.CODE_BORDER)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex_none()
                            .mr_2()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.TEXT)
                            .child("Review"),
                    )
                    .child(
                        Self::header_button("review-tab-changes", "Changes", tab == Tab::Changes)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.set_review_tab(Tab::Changes, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        Self::header_button("review-tab-turns", "Agent turns", tab == Tab::Turns)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.set_review_tab(Tab::Turns, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        Self::header_button("review-tab-history", "History", tab == Tab::History)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.set_review_tab(Tab::History, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(div().flex_1().min_w_0())
                    .child(
                        Self::header_button("changes-refresh", "Refresh", false)
                            .text_color(theme.ACCENT)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.refresh_review_pane(cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        Self::header_button("review-maximize", "⤢", false)
                            .tooltip(|_, cx| {
                                cx.new(|_| super::changes::ChangesTooltip(
                                    "Maximize or restore · Ctrl+Shift+Enter".into(),
                                ))
                                .into()
                            })
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.dispatch_action(
                                    Box::new(crate::ui_actions::ToggleReviewMaximize {
                                        panel: cx.entity_id(),
                                    }),
                                    cx,
                                );
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        Self::header_button("diff-close", "✕", false).on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|panel, _, window, cx| {
                                panel.close_diff_review(window, cx);
                                cx.stop_propagation();
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(scopes)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(80.))
                            .truncate()
                            .text_size(px(10.))
                            .font_family(theme.FONT_MONO)
                            .text_color(theme.TEXT_DIM)
                            .child(summary),
                    )
                    .when(loading, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(10.))
                                .text_color(theme.ACCENT)
                                .child("Refreshing…"),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .gap_1()
                            .child(
                                Self::header_button("review-unified", "Unified", !split).on_click(
                                    cx.listener(|panel, _, _, cx| {
                                        panel.set_review_split(false, cx);
                                        cx.stop_propagation();
                                    }),
                                ),
                            )
                            .child(
                                Self::header_button("review-split", "Split", split).on_click(
                                    cx.listener(|panel, _, _, cx| {
                                        panel.set_review_split(true, cx);
                                        cx.stop_propagation();
                                    }),
                                ),
                            ),
                    ),
            )
            .children(menu)
            .into_any_element()
    }

    fn side_list(title: &str, count: Option<usize>) -> gpui::Div {
        let theme = Theme::global();
        div()
            .debug_selector(|| "changes-file-list".into())
            .w(relative(0.32))
            .min_w(px(180.))
            .max_w(px(380.))
            .flex_none()
            .flex()
            .flex_col()
            .m_2()
            .mr_0()
            .rounded_xl()
            .border_1()
            .border_color(theme.CODE_BORDER)
            .bg(theme.CODE_BG)
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.CODE_BORDER)
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.TEXT)
                            .child(title.to_owned()),
                    )
                    .children(count.map(|count| {
                        div()
                            .text_size(px(11.))
                            .text_color(theme.TEXT_FAINT)
                            .child(count.to_string())
                    })),
            )
    }

    fn diff_card(content: gpui::AnyElement, footer: Option<(String, bool)>) -> gpui::AnyElement {
        let theme = Theme::global();
        div()
            .min_w_0()
            .flex_1()
            .flex()
            .flex_col()
            .m_2()
            .rounded_xl()
            .border_1()
            .border_color(theme.CODE_BORDER)
            .bg(theme.CODE_BG)
            .overflow_hidden()
            .child(div().flex_1().min_h_0().relative().child(content))
            .children(footer.map(|(text, warn)| {
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.CODE_BORDER)
                    .text_size(px(10.))
                    .text_color(if warn { theme.WARN } else { theme.TEXT_FAINT })
                    .child(text)
            }))
            .into_any_element()
    }

    fn message(text: impl Into<SharedString>, error: bool) -> gpui::AnyElement {
        let theme = Theme::global();
        div()
            .p_4()
            .text_size(px(12.))
            .text_color(if error { theme.ERROR } else { theme.TEXT_DIM })
            .child(text.into())
            .into_any_element()
    }

    fn render_uncommitted(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::global();
        let review = self.working_tree_review.as_ref().expect("checked");
        let tree = review.tree.as_ref();
        let file_count = tree.map(unique_file_count).unwrap_or(0);
        let loading = review.loading;
        let clean = tree.is_some_and(|tree| tree.is_clean());
        let list_panel = cx.entity().downgrade();
        let file_list = list(review.list.clone(), move |index, _, cx| {
            let Some(panel) = list_panel.upgrade() else {
                return div().into_any_element();
            };
            panel.read(cx).render_changes_row(index, list_panel.clone())
        })
        .size_full();
        let side = Self::side_list("Working tree", Some(file_count)).child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .when(review.rows.is_empty(), |el| {
                    el.child(
                        div()
                            .p_4()
                            .text_size(px(11.))
                            .text_color(theme.TEXT_DIM)
                            .child(if loading {
                                "Reading repository…"
                            } else if clean {
                                "Working tree is clean."
                            } else {
                                "No changed files."
                            }),
                    )
                })
                .when(!review.rows.is_empty(), |el| {
                    el.child(file_list).child(crate::scrollbar::vertical_list(
                        &review.list,
                        "changes-file-scrollbar",
                    ))
                }),
        );
        let content = if let Some(content) = &review.content {
            content.render()
        } else if let Some(error) = &review.error {
            Self::message(error.clone(), true)
        } else {
            Self::message(
                if loading {
                    "Loading diff…"
                } else if clean {
                    "No changes to review."
                } else {
                    "Select a file to review."
                },
                false,
            )
        };
        let footer = if review.truncated {
            "Live working-tree snapshot · some large content was omitted · Refresh to update"
        } else {
            "Live working-tree snapshot · Refresh to update"
        };
        div()
            .size_full()
            .flex()
            .child(side)
            .child(Self::diff_card(content, Some((footer.into(), review.truncated))))
            .into_any_element()
    }

    /// Left list (turns or history, or the scoped file tree when `None`) and
    /// the scoped stacked diff.
    fn render_scoped_body(
        &self,
        list: Option<gpui::AnyElement>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::global();
        let review = self.working_tree_review.as_ref().expect("checked");
        let pane = &review.pane;
        let wanted = pane.wanted_scope();
        let scoped = pane
            .scoped
            .as_ref()
            .filter(|s| Some(&s.scope) == wanted.as_ref());
        let content = if let Some(scoped) = scoped {
            if scoped.files.is_empty() {
                Self::message("No differences.", false)
            } else {
                scoped.stack.render()
            }
        } else if let Some(error) = &pane.scoped_error {
            Self::message(error.clone(), true)
        } else if pane.scoped_loading.is_some() {
            Self::message("Loading diff…", false)
        } else {
            Self::message(
                match pane.tab {
                    Tab::Turns => "No agent turn edited files yet.",
                    Tab::History => "Select a commit.",
                    Tab::Changes => "Choose what to compare.",
                },
                false,
            )
        };
        let footer = scoped.and_then(|s| {
            s.truncated
                .then(|| ("Some large content was omitted.".to_owned(), true))
        });
        let side = match list {
            Some(list) => list,
            None => {
                let files = scoped.map(|s| s.files.as_slice()).unwrap_or_default();
                let selected = scoped.map(|s| s.stack.selected());
                Self::side_list("Files", Some(files.len()))
                    .child(
                        div()
                            .id("review-scope-files")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .py_1()
                            .children(files.iter().take(crate::review_stack::MAX_STACKED_FILES).enumerate().map(
                                |(index, file)| {
                                    self.scoped_file_row(
                                        "review-scope-file",
                                        index,
                                        file,
                                        selected == Some(index),
                                        0.,
                                        cx.listener(move |panel, _, _, cx| {
                                            panel.select_scoped_file(index, cx);
                                            cx.stop_propagation();
                                        }),
                                    )
                                },
                            )),
                    )
                    .into_any_element()
            }
        };
        let _ = theme;
        div()
            .size_full()
            .flex()
            .child(side)
            .child(Self::diff_card(content, footer))
            .into_any_element()
    }

    fn scoped_file_row(
        &self,
        prefix: &'static str,
        index: usize,
        file: &DiffFile,
        selected: bool,
        indent: f32,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> gpui::AnyElement {
        let theme = Theme::global();
        let letter = file_letter(file);
        let status = match letter {
            'A' => 'A',
            'D' => 'D',
            'R' => 'R',
            _ => 'M',
        };
        let (added, removed) = file.counts();
        div()
            .id(SharedString::from(format!("{prefix}-{index}")))
            .debug_selector(move || format!("{prefix}-{index}").into())
            .mx_1()
            .h(px(28.))
            .pl(px(8. + indent))
            .pr_2()
            .rounded_md()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .when(selected, |el| el.bg(theme.ACCENT_DIM))
            .when(!selected, |el| el.hover(|el| el.bg(theme.QUOTE_BG)))
            .child(
                div()
                    .w(px(12.))
                    .flex_none()
                    .font_family(theme.FONT_MONO)
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(status_color(status))
                    .child(letter.to_string()),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .font_family(theme.FONT_MONO)
                    .text_size(px(11.))
                    .text_color(if selected { theme.TEXT } else { theme.CODE_TEXT })
                    .child(file.path.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap_1()
                    .font_family(theme.FONT_MONO)
                    .text_size(px(9.))
                    .child(div().text_color(theme.OK).child(format!("+{added}")))
                    .child(div().text_color(theme.ERROR).child(format!("−{removed}"))),
            )
            .on_click(on_click)
            .into_any_element()
    }

    fn render_turn_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::global();
        let pane = &self.working_tree_review.as_ref().expect("checked").pane;
        let mut rows = div()
            .id("review-turns")
            .debug_selector(|| "review-turns".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .py_1()
            .flex()
            .flex_col();
        if pane.turns.is_empty() {
            rows = rows.child(
                div()
                    .p_4()
                    .text_size(px(11.))
                    .text_color(theme.TEXT_DIM)
                    .child("No agent turn in this conversation edited files."),
            );
        }
        let scoped_selected = pane
            .scoped
            .as_ref()
            .filter(|s| matches!(s.scope, Scope::Turn(_)))
            .map(|s| s.stack.selected());
        for (index, turn) in pane.turns.iter().enumerate() {
            let selected = pane.turn_selected == Some(index);
            let expanded = pane.turn_expanded == Some(index);
            let (added, removed) = turn.counts();
            let files = turn.preview.files.len();
            rows = rows.child(
                div()
                    .id(("review-turn", index))
                    .debug_selector(move || format!("review-turn-{index}").into())
                    .mx_1()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(theme.ACCENT_DIM))
                    .when(!selected, |el| el.hover(|el| el.bg(theme.QUOTE_BG)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(11.))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(theme.TEXT_FAINT)
                                    .child(if expanded { "▾" } else { "▸" }),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.TEXT)
                                    .child(format!("Turn {}", turn.number)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_color(theme.TEXT_DIM)
                                    .child(turn.prompt.clone()),
                            ),
                    )
                    .child(
                        div()
                            .pl(px(18.))
                            .flex()
                            .gap_2()
                            .text_size(px(10.))
                            .font_family(theme.FONT_MONO)
                            .text_color(theme.TEXT_FAINT)
                            .child(format!("{files} file{}", if files == 1 { "" } else { "s" }))
                            .child(div().text_color(theme.OK).child(format!("+{added}")))
                            .child(div().text_color(theme.ERROR).child(format!("−{removed}"))),
                    )
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.select_review_turn(index, None, cx);
                        cx.stop_propagation();
                    })),
            );
            if expanded {
                for (file_index, file) in turn.preview.files.iter().enumerate() {
                    let id = index * 1000 + file_index;
                    rows = rows.child(self.scoped_file_row(
                        "review-turn-file",
                        id,
                        file,
                        selected && scoped_selected == Some(file_index),
                        14.,
                        cx.listener(move |panel, _, _, cx| {
                            panel.select_review_turn(index, Some(file_index), cx);
                            cx.stop_propagation();
                        }),
                    ));
                }
            }
        }
        Self::side_list("Agent turns", Some(pane.turns.len()))
            .child(rows)
            .into_any_element()
    }

    fn render_history_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::global();
        let pane = &self.working_tree_review.as_ref().expect("checked").pane;
        let weak = cx.entity().downgrade();
        let now = now_secs();
        let body = if pane.commits.is_empty() {
            div()
                .p_4()
                .text_size(px(11.))
                .text_color(if pane.history_error.is_some() {
                    theme.ERROR
                } else {
                    theme.TEXT_DIM
                })
                .child(match &pane.history_error {
                    Some(error) => error.clone(),
                    None if pane.history_loading => "Reading history…".into(),
                    None => "No commits yet.".into(),
                })
                .into_any_element()
        } else {
            let graph_width = pane
                .graph
                .iter()
                .map(|row| row.width)
                .max()
                .unwrap_or(1)
                .min(12) as f32
                * LANE_WIDTH
                + 8.;
            div()
                .flex_1()
                .min_h_0()
                .size_full()
                .relative()
                .child(
                    list(pane.history_list.clone(), move |index, _, cx| {
                        let Some(panel) = weak.upgrade() else {
                            return div().into_any_element();
                        };
                        panel.read(cx).render_history_row(index, graph_width, now, weak.clone())
                    })
                    .size_full(),
                )
                .child(crate::scrollbar::vertical_list(&pane.history_list, "review-history-scrollbar"))
                .into_any_element()
        };
        Self::side_list("History", Some(pane.commits.len()))
            .debug_selector(|| "review-history".into())
            .w(relative(0.45))
            .max_w(px(560.))
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .into_any_element()
    }

    fn render_history_row(
        &self,
        index: usize,
        graph_width: f32,
        now: i64,
        panel: gpui::WeakEntity<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::global();
        let Some(pane) = self.review_pane() else {
            return div().into_any_element();
        };
        let (Some(commit), Some(row)) = (pane.commits.get(index), pane.graph.get(index)) else {
            return div().into_any_element();
        };
        let selected = pane.commit_selected == Some(index);
        let row = row.clone();
        let graph = gpui::canvas(
            |_, _, _| {},
            move |bounds, _, window, _| paint_graph_row(bounds, &row, window),
        )
        .w(px(graph_width))
        .h_full()
        .flex_none();
        let refs = commit.refs.iter().take(3).map(|name| {
            div()
                .flex_none()
                .px_1()
                .rounded_sm()
                .bg(theme.QUOTE_BG)
                .text_size(px(9.))
                .font_family(theme.FONT_MONO)
                .text_color(if name.starts_with("HEAD") { theme.ACCENT } else { theme.TEXT_DIM })
                .child(name.clone())
        });
        div()
            .id(("review-commit", index))
            .debug_selector(move || format!("review-commit-{index}").into())
            .h(px(HISTORY_ROW))
            .pr_2()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .when(selected, |el| el.bg(theme.ACCENT_DIM))
            .when(!selected, |el| el.hover(|el| el.bg(theme.QUOTE_BG)))
            .child(graph)
            .child(
                div()
                    .flex_none()
                    .font_family(theme.FONT_MONO)
                    .text_size(px(10.))
                    .text_color(theme.TEXT_FAINT)
                    .child(commit.short().to_owned()),
            )
            .children(refs)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(if selected { theme.TEXT } else { theme.CODE_TEXT })
                    .child(commit.subject.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .max_w(px(90.))
                    .truncate()
                    .text_size(px(10.))
                    .text_color(theme.TEXT_FAINT)
                    .child(commit.author.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.))
                    .text_color(theme.TEXT_FAINT)
                    .child(relative_date(commit.time, now)),
            )
            .on_click(move |_, _, cx| {
                let _ = panel.update(cx, |panel, cx| panel.select_review_commit(index, cx));
            })
            .into_any_element()
    }

    fn render_changes_row(&self, row_index: usize, panel: gpui::WeakEntity<Self>) -> gpui::AnyElement {
        let theme = Theme::global();
        let Some(review) = &self.working_tree_review else {
            return div().into_any_element();
        };
        let Some(row) = review.rows.get(row_index) else {
            return div().into_any_element();
        };
        match *row {
            ChangesRow::Group(area, count) => div()
                .h(px(30.))
                .px_3()
                .pt_2()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.TEXT_DIM)
                .child(area.label())
                .child(
                    div()
                        .rounded_full()
                        .px_1p5()
                        .bg(theme.QUOTE_BG)
                        .text_size(px(10.))
                        .text_color(theme.TEXT_FAINT)
                        .child(count.to_string()),
                )
                .into_any_element(),
            ChangesRow::File(index) => {
                let Some(file) = review.tree.as_ref().and_then(|tree| tree.files.get(index)) else {
                    return div().into_any_element();
                };
                let selected = review.selected == index;
                let previous = file.previous_path.clone();
                div()
                    .id(("changes-file", index))
                    .debug_selector(move || format!("changes-file-{index}").into())
                    .mx_1()
                    .h(px(if previous.is_some() { 40. } else { 28. }))
                    .px_2()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(theme.ACCENT_DIM))
                    .when(!selected, |el| el.hover(|el| el.bg(theme.QUOTE_BG)))
                    .child(
                        div()
                            .w(px(12.))
                            .flex_none()
                            .font_family(theme.FONT_MONO)
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(status_color(file.status))
                            .child(file.status.to_string()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .font_family(theme.FONT_MONO)
                                    .text_size(px(11.))
                                    .text_color(if selected { theme.TEXT } else { theme.CODE_TEXT })
                                    .child(file.path.clone()),
                            )
                            .children(previous.map(|old| {
                                div()
                                    .truncate()
                                    .font_family(theme.FONT_MONO)
                                    .text_size(px(9.))
                                    .text_color(theme.TEXT_FAINT)
                                    .child(format!("from {old}"))
                            })),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .gap_1()
                            .font_family(theme.FONT_MONO)
                            .text_size(px(9.))
                            .child(div().text_color(theme.OK).child(format!("+{}", file.added)))
                            .child(div().text_color(theme.ERROR).child(format!("−{}", file.removed))),
                    )
                    .on_click(move |_, _, cx| {
                        let _ = panel.update(cx, |panel, cx| panel.select_working_tree_file(index, cx));
                    })
                    .into_any_element()
            }
        }
    }

    /// "Review changes" under a finished assistant turn that edited files.
    /// `index` is the turn's response-stats row.
    pub(super) fn turn_review_button(&self, index: usize, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.git_changes_context().is_none() {
            return None;
        }
        let edited = self.items[..index.min(self.items.len())]
            .iter()
            .rev()
            .take_while(|item| !matches!(item, Item::User(_)))
            .any(|item| {
                matches!(item, Item::Tool { name, input, done: true, error: None, .. }
                    if crate::diff_model::from_tool(name, input).is_some_and(|p| !p.files.is_empty()))
            });
        if !edited {
            return None;
        }
        let theme = Theme::global();
        let source = cx.entity_id();
        Some(
            div()
                .id(("turn-review-changes", index))
                .debug_selector(move || format!("turn-review-changes-{index}").into())
                .ml_1()
                .mt_1()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(theme.CODE_BORDER)
                .text_size(px(11.))
                .text_color(theme.ACCENT)
                .cursor_pointer()
                .hover(|el| el.bg(theme.QUOTE_BG))
                .child("Review changes")
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    window.dispatch_action(
                        Box::new(crate::ui_actions::OpenWorkingTreeChanges { source, staged: false }),
                        cx,
                    );
                })
                .into_any_element(),
        )
    }
}

fn load_scope(
    location: &Location,
    directory: &str,
    scope: &Scope,
) -> Result<(DiffPreview, bool, String), String> {
    match scope {
        Scope::Branch(branch) => {
            let (patch, truncated) = history::diff_range(location, directory, branch, None)?;
            let preview = crate::diff_model::from_patch(&patch).unwrap_or(DiffPreview { files: Vec::new() });
            let (added, removed) = preview.counts();
            let caption = format!(
                "merge-base({branch}, HEAD) → working tree · {} file{} · +{added} −{removed}",
                preview.files.len(),
                if preview.files.len() == 1 { "" } else { "s" }
            );
            Ok((preview, truncated, caption))
        }
        Scope::Commit(sha) => {
            let detail = history::show_commit(location, directory, sha)?;
            let preview =
                crate::diff_model::from_patch(&detail.patch).unwrap_or(DiffPreview { files: Vec::new() });
            let (added, removed) = preview.counts();
            let caption = format!(
                "{} · {} · {} · {} file{} · +{added} −{removed}",
                detail.commit.short(),
                detail.commit.subject,
                detail.commit.author,
                preview.files.len(),
                if preview.files.len() == 1 { "" } else { "s" }
            );
            Ok((preview, detail.truncated, caption))
        }
        Scope::Turn(_) => Err("Agent turns load from the transcript".into()),
    }
}

fn paint_graph_row(bounds: gpui::Bounds<gpui::Pixels>, row: &GraphRow, window: &mut Window) {
    let x = |lane: usize| bounds.left() + px(8. + lane as f32 * LANE_WIDTH);
    let y = |edge: Edge| match edge {
        Edge::Top => bounds.top(),
        Edge::Middle => bounds.top() + bounds.size.height / 2.,
        Edge::Bottom => bounds.bottom(),
    };
    for segment in &row.segments {
        let mut builder = gpui::PathBuilder::stroke(px(1.5));
        let from = gpui::point(x(segment.from_lane), y(segment.from));
        let to = gpui::point(x(segment.to_lane), y(segment.to));
        builder.move_to(from);
        if segment.from_lane == segment.to_lane {
            builder.line_to(to);
        } else {
            let mid = gpui::point(from.x, to.y);
            builder.curve_to(to, mid);
        }
        if let Ok(path) = builder.build() {
            window.paint_path(path, lane_color(segment.color));
        }
    }
    let center = gpui::point(x(row.lane), y(Edge::Middle));
    let size = px(8.);
    window.paint_quad(
        gpui::fill(
            gpui::Bounds::centered_at(center, gpui::size(size, size)),
            lane_color(row.lane),
        )
        .corner_radii(size / 2.),
    );
}

#[cfg(test)]
#[path = "review_pane_tests.rs"]
mod tests;
