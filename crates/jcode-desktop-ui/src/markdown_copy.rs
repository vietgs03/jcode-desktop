//! Click-to-copy buttons for transcript surfaces (code blocks, tables,
//! messages, tool output, errors), mirroring the CLI's copy targets.
//!
//! Feedback state is a single "last copied" marker: only one button can show
//! "Copied" at a time, so a thread-local slot is enough and no render path
//! needs a mutable window.
use std::cell::RefCell;
use std::time::{Duration, Instant};

use gpui::{SharedString, div, prelude::*, px};

use crate::theme::Theme;

/// How long a button keeps its "Copied" label.
pub(crate) const FEEDBACK: Duration = Duration::from_millis(1500);

thread_local! {
    static LAST_COPIED: RefCell<Option<(SharedString, Instant)>> = const { RefCell::new(None) };
}

/// Whether the button with `id` was clicked within the feedback window.
pub(crate) fn is_copied(id: &str) -> bool {
    LAST_COPIED.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|(copied, at)| copied.as_ref() == id && at.elapsed() < FEEDBACK)
    })
}

fn mark_copied(id: SharedString, cx: &mut gpui::App) {
    LAST_COPIED.with(|slot| *slot.borrow_mut() = Some((id, Instant::now())));
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(FEEDBACK + Duration::from_millis(50))
            .await;
        let _ = cx.update(|cx| cx.refresh_windows());
    })
    .detach();
}

/// A compact "copy" control. `id` must be stable across frames (derive it
/// from content or the row index). `selector` names it for tests.
pub(crate) fn copy_button(
    id: impl Into<SharedString>,
    selector: &'static str,
    text: String,
) -> gpui::Stateful<gpui::Div> {
    let id: SharedString = id.into();
    let copied = is_copied(&id);
    let click_id = id.clone();
    div()
        .id(gpui::ElementId::Name(id))
        .debug_selector(move || {
            if copied {
                format!("{selector}-copied")
            } else {
                selector.to_string()
            }
        })
        .flex_none()
        .px_1p5()
        .rounded_sm()
        .cursor_pointer()
        .text_size(px(10.5))
        .text_color(if copied {
            Theme::global().ACCENT
        } else {
            Theme::global().TEXT_FAINT
        })
        .hover(|el| el.text_color(Theme::global().TEXT).bg(Theme::global().INLINE_CODE_BG))
        // Swallow the press so transcript selection and panel focus handlers
        // never start a drag or steal the click.
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
            mark_copied(click_id.clone(), cx);
        })
        .child(if copied { "✓ Copied" } else { "⧉ Copy" })
}
