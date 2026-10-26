//! Ctrl+F find-in-transcript for a chat panel.
//!
//! Searches the panel's logical selectable document (the plain text of every
//! rendered leaf, including rows the virtual list has not mounted), so large
//! sessions never search rendered elements. Matches are painted by the shared
//! `TextSelection` leaves: all matches subtle, the current one strong.
use super::*;

gpui::actions!(
    panel_find,
    [OpenFind, OpenFindIfEmpty, FindNext, FindPrevious, CloseFind]
);

const KEY_CONTEXT: &str = "PanelFind";
/// Typing pauses this long before the transcript is searched again.
const DEBOUNCE: Duration = Duration::from_millis(90);

/// Registered from `input::bind_keys`, right after the composer's own keys, so
/// these outrank them at equal context depth (the composer binds Ctrl+F to a
/// word motion, which still wins while a draft is being edited).
pub(crate) fn bindings() -> Vec<gpui::KeyBinding> {
    use gpui::KeyBinding;
    let find_input = "PanelFind > PromptInput";
    let mut bindings = vec![
        KeyBinding::new("ctrl-f", OpenFind, Some("ChatPanel")),
        KeyBinding::new("ctrl-f", OpenFindIfEmpty, Some("ChatPanel > PromptInput")),
        KeyBinding::new("f3", FindNext, Some("ChatPanel")),
        KeyBinding::new("shift-f3", FindPrevious, Some("ChatPanel")),
        KeyBinding::new("enter", FindNext, Some(find_input)),
        KeyBinding::new("shift-enter", FindPrevious, Some(find_input)),
        KeyBinding::new("f3", FindNext, Some(find_input)),
        KeyBinding::new("shift-f3", FindPrevious, Some(find_input)),
        KeyBinding::new("escape", CloseFind, Some(find_input)),
        KeyBinding::new("escape", CloseFind, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-f", OpenFind, Some(find_input)),
    ];
    if cfg!(target_os = "macos") {
        bindings.extend([
            KeyBinding::new("cmd-f", OpenFind, Some("ChatPanel")),
            KeyBinding::new("cmd-f", OpenFind, Some("ChatPanel > PromptInput")),
            KeyBinding::new("cmd-g", FindNext, Some("ChatPanel")),
            KeyBinding::new("cmd-shift-g", FindPrevious, Some("ChatPanel")),
        ]);
    }
    bindings
}

pub(crate) struct FindState {
    pub(crate) input: Entity<PromptInput>,
    /// The query the matches were computed for.
    query: String,
    pub(crate) case_sensitive: bool,
    /// (leaf key, byte range) in document order.
    pub(crate) matches: Vec<(SharedString, std::ops::Range<usize>)>,
    pub(crate) current: Option<usize>,
    /// Document generation the matches belong to.
    generation: Option<u64>,
    restore_focus: Option<FocusHandle>,
    debounce: Option<Task<()>>,
    /// The current match must be scrolled into view after the next paint.
    reveal_pending: bool,
}

/// Every non-overlapping occurrence of `query` in `text`.
pub(crate) fn find_in(text: &str, query: &str, case_sensitive: bool) -> Vec<std::ops::Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    if case_sensitive || (query.is_ascii() && text.is_ascii()) {
        let lowered;
        let (hay, needle) = if case_sensitive {
            (text, query.to_owned())
        } else {
            lowered = text.to_ascii_lowercase();
            (lowered.as_str(), query.to_ascii_lowercase())
        };
        let needle = needle.as_str();
        let mut start = 0;
        while let Some(found) = hay[start..].find(needle) {
            let at = start + found;
            result.push(at..at + needle.len());
            start = at + needle.len().max(1);
        }
        return result;
    }
    // Unicode case folding can change byte lengths, so compare char by char
    // with simple lowercase mapping and report ranges in the original text.
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let mut matched = 0;
        let mut j = i;
        while matched < needle.len() && j < chars.len() {
            let folded: Vec<char> = chars[j].1.to_lowercase().collect();
            if needle.get(matched..matched + folded.len()) != Some(folded.as_slice()) {
                break;
            }
            matched += folded.len();
            j += 1;
        }
        if matched == needle.len() {
            let end = chars.get(j).map_or(text.len(), |(at, _)| *at);
            result.push(chars[i].0..end);
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    result
}

impl Panel {
    pub(crate) fn find_state(&self) -> Option<&FindState> {
        self.find.as_ref()
    }

    pub(super) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefill = self
            .transcript_selection
            .read(cx)
            .selection_text()
            .filter(|text| !text.contains('\n') && text.len() <= 200);
        if let Some(state) = &self.find {
            if let Some(text) = prefill {
                state.input.update(cx, |input, cx| input.set_content(text, cx));
            }
            let input = state.input.clone();
            input.update(cx, |input, cx| input.select_all_text(cx));
            window.focus(&input.focus_handle(cx), cx);
            self.schedule_find(cx, true);
            return;
        }
        let restore_focus = window.focused(cx);
        let change = cx.weak_entity();
        let input = cx.new(|cx| {
            let mut input = PromptInput::new(cx, "Find in transcript", |_, _, _, _| {})
                .without_command_completion()
                .without_chrome()
                .with_on_change(move |_, app| {
                    let _ = change.update(app, |panel, cx| panel.schedule_find(cx, false));
                });
            input.set_submission_enabled(false, cx);
            if let Some(text) = prefill {
                input.set_content(text, cx);
                input.select_all_text(cx);
            }
            input
        });
        window.focus(&input.focus_handle(cx), cx);
        self.find = Some(FindState {
            input,
            query: String::new(),
            case_sensitive: false,
            matches: Vec::new(),
            current: None,
            generation: None,
            restore_focus,
            debounce: None,
            reveal_pending: false,
        });
        self.schedule_find(cx, true);
        cx.notify();
    }

    pub(super) fn open_find_if_empty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.read(cx).content.is_empty() || self.find.is_some() {
            self.open_find(window, cx);
        } else {
            // A draft keeps Ctrl+F as the composer's word motion.
            cx.propagate();
        }
    }

    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.find.take() else {
            cx.propagate();
            return;
        };
        self.transcript_selection
            .update(cx, |selection, _| selection.set_find_matches(&[], None));
        match state.restore_focus {
            Some(handle) if handle != state.input.focus_handle(cx) => window.focus(&handle, cx),
            _ => self.focus_input(window, cx),
        }
        cx.notify();
    }

    pub(super) fn toggle_find_case(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.find {
            state.case_sensitive = !state.case_sensitive;
            state.generation = None;
        }
        self.schedule_find(cx, true);
    }

    /// Search again after typing pauses. `now` skips the debounce.
    fn schedule_find(&mut self, cx: &mut Context<Self>, now: bool) {
        let Some(state) = &mut self.find else { return };
        if now {
            state.debounce = None;
            state.generation = None;
            self.run_find(true, cx);
            return;
        }
        state.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |panel, cx| {
                if let Some(state) = &mut panel.find {
                    state.generation = None;
                    state.debounce = None;
                }
                panel.run_find(true, cx);
            });
        }));
    }

    /// Recompute matches if the query or document changed. `reveal` scrolls
    /// to the first match at or after the previous current one.
    fn run_find(&mut self, reveal: bool, cx: &mut Context<Self>) {
        let generation = self.transcript_selection.read(cx).document_generation();
        let Some(state) = &mut self.find else { return };
        let query = state.input.read(cx).content.to_string();
        if state.generation == Some(generation) && state.query == query {
            return;
        }
        let query_changed = state.query != query;
        let previous = state
            .current
            .and_then(|index| state.matches.get(index))
            .cloned();
        let selection = self.transcript_selection.read(cx);
        let mut matches = Vec::new();
        for (key, text) in selection.document() {
            for range in find_in(text, &query, state.case_sensitive) {
                matches.push((key.clone(), range));
            }
        }
        // Keep the current match through document growth (streaming), and
        // otherwise start from the first match.
        let current = if matches.is_empty() {
            None
        } else if !query_changed && let Some(previous) = previous {
            Some(matches.iter().position(|m| *m == previous).unwrap_or(0))
        } else {
            Some(0)
        };
        state.matches = matches;
        state.current = current;
        state.query = query;
        state.generation = Some(generation);
        state.reveal_pending |= reveal && query_changed && current.is_some();
        let (matches, current) = (state.matches.clone(), state.current);
        self.transcript_selection
            .update(cx, |selection, _| selection.set_find_matches(&matches, current));
        self.transcript_measurements.dirty = true;
        cx.notify();
    }

    pub(super) fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(state) = &mut self.find else {
            cx.propagate();
            return;
        };
        // Enter right after typing searches without waiting for the debounce.
        if state.debounce.take().is_some() {
            state.generation = None;
            self.run_find(false, cx);
        }
        let Some(state) = &mut self.find else { return };
        let count = state.matches.len();
        if count == 0 {
            return;
        }
        state.current = Some(match state.current {
            None => 0,
            Some(index) if forward => (index + 1) % count,
            Some(index) => (index + count - 1) % count,
        });
        state.reveal_pending = true;
        let (matches, current) = (state.matches.clone(), state.current);
        self.transcript_selection
            .update(cx, |selection, _| selection.set_find_matches(&matches, current));
        cx.notify();
    }

    /// Called every render: refresh matches when the transcript changed and
    /// scroll the current match into view (item first, then its line).
    pub(super) fn sync_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            return;
        }
        if self.find.as_ref().is_some_and(|state| state.debounce.is_none()) {
            self.run_find(false, cx);
        }
        let Some(state) = &mut self.find else { return };
        if !std::mem::take(&mut state.reveal_pending) {
            return;
        }
        let Some((key, _)) = state.current.and_then(|index| state.matches.get(index)) else {
            return;
        };
        let Some(row) = self.transcript_text_document.row_of(key) else {
            return;
        };
        self.cancel_transcript_momentum();
        self.release_startup_preview();
        self.stick_to_bottom = false;
        let visible = self.transcript_list.bounds_for_item(row).is_some_and(|item| {
            let viewport = self.transcript_list.viewport_bounds();
            item.top() >= viewport.top() && item.bottom() <= viewport.bottom()
        });
        if !visible {
            self.transcript_list.scroll_to(gpui::ListOffset {
                item_ix: row,
                offset_in_item: px(0.),
            });
        }
        // The match's line is only known once its leaf paints.
        cx.on_next_frame(window, |panel, window, cx| {
            panel.reveal_find_line(window, cx);
        });
        window.request_animation_frame();
    }

    fn reveal_find_line(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(bounds) = self.transcript_selection.read(cx).find_current_bounds() else {
            return;
        };
        let viewport = self.transcript_list.viewport_bounds();
        // Keep clear of the floating composer at the bottom and the find bar.
        let top = viewport.top() + px(48.);
        let bottom = viewport.bottom() - px(self.composer_dock_height + TRANSCRIPT_BOTTOM_GAP);
        if bounds.top() >= top && bounds.bottom() <= bottom.max(top + px(1.)) {
            return;
        }
        let target = viewport.top() + (viewport.size.height / 3.);
        self.transcript_list.scroll_by(bounds.top() - target);
        self.stick_to_bottom = false;
        cx.notify();
    }

    pub(super) fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let state = self.find.as_ref()?;
        let theme = Theme::global();
        let count = state.matches.len();
        let counter = match state.current {
            Some(index) if count > 0 => format!("{}/{}", index + 1, count),
            _ if state.query.is_empty() => String::new(),
            _ => "0/0".into(),
        };
        let no_results = count == 0 && !state.query.is_empty();
        let button = |id: &'static str, label: &'static str, active: bool| {
            div()
                .id(id)
                .debug_selector(move || id.to_string())
                .flex_none()
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .text_size(px(11.5))
                .font_family(theme.FONT_MONO)
                .cursor_pointer()
                .text_color(if active { theme.TEXT } else { theme.TEXT_DIM })
                .when(active, |el| el.bg(theme.SELECTION))
                .hover(|el| el.bg(theme.ACCENT_DIM).text_color(theme.TEXT))
                .child(label)
        };
        Some(
            div()
                .absolute()
                .top_2()
                .right_3()
                .left_3()
                .flex()
                .justify_end()
                .child(
                    div()
                        .id("panel-find-bar")
                        .debug_selector(|| "panel-find-bar".into())
                        .key_context(KEY_CONTEXT)
                        .occlude()
                        .flex()
                        .items_center()
                        .gap_1()
                        .w(px(360.))
                        .max_w_full()
                        .min_w_0()
                        .pl_2()
                        .pr_1()
                        .py_1()
                        .rounded_md()
                        .bg(theme.HEADER_BG)
                        .border_1()
                        .border_color(if no_results {
                            theme.ERROR
                        } else {
                            theme.PANEL_BORDER
                        })
                        .shadow_md()
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation()
                        })
                        .child(
                            div()
                                .debug_selector(|| "panel-find-input".into())
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.5))
                                .child(state.input.clone()),
                        )
                        .child(
                            div()
                                .debug_selector(|| "panel-find-count".into())
                                .flex_none()
                                .min_w(px(36.))
                                .text_size(px(11.))
                                .font_family(theme.FONT_MONO)
                                .text_color(if no_results { theme.ERROR } else { theme.TEXT_DIM })
                                .child(counter),
                        )
                        .child(
                            button("panel-find-case", "Aa", state.case_sensitive).on_click(
                                cx.listener(|panel, _, _, cx| panel.toggle_find_case(cx)),
                            ),
                        )
                        .child(
                            button("panel-find-prev", "↑", false)
                                .on_click(cx.listener(|panel, _, _, cx| panel.find_step(false, cx))),
                        )
                        .child(
                            button("panel-find-next", "↓", false)
                                .on_click(cx.listener(|panel, _, _, cx| panel.find_step(true, cx))),
                        )
                        .child(button("panel-find-close", "✕", false).on_click(cx.listener(
                            |panel, _, window, cx| panel.close_find(window, cx),
                        ))),
                )
                .into_any_element(),
        )
    }
}

/// Attach the find actions to the panel root.
pub(super) fn actions<E: InteractiveElement>(el: E, cx: &mut Context<Panel>) -> E {
    el.on_action(cx.listener(|panel, _: &OpenFind, window, cx| panel.open_find(window, cx)))
        .on_action(cx.listener(|panel, _: &OpenFindIfEmpty, window, cx| {
            panel.open_find_if_empty(window, cx)
        }))
        .on_action(cx.listener(|panel, _: &FindNext, _, cx| panel.find_step(true, cx)))
        .on_action(cx.listener(|panel, _: &FindPrevious, _, cx| panel.find_step(false, cx)))
        .on_action(cx.listener(|panel, _: &CloseFind, window, cx| panel.close_find(window, cx)))
}

#[cfg(test)]
#[path = "panel_find_tests.rs"]
mod tests;
