//! Live git working-tree summary, footer pill and dedicated Changes review.
use super::*;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::diff_model::{DiffFile, DiffPreview};
use jcode_desktop_harness::session_git::{
    self, Area, ChangedFile, Location, WorkingTree, WorkingTreePatches,
};

const SUMMARY_THROTTLE: Duration = Duration::from_secs(2);

/// `/changes` opens the review. `Some(true)` for `--staged`/`--cached`.
/// Other arguments fall through to the transcript command's usage error.
pub(super) fn changes_command(content: &str) -> Option<bool> {
    let mut parts = content.split_whitespace();
    if parts.next()? != "/changes" {
        return None;
    }
    match (parts.next(), parts.next()) {
        (None, _) => Some(false),
        (Some("--staged" | "--cached"), None) => Some(true),
        _ => None,
    }
}

#[derive(Clone)]
pub(super) enum ChangesRow {
    Group(Area, usize),
    File(usize),
}

pub(super) struct WorkingTreeReview {
    pub(super) location: Location,
    pub(super) directory: String,
    pub(super) tree: Option<WorkingTree>,
    pub(super) rows: Vec<ChangesRow>,
    pub(super) list: ListState,
    pub(super) content: Option<crate::review_stack::StackedDiff>,
    pub(super) selected: usize,
    pub(super) prefer_staged: bool,
    pub(super) loading: bool,
    pub(super) refresh_pending: bool,
    pub(super) error: Option<String>,
    pub(super) truncated: bool,
    /// Scopes, tabs, agent turns and history of the Review pane.
    pub(super) pane: super::review_pane::PaneState,
}

struct ReviewSnapshot {
    tree: WorkingTree,
    preview: DiffPreview,
    truncated: bool,
}

pub(super) struct ChangesTooltip(pub(super) String);

impl Render for ChangesTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .debug_selector(|| "changes-pill-tooltip".into())
            .max_w(px(360.))
            .p_2()
            .rounded_md()
            .bg(Theme::global().HEADER_BG)
            .text_size(px(12.))
            .text_color(Theme::global().TEXT)
            .child(self.0.clone())
    }
}

pub(super) fn status_kind(status: char) -> &'static str {
    match status {
        '?' | 'A' => "Added",
        'D' => "Deleted",
        'R' | 'C' => "Renamed",
        'U' => "Conflict",
        _ => "Modified",
    }
}

pub(super) fn status_color(status: char) -> gpui::Rgba {
    let theme = Theme::global();
    match status {
        '?' | 'A' => theme.OK,
        'D' | 'U' => theme.ERROR,
        'R' | 'C' => theme.ACCENT,
        _ => theme.WARN,
    }
}

pub(super) fn unique_file_count(tree: &WorkingTree) -> usize {
    tree.files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<HashSet<_>>()
        .len()
}

pub(super) fn branch_label(tree: &WorkingTree) -> String {
    let mut label = tree
        .branch
        .head
        .clone()
        .or_else(|| tree.branch.oid.as_ref().map(|oid| format!("detached @ {oid}")))
        .unwrap_or_else(|| "No commits".into());
    if tree.branch.ahead > 0 {
        label.push_str(&format!("  ↑{}", tree.branch.ahead));
    }
    if tree.branch.behind > 0 {
        label.push_str(&format!("  ↓{}", tree.branch.behind));
    }
    label
}

pub(super) fn summary_label(tree: &WorkingTree) -> String {
    let (added, removed) = tree.totals();
    format!(
        "{} · {} file{} · +{} −{} · {} staged · {} unstaged · {} untracked",
        branch_label(tree),
        unique_file_count(tree),
        if unique_file_count(tree) == 1 { "" } else { "s" },
        added,
        removed,
        tree.count(Area::Staged),
        tree.count(Area::Unstaged),
        tree.count(Area::Untracked),
    )
}

fn grouped_rows(tree: &WorkingTree) -> Vec<ChangesRow> {
    let mut rows = Vec::new();
    for area in Area::ALL {
        let indices: Vec<_> = tree
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| (file.area == area).then_some(index))
            .collect();
        if !indices.is_empty() {
            rows.push(ChangesRow::Group(area, indices.len()));
            rows.extend(indices.into_iter().map(ChangesRow::File));
        }
    }
    rows
}

fn fallback_file(change: &ChangedFile, truncated: bool) -> DiffFile {
    let note = if change.binary {
        "Binary file. Text preview unavailable."
    } else if change.area == Area::Untracked && change.added == 0 {
        "Empty untracked file."
    } else if truncated {
        "Preview omitted because the working-tree review reached its size limit."
    } else {
        "No line content is available for this change."
    };
    DiffFile {
        path: change.path.clone(),
        previous_path: change.previous_path.clone(),
        kind: status_kind(change.status).into(),
        hunks: Vec::new(),
        note: Some(note.into()),
    }
}

