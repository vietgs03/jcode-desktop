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
enum ChangesRow {
    Group(Area, usize),
    File(usize),
}

pub(super) struct WorkingTreeReview {
    location: Location,
    directory: String,
    tree: Option<WorkingTree>,
    rows: Vec<ChangesRow>,
    list: ListState,
    content: Option<crate::diff_review_content::ReviewContent>,
    selected: usize,
    prefer_staged: bool,
    loading: bool,
    refresh_pending: bool,
    error: Option<String>,
    truncated: bool,
}

struct ReviewSnapshot {
    tree: WorkingTree,
    preview: DiffPreview,
    truncated: bool,
}

struct ChangesTooltip(String);

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

fn status_kind(status: char) -> &'static str {
    match status {
        '?' | 'A' => "Added",
        'D' => "Deleted",
        'R' | 'C' => "Renamed",
        'U' => "Conflict",
        _ => "Modified",
    }
}

fn status_color(status: char) -> gpui::Rgba {
    let theme = Theme::global();
    match status {
        '?' | 'A' => theme.OK,
        'D' | 'U' => theme.ERROR,
        'R' | 'C' => theme.ACCENT,
        _ => theme.WARN,
    }
}

fn unique_file_count(tree: &WorkingTree) -> usize {
    tree.files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<HashSet<_>>()
        .len()
}

