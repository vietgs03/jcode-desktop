use super::*;

#[test]
fn find_in_is_case_aware_and_non_overlapping() {
    assert_eq!(find_in("Foo foo FOO", "foo", false), vec![0..3, 4..7, 8..11]);
    assert_eq!(find_in("Foo foo FOO", "foo", true), vec![4..7]);
    assert_eq!(find_in("aaaa", "aa", false), vec![0..2, 2..4]);
    assert!(find_in("abc", "", false).is_empty());
    // Non-ASCII text reports byte ranges in the original string.
    let text = "Xin chào ĐÂY là đây";
    let found = find_in(text, "đây", false);
    assert_eq!(found.len(), 2);
    for range in found {
        assert_eq!(text[range].to_lowercase(), "đây");
    }
}

fn find_panel(
    cx: &mut gpui::TestAppContext,
    items: Vec<Item>,
) -> (Entity<Panel>, &mut gpui::VisualTestContext) {
    cx.update(crate::input::bind_keys);
    let (panel, vcx) = cx.add_window_view(|_, cx| {
        let mut panel = Panel::new(
            "find-test".into(),
            None,
            None,
            crate::harness::spawn_inert(),
            cx,
        );
        *panel.items = items;
        panel
    });
    let handle = vcx.update(|window, _| window.window_handle());
    vcx.simulate_window_resize(handle, gpui::size(px(700.), px(500.)));
    vcx.run_until_parked();
    (panel, vcx)
}

fn counter(panel: &Entity<Panel>, vcx: &mut gpui::VisualTestContext) -> (Option<usize>, usize) {
    panel.read_with(vcx, |panel, _| {
        let state = panel.find_state().expect("find bar open");
        (state.current, state.matches.len())
    })
}

#[gpui::test]
fn ctrl_f_finds_highlights_steps_and_closes(cx: &mut gpui::TestAppContext) {
    let mut items = Vec::new();
    for n in 0..40 {
        items.push(Item::User(format!("question {n}")));
        items.push(Item::Assistant(format!(
            "Answer {n} paragraph.\n\nfiller text for scrolling {n}"
        )));
    }
    items[3] = Item::Assistant("The needle is here and another Needle.".into());
    items[71] = Item::Assistant("Late NEEDLE near the end.\n\n```rust\nlet needle = 1;\n```".into());
    let (panel, vcx) = find_panel(cx, items);
    let composer = panel.read_with(vcx, |panel, cx| panel.input_focus_handle(cx));
    panel.update_in(vcx, |panel, window, cx| panel.focus_input(window, cx));
    vcx.run_until_parked();

    vcx.simulate_keystrokes("ctrl-f");
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("panel-find-bar").is_some(), "find bar opens");
    let find_focus = panel.read_with(vcx, |panel, cx| {
        panel.find_state().unwrap().input.focus_handle(cx)
    });
    assert!(vcx.update(|window, _| find_focus.is_focused(window)), "find input focused");

    vcx.simulate_input("needle");
    vcx.executor().advance_clock(Duration::from_millis(200));
    vcx.run_until_parked();
    // Two in item 3, two in item 71 (paragraph + code block).
    assert_eq!(counter(&panel, vcx), (Some(0), 4));
    panel.read_with(vcx, |panel, cx| {
        let selection = panel.transcript_selection.read(cx);
        let state = panel.find_state().unwrap();
        let (key, _) = &state.matches[0];
        assert!(
            selection.find_matches(key).iter().any(|(_, current)| *current),
            "current match is highlighted strongly"
        );
    });
    // Typing does not submit to the composer.
    assert!(panel.read_with(vcx, |panel, cx| panel.input.read(cx).content.is_empty()));

    let before = panel.read_with(vcx, |panel, _| panel.transcript_list.scroll_px_offset_for_scrollbar().y);
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert_eq!(counter(&panel, vcx), (Some(1), 4));
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert_eq!(counter(&panel, vcx), (Some(2), 4));
    let after = panel.read_with(vcx, |panel, _| panel.transcript_list.scroll_px_offset_for_scrollbar().y);
    assert_ne!(before, after, "the transcript scrolls to the late match");
    let (row, painted) = panel.read_with(vcx, |panel, cx| {
        let state = panel.find_state().unwrap();
        let key = &state.matches[state.current.unwrap()].0;
        (
            panel.transcript_text_document.row_of(key).unwrap(),
            panel.transcript_selection.read(cx).find_current_bounds(),
        )
    });
    let item = panel.read_with(vcx, |panel, _| panel.transcript_list.bounds_for_item(row));
    assert!(item.is_some(), "the current match's row is mounted");
    let painted = painted.expect("current match painted");
    let viewport = panel.read_with(vcx, |panel, _| panel.transcript_list.viewport_bounds());
    assert!(
        painted.top() >= viewport.top() && painted.bottom() <= viewport.bottom(),
        "current match is inside the viewport: {painted:?} vs {viewport:?}"
    );

    // Wraps both ways.
    vcx.simulate_keystrokes("enter");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    assert_eq!(counter(&panel, vcx), (Some(0), 4));
    vcx.simulate_keystrokes("shift-enter");
    vcx.run_until_parked();
    assert_eq!(counter(&panel, vcx), (Some(3), 4));
    vcx.simulate_keystrokes("f3");
    vcx.run_until_parked();
    assert_eq!(counter(&panel, vcx), (Some(0), 4));

    // Case sensitivity.
    panel.update(vcx, |panel, cx| panel.toggle_find_case(cx));
    vcx.run_until_parked();
    assert_eq!(counter(&panel, vcx).1, 2);

    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(panel.read_with(vcx, |panel, _| panel.find_state().is_none()));
    assert!(vcx.debug_bounds("panel-find-bar").is_none());
    assert!(vcx.update(|window, _| composer.is_focused(window)), "focus restored");
    panel.read_with(vcx, |panel, cx| {
        let selection = panel.transcript_selection.read(cx);
        assert!(selection.document().iter().all(|(key, _)| selection.find_matches(key).is_empty()));
    });
}

#[gpui::test]
fn ctrl_f_keeps_word_motion_while_drafting(cx: &mut gpui::TestAppContext) {
    let (panel, vcx) = find_panel(cx, vec![Item::Assistant("hello world".into())]);
    panel.update_in(vcx, |panel, window, cx| panel.focus_input(window, cx));
    vcx.simulate_input("draft text");
    vcx.run_until_parked();
    vcx.simulate_keystrokes("ctrl-f");
    vcx.run_until_parked();
    assert!(panel.read_with(vcx, |panel, _| panel.find_state().is_none()));
    assert_eq!(
        panel.read_with(vcx, |panel, cx| panel.input.read(cx).content.to_string()),
        "draft text"
    );
}

#[gpui::test]
fn find_matches_follow_streaming_document_updates(cx: &mut gpui::TestAppContext) {
    let (panel, vcx) = find_panel(cx, vec![Item::Assistant("alpha beta".into())]);
    panel.update_in(vcx, |panel, window, cx| panel.open_find(window, cx));
    vcx.simulate_input("beta");
    vcx.executor().advance_clock(Duration::from_millis(200));
    vcx.run_until_parked();
    assert_eq!(counter(&panel, vcx), (Some(0), 1));
    panel.update(vcx, |panel, cx| {
        panel.items.push(Item::Assistant("beta again".into()));
        cx.notify();
    });
    vcx.run_until_parked();
    assert_eq!(counter(&panel, vcx), (Some(0), 2));
}