fn files_by_path(patch: &str) -> HashMap<String, VecDeque<DiffFile>> {
    let mut by_path: HashMap<String, VecDeque<DiffFile>> = HashMap::new();
    if let Some(preview) = crate::diff_model::from_patch(patch) {
        for file in preview.files {
            by_path.entry(file.path.clone()).or_default().push_back(file);
        }
    }
    by_path
}

fn review_snapshot(mut tree: WorkingTree, patches: WorkingTreePatches) -> ReviewSnapshot {
    tree.files.sort_by(|a, b| {
        (a.area, a.path.as_str(), a.status).cmp(&(b.area, b.path.as_str(), b.status))
    });
    let mut parsed = HashMap::new();
    parsed.insert(Area::Staged, files_by_path(&patches.staged));
    parsed.insert(Area::Unstaged, files_by_path(&patches.unstaged));
    parsed.insert(Area::Untracked, files_by_path(&patches.untracked));
    let files = tree
        .files
        .iter()
        .map(|change| {
            let mut file = parsed
                .get_mut(&change.area)
                .and_then(|area| area.get_mut(&change.path))
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| fallback_file(change, patches.truncated));
            file.kind = status_kind(change.status).into();
            if file.previous_path.is_none() {
                file.previous_path = change.previous_path.clone();
            }
            if change.binary {
                file.hunks.clear();
                file.note = Some("Binary file. Text preview unavailable.".into());
            }
            file
        })
        .collect();
    ReviewSnapshot {
        tree,
        preview: DiffPreview { files },
        truncated: patches.truncated,
    }
}

impl Panel {
    /// Where this conversation's checkout lives, for local and WSL sessions.
    pub(crate) fn git_changes_context(&self) -> Option<(Location, String)> {
        if self.is_change_review()
            || self.is_accounts_panel()
            || self.is_changelog()
            || self.is_side_document()
            || self.preview_state.is_some()
            || self.terminal.is_some()
        {
            return None;
        }
        let location = Location::of_session(&self.session_id)?;
        let directory = self
            .working_dir
            .as_deref()
            .map(str::trim)
            .filter(|directory| !directory.is_empty())?
            .to_owned();
        Some((location, directory))
    }

    fn sync_changes_context(&mut self) -> Option<(Location, String)> {
        let context = self.git_changes_context()?;
        if self.changes_context.as_ref() != Some(&context) {
            self.changes_context = Some(context.clone());
            self.changes_summary = None;
            self.changes_last_started = None;
            // An in-flight probe of the old checkout is discarded on arrival
            // and immediately followed by one for the new checkout.
            self.changes_refresh_pending = self.changes_refresh_in_flight;
        }
        Some(context)
    }