fn branch_label(tree: &WorkingTree) -> String {
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

fn summary_label(tree: &WorkingTree) -> String {
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
    pub(super) fn git_changes_context(&self) -> Option<(Location, String)> {
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
            });
        } else if let Some(review) = &mut self.working_tree_review {
            review.prefer_staged |= prefer_staged;
        }
        self.refresh_working_tree_review(cx);
    }

    fn refresh_working_tree_review(&mut self, cx: &mut Context<Self>) {
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
                        let content = (!snapshot.preview.files.is_empty()).then(|| {
                            crate::diff_review_content::ReviewContent::with_label(
                                snapshot.preview,
                                selected,
                                true,
                                false,
                                Some("Working tree".into()),
                                cx,
                            )
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

    fn select_working_tree_file(&mut self, index: usize, cx: &mut Context<Self>) {
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
            content.select(index, cx);
        }
        cx.notify();
    }

    fn render_changes_row(
        &self,
        row_index: usize,
        panel: gpui::WeakEntity<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::global();
        let Some(review) = &self.working_tree_review else {
            return div().into_any_element();
        };
        let Some(row) = review.rows.get(row_index) else {
            return div().into_any_element();
        };
        match *row {
            ChangesRow::Group(area, count) => div()
                .h(px(34.))
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
                let Some(tree) = &review.tree else {
                    return div().into_any_element();
                };
                let Some(file) = tree.files.get(index) else {
                    return div().into_any_element();
                };
                let selected = review.selected == index;
                let max = (file.added + file.removed).max(1) as f32;
                let add_fraction = file.added as f32 / max;
                let remove_fraction = file.removed as f32 / max;
                let path = file.path.clone();
                let previous = file.previous_path.clone();
                let status = file.status.to_string();
                div()
                    .id(("changes-file", index))
                    .debug_selector(move || format!("changes-file-{index}").into())
                    .mx_1()
                    .h(px(if previous.is_some() { 42. } else { 36. }))
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
                            .child(status),
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
                                    .text_color(if selected {
                                        theme.TEXT
                                    } else {
                                        theme.CODE_TEXT
                                    })
                                    .child(path),
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
                            .w(px(54.))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .w_full()
                                    .h(px(4.))
                                    .rounded_full()
                                    .overflow_hidden()
                                    .flex()
                                    .bg(theme.QUOTE_BG)
                                    .when(add_fraction > 0., |el| {
                                        el.child(
                                            div()
                                                .h_full()
                                                .w(relative(add_fraction))
                                                .bg(theme.OK),
                                        )
                                    })
                                    .when(remove_fraction > 0., |el| {
                                        el.child(
                                            div()
                                                .h_full()
                                                .w(relative(remove_fraction))
                                                .bg(theme.ERROR),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .gap_1()
                                    .font_family(theme.FONT_MONO)
                                    .text_size(px(9.))
                                    .child(
                                        div()
                                            .text_color(theme.OK)
                                            .child(format!("+{}", file.added)),
                                    )
                                    .child(
                                        div()
                                            .text_color(theme.ERROR)
                                            .child(format!("−{}", file.removed)),
                                    ),
                            ),
                    )
                    .on_click(move |_, _, cx| {
                        let _ = panel.update(cx, |panel, cx| {
                            panel.select_working_tree_file(index, cx)
                        });
                    })
                    .into_any_element()
            }
        }
    }

    pub(super) fn render_working_tree_review(
        &self,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::global();
        let review = self
            .working_tree_review
            .as_ref()
            .expect("working-tree review checked by caller");
        let tree = review.tree.as_ref();
        let summary = tree.map(summary_label);
        let file_count = tree.map(unique_file_count).unwrap_or(0);
        let panel = cx.entity().downgrade();
        let list_panel = panel.clone();
        let file_list = list(review.list.clone(), move |index, _, cx| {
            let Some(panel) = list_panel.upgrade() else {
                return div().into_any_element();
            };
            panel
                .read(cx)
                .render_changes_row(index, list_panel.clone())
        })
        .size_full();
        let content = review.content.as_ref().map(|content| content.render());
        let loading = review.loading;
        let error = review.error.clone();
        let placeholder = content.is_none() && error.is_none();
        let clean = tree.is_some_and(WorkingTree::is_clean);
        let truncated = review.truncated;
        let directory = crate::platform_dirs::tilde(&review.directory);
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
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_3()
                    .p_3()
                    .border_b_1()
                    .border_color(theme.CODE_BORDER)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
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
                                            .text_size(px(14.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.TEXT)
                                            .child("Changes"),
                                    )
                                    .when(loading, |el| {
                                        el.child(
                                            div()
                                                .text_size(px(10.))
                                                .text_color(theme.ACCENT)
                                                .child("Refreshing…"),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(10.))
                                    .font_family(theme.FONT_MONO)
                                    .text_color(theme.TEXT_DIM)
                                    .child(summary.unwrap_or(directory)),
                            ),
                    )
                    .child(
                        div()
                            .id("changes-refresh")
                            .debug_selector(|| "changes-refresh".into())
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_size(px(11.))
                            .text_color(theme.ACCENT)
                            .cursor_pointer()
                            .hover(|el| el.bg(theme.QUOTE_BG))
                            .child("Refresh")
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.refresh_working_tree_review(cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id("diff-close")
                            .debug_selector(|| "diff-close".into())
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_size(px(11.))
                            .text_color(theme.TEXT_DIM)
                            .cursor_pointer()
                            .hover(|el| el.bg(theme.QUOTE_BG))
                            .child("Close review · Esc")
                            .on_mouse_down(
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
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .debug_selector(|| "changes-file-list".into())
                            .w(relative(0.32))
                            .min_w(px(220.))
                            .max_w(px(420.))
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
                                            .child("Working tree"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(theme.TEXT_FAINT)
                                            .child(file_count.to_string()),
                                    ),
                            )
                            .child(
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
                            ),
                    )
                    .child(
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
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .relative()
                                    .when_some(content, |el, content| el.child(content))
                                    .when_some(error, |el, error| {
                                        el.child(
                                            div()
                                                .p_4()
                                                .text_size(px(12.))
                                                .text_color(theme.ERROR)
                                                .child(error),
                                        )
                                    })
                                    .when(placeholder, |el| {
                                        el.child(
                                            div()
                                                .p_4()
                                                .text_size(px(12.))
                                                .text_color(theme.TEXT_DIM)
                                                .child(if loading {
                                                    "Loading diff…"
                                                } else if clean {
                                                    "No changes to review."
                                                } else {
                                                    "Select a file to review."
                                                }),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .px_3()
                                    .py_2()
                                    .border_t_1()
                                    .border_color(theme.CODE_BORDER)
                                    .text_size(px(10.))
                                    .text_color(if truncated {
                                        theme.WARN
                                    } else {
                                        theme.TEXT_FAINT
                                    })
                                    .child(if truncated {
                                        "Live working-tree snapshot · some large content was omitted · Refresh to update"
                                    } else {
                                        "Live working-tree snapshot · Refresh to update"
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "panel_changes_tests.rs"]
mod tests;
