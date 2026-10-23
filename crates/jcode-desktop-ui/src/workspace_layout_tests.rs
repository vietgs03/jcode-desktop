//! Panel width bookkeeping across open/close/move, driven through the real
//! workspace actions rather than the width helpers alone.
use super::*;

fn workspace_with(panels: usize, cx: &mut gpui::TestAppContext) -> (Entity<Workspace>, &mut gpui::VisualTestContext) {
    cx.add_window_view(|_, cx| {
        let mut w = Workspace::for_test(learning::Coach::new(), cx);
        for i in 0..panels {
            w.push_test_panel(&format!("panel-{i}"), cx);
            w.slots[i].width_fraction = if panels == 1 { 1.0 } else { DEFAULT_WIDTH };
        }
        w
    })
}

fn live_widths(w: &Workspace, row: usize) -> Vec<f32> {
    w.slots
        .iter()
        .filter(|slot| slot.row == row && !slot.closing)
        .map(|slot| slot.width_fraction)
        .collect()
}

#[gpui::test]
fn closing_one_of_two_halves_gives_the_survivor_the_full_width(cx: &mut gpui::TestAppContext) {
    for closed in [0, 1] {
        let (workspace, vcx) = workspace_with(2, cx);
        vcx.update(|window, cx| {
            workspace.update(cx, |w, cx| {
                w.set_active(closed, cx);
                assert_eq!(live_widths(w, 0), vec![DEFAULT_WIDTH, DEFAULT_WIDTH]);
                w.close_panel(&ClosePanel, window, cx);
                assert_eq!(live_widths(w, 0), vec![1.0], "closed index {closed}");
                let survivor = w.slots.iter().find(|s| !s.closing).unwrap();
                assert!(survivor.restore_fraction.is_none());
            })
        });
    }
}

#[gpui::test]
fn closing_one_of_three_keeps_the_remaining_widths(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = workspace_with(3, cx);
    vcx.update(|window, cx| {
        workspace.update(cx, |w, cx| {
            w.slots[2].width_fraction = 0.75;
            w.set_active(0, cx);
            w.close_panel(&ClosePanel, window, cx);
            assert_eq!(live_widths(w, 0), vec![DEFAULT_WIDTH, 0.75]);
        })
    });
}

#[gpui::test]
fn moving_a_panel_to_another_strip_promotes_the_one_left_behind(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = workspace_with(2, cx);
    vcx.update(|window, cx| {
        workspace.update(cx, |w, cx| {
            w.set_active(1, cx);
            w.move_panel_to_row(1, window, cx);
            assert_eq!(live_widths(w, 0), vec![1.0]);
        })
    });
}

#[gpui::test]
fn a_lone_panel_can_still_be_narrowed_on_purpose(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = workspace_with(1, cx);
    vcx.update(|window, cx| {
        workspace.update(cx, |w, cx| {
            w.cycle_width(&CycleWidth, window, cx);
            assert!(live_widths(w, 0)[0] < 1.0, "manual width change sticks");
            w.maximize_width(&MaximizeWidth, window, cx);
            assert_eq!(live_widths(w, 0), vec![1.0]);
        })
    });
}

#[gpui::test]
fn reopening_after_a_close_and_closing_again_ends_full_width(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = workspace_with(2, cx);
    vcx.update(|window, cx| {
        workspace.update(cx, |w, cx| {
            w.close_panel(&ClosePanel, window, cx);
            w.remove_finished_closing_panels(Instant::now() + Duration::from_secs(5), window, cx);
            assert_eq!(w.slots.len(), 1);
            w.push_test_panel("again", cx);
            w.slots[0].width_fraction = DEFAULT_WIDTH;
            w.slots[1].width_fraction = DEFAULT_WIDTH;
            w.set_active(1, cx);
            w.close_panel(&ClosePanel, window, cx);
            assert_eq!(live_widths(w, 0), vec![1.0]);
        })
    });
}