    fn start_changes_summary_refresh(
        &mut self,
        context: (Location, String),
        cx: &mut Context<Self>,
    ) {
        if self.changes_refresh_in_flight {
            self.changes_refresh_pending = true;
            return;
        }
        self.changes_refresh_in_flight = true;
        self.changes_last_started = Some(cx.background_executor().now());
        let worker_context = context.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    session_git::working_tree(&worker_context.0, &worker_context.1)
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.changes_refresh_in_flight = false;
                if panel.changes_context.as_ref() == Some(&context) {
                    panel.changes_summary = Some(result);
                }
                let pending = std::mem::take(&mut panel.changes_refresh_pending);
                cx.notify();
                if pending {
                    panel.request_changes_summary_refresh(true, cx);
                }
            });
        })
        .detach();
    }

    /// `immediate=false` coalesces turn-completion bursts within two seconds.
    fn request_changes_summary_refresh(&mut self, immediate: bool, cx: &mut Context<Self>) {
        let Some(context) = self.sync_changes_context() else {
            return;
        };
        if self.changes_refresh_in_flight {
            self.changes_refresh_pending = true;
            return;
        }
        if !immediate
            && let Some(started) = self.changes_last_started
            && let Some(remaining) = SUMMARY_THROTTLE
                .checked_sub(cx.background_executor().now().saturating_duration_since(started))
                .filter(|remaining| !remaining.is_zero())
        {
            if !self.changes_refresh_timer {
                self.changes_refresh_timer = true;
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(remaining).await;
                    let _ = this.update(cx, |panel, cx| {
                        panel.changes_refresh_timer = false;
                        panel.request_changes_summary_refresh(true, cx);
                    });
                })
                .detach();
            }
            return;
        }
        self.start_changes_summary_refresh(context, cx);
    }

    pub(super) fn refresh_changes_after_turn(&mut self, cx: &mut Context<Self>) {
        if self.git_changes_context().is_some() {
            self.request_changes_summary_refresh(false, cx);
        }
    }

    /// Small footer entry point. The first render starts one background probe.
    pub(super) fn render_changes_pill(
        &mut self,
        screenshot_mode: bool,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if screenshot_mode || self.git_changes_context().is_none() {
            return None;
        }
        if self.changes_summary.is_none() && !self.changes_refresh_in_flight {
            self.request_changes_summary_refresh(true, cx);
        }
        let tree = self.changes_summary.as_ref()?.as_ref().ok()?;
        if tree.is_clean() {
            return None;
        }
        let (added, removed) = tree.totals();
        let tooltip = format!("Changes · {}", summary_label(tree));
        let source = cx.entity_id();
        let theme = Theme::global();
        Some(
            super::composer::composer_pill("panel-changes")
                .flex_none()
                .text_color(theme.TEXT_DIM)
                .tooltip(move |_, cx| cx.new(|_| ChangesTooltip(tooltip.clone())).into())
                .child(
                    gpui::svg()
                        .data(include_bytes!("../../../assets/icons/worktree.svg") as &'static [u8])
                        .size(px(11.))
                        .flex_none()
                        .text_color(theme.TEXT_FAINT),
                )
                .child(
                    div()
                        .text_color(theme.OK)
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .text_color(theme.ERROR)
                        .child(format!("−{removed}")),
                )
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    window.dispatch_action(
                        Box::new(crate::ui_actions::OpenWorkingTreeChanges {
                            source,
                            staged: false,
                        }),
                        cx,
                    );
                })
                .into_any_element(),
        )
    }

    pub(crate) fn set_working_tree_review(
        &mut self,
        location: Location,
        directory: String,
        prefer_staged: bool,
        cx: &mut Context<Self>,
    ) {
        self.title = "Changes".into();
        self.diff_review = None;
        let same = self.working_tree_review.as_ref().is_some_and(|review| {
            review.location == location && review.directory == directory
        });
        if !same {
            self.working_tree_review = Some(WorkingTreeReview {
                location,
                directory,
                tree: None,
                rows: Vec::new(),
                list: ListState::new(0, ListAlignment::Top, px(34.)),
                content: None,
                selected: 0,
                prefer_staged,
                loading: false,
                refresh_pending: false,
                error: None,
                truncated: false,
                pane: super::review_pane::PaneState::new(),
            });
            self.load_review_branches(cx);
        } else if let Some(review) = &mut self.working_tree_review {
            review.prefer_staged |= prefer_staged;
        }
        self.refresh_working_tree_review(cx);
    }

    pub(super) fn refresh_working_tree_review(&mut self, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        if review.loading {
            review.refresh_pending = true;
            return;
        }
        review.loading = true;
        review.error = None;
        let location = review.location.clone();
        let directory = review.directory.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let tree = session_git::working_tree(&location, &directory)?;
                    let patches = session_git::working_tree_patches(&location, &directory, &tree)?;
                    Ok::<_, String>(review_snapshot(tree, patches))
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                let Some(current) = panel.working_tree_review.as_ref() else {
                    return;
                };
                let old_key = current
                    .tree
                    .as_ref()
                    .and_then(|tree| tree.files.get(current.selected))
                    .map(|file| (file.area, file.path.clone()));
                let prefer_staged = current.prefer_staged;
                let (tree, content, selected, rows, truncated, error) = match result {
                    Ok(snapshot) => {
                        let selected = if prefer_staged {
                            snapshot
                                .tree
                                .files
                                .iter()
                                .position(|file| file.area == Area::Staged)
                                .unwrap_or(0)
                        } else {
                            old_key
                                .as_ref()
                                .and_then(|(area, path)| {
                                    snapshot.tree.files.iter().position(|file| {
                                        file.area == *area && file.path == *path
                                    })
                                })
                                .unwrap_or(0)
                        };
                        let split = current.pane.split;
                        let attach = current.pane.attach.clone();
                        let content = (!snapshot.preview.files.is_empty()).then(|| {
                            let mut stack = crate::review_stack::StackedDiff::new(
                                snapshot.preview,
                                Some("Working tree".into()),
                                split,
                                attach,
                                cx,
                            );
                            stack.select(selected);
                            stack
                        });
                        let rows = grouped_rows(&snapshot.tree);
                        (
                            Some(snapshot.tree),
                            content,
                            selected,
                            rows,
                            snapshot.truncated,
                            None,
                        )
                    }
                    Err(error) => (None, None, 0, Vec::new(), false, Some(error)),
                };
                let review = panel.working_tree_review.as_mut().expect("checked above");
                review.tree = tree;
                review.content = content;
                review.selected = selected;
                review.list = ListState::new(rows.len(), ListAlignment::Top, px(34.));
                review.rows = rows;
                review.truncated = truncated;
                review.error = error;
                review.loading = false;
                review.prefer_staged = false;
                let pending = std::mem::take(&mut review.refresh_pending);
                cx.notify();
                if pending {
                    panel.refresh_working_tree_review(cx);
                }
            });
        })
        .detach();
    }

    pub(super) fn select_working_tree_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(review) = &mut self.working_tree_review else {
            return;
        };
        let Some(tree) = &review.tree else {
            return;
        };
        if index >= tree.files.len() {
            return;
        }
        review.selected = index;
        if let Some(content) = &mut review.content {
            content.select(index);
        }
        cx.notify();
    }
}

#[cfg(test)]
#[path = "panel_changes_tests.rs"]
mod tests;
