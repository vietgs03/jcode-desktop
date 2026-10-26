//! Review pane diff area: every file's diff stacked in one continuous scroll
//! (multibuffer style). Each file is its own `DiffView` so the file tree can
//! scroll straight to it and collapse state stays per file.
use gpui::{AnyElement, App, Entity, ScrollHandle, SharedString, div, prelude::*};

use crate::diff_model::{DiffFile, DiffPreview};
use crate::diff_view::{AttachHandler, DiffView};

/// Files beyond this are listed in the tree but not rendered.
pub(crate) const MAX_STACKED_FILES: usize = 400;

pub(crate) struct StackedDiff {
    views: Vec<Entity<DiffView>>,
    scroll: ScrollHandle,
    split: bool,
    selected: usize,
}

impl StackedDiff {
    pub(crate) fn new(
        preview: DiffPreview,
        label: Option<SharedString>,
        split: bool,
        attach: Option<AttachHandler>,
        cx: &mut App,
    ) -> Self {
        let views = preview
            .files
            .into_iter()
            .take(MAX_STACKED_FILES)
            .map(|file: DiffFile| {
                let label = label.clone();
                let attach = attach.clone();
                cx.new(|cx| {
                    let mut view = DiffView::new(DiffPreview { files: vec![file] }, cx);
                    view.set_status(true, false, cx);
                    view.set_label(label);
                    view.set_bare(true);
                    view.set_attach(attach);
                    view.set_split(split, cx);
                    view
                })
            })
            .collect();
        Self {
            views,
            scroll: ScrollHandle::new(),
            split,
            selected: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.views.len()
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn split(&self) -> bool {
        self.split
    }

    pub(crate) fn scroll_handle(&self) -> &ScrollHandle {
        &self.scroll
    }

    /// Bring file `index` to the top of the scroll.
    pub(crate) fn select(&mut self, index: usize) {
        if index < self.views.len() {
            self.selected = index;
            self.scroll.scroll_to_top_of_item(index);
        }
    }

    pub(crate) fn set_split(&mut self, split: bool, cx: &mut App) {
        self.split = split;
        for view in &self.views {
            view.update(cx, |view, cx| view.set_split(split, cx));
        }
    }

    pub(crate) fn render(&self) -> AnyElement {
        div()
            .size_full()
            .min_h_0()
            .min_w_0()
            .relative()
            .child(
                div()
                    .id("review-stack-scroll")
                    .debug_selector(|| "review-stack-scroll".into())
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .p_2()
                    .children(self.views.iter().enumerate().map(|(index, view)| {
                        div()
                            .debug_selector(move || format!("review-stack-file-{index}").into())
                            .w_full()
                            .min_w_0()
                            .flex_none()
                            .child(view.clone())
                    })),
            )
            .child(crate::scrollbar::vertical(&self.scroll, "review-stack-scrollbar"))
            .into_any_element()
    }
}
