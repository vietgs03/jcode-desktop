//! Count real Panel renders, not a surrogate cache implementation.
use super::*;
use crate::render_stats::test_renders;

fn count(panel: &gpui::Entity<Panel>) -> usize {
    test_renders::count(panel.entity_id())
}

#[gpui::test]
fn panel_cache_reuses_transcript_on_workspace_notifications(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel("cached-panel", cx);
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    vcx.run_until_parked();
    // Settle camera and focus invalidations before measuring chrome-only work.
    for _ in 0..3 {
        workspace.update(vcx, |_, cx| cx.notify());
        vcx.run_until_parked();
    }
    let before = count(&panel);
    assert!(before > 0);
    for _ in 0..20 {
        workspace.update(vcx, |_, cx| cx.notify());
        vcx.run_until_parked();
    }
    assert_eq!(
        count(&panel),
        before,
        "chrome redraws must reuse the real transcript subtree"
    );
    panel.update(vcx, |_, cx| cx.notify());
    vcx.run_until_parked();
    assert!(
        count(&panel) > before,
        "panel notifications must invalidate the cached subtree"
    );
}

#[gpui::test]
fn panel_cache_preserves_streaming_and_resize(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel("cache-stream", cx);
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    vcx.run_until_parked();
    let before = count(&panel);
    panel.update(vcx, |panel, cx| {
        panel.apply(
            &jcode_sdk::ApiEvent::TextDelta {
                message_id: None,
                session_id: "cache-stream".into(),
                text: "A visible streaming response".into(),
            },
            cx,
        );
    });
    vcx.run_until_parked();
    assert!(
        count(&panel) > before,
        "streaming must invalidate cached content"
    );
    let before = count(&panel);
    vcx.simulate_resize(gpui::size(px(900.), px(700.)));
    vcx.run_until_parked();
    assert!(
        count(&panel) > before,
        "resizing must relayout cached content"
    );
}

#[gpui::test]
fn panel_cache_descendant_composer_notifications_remain_live(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel("cache-input", cx);
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    vcx.run_until_parked();
    vcx.update(|window, cx| window.focus(&panel.read(cx).input_focus_handle(cx), cx));
    vcx.run_until_parked();
    let before = count(&panel);
    vcx.simulate_keystrokes("cache");
    vcx.simulate_keystrokes("space");
    vcx.simulate_keystrokes("invalidation");
    vcx.run_until_parked();
    assert!(
        count(&panel) > before,
        "a descendant composer change must invalidate its cached parent"
    );
    assert_eq!(
        panel.read_with(vcx, |panel, cx| panel.snapshot(cx).draft.content),
        "cache invalidation"
    );
}

#[gpui::test]
fn typing_pauses_do_not_repaint_the_panel_while_the_caret_is_solid(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel("caret-solid", cx);
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    vcx.run_until_parked();
    vcx.update(|window, cx| window.focus(&panel.read(cx).input_focus_handle(cx), cx));
    vcx.simulate_keystrokes("h i");
    vcx.run_until_parked();
    vcx.update(|window, cx| window.simulate_next_frame(cx));
    vcx.run_until_parked();
    // A typing pause shorter than the solid caret phase. The caret is fully
    // opaque, so every composer frame here would re-render the whole chat
    // panel for nothing and compete with the next keystroke.
    let before = count(&panel);
    for _ in 0..12 {
        vcx.executor()
            .advance_clock(std::time::Duration::from_millis(34));
        vcx.run_until_parked();
    }
    assert!(
        count(&panel) - before <= 1,
        "solid caret repainted the panel {} times between keystrokes",
        count(&panel) - before
    );
    // Breathing still animates afterwards, in the composer alone: the panel
    // around it is drawn from the last frame.
    let input = panel.read_with(vcx, |panel, _| panel.input.clone());
    let before = count(&panel);
    let input_before = count_id(input.entity_id());
    for _ in 0..30 {
        vcx.executor()
            .advance_clock(std::time::Duration::from_millis(34));
        vcx.run_until_parked();
    }
    assert!(
        count_id(input.entity_id()) > input_before + 10,
        "caret breathing must keep animating"
    );
    assert!(
        count(&panel) - before <= 1,
        "caret breathing rebuilt the panel {} times",
        count(&panel) - before
    );
}

fn count_id(id: gpui::EntityId) -> usize {
    test_renders::count(id)
}

#[gpui::test]
fn focus_moves_and_new_panels_do_not_rerender_other_panels(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        for name in ["p0", "p1", "p2", "p3"] {
            workspace.push_test_panel(name, cx);
        }
        workspace
    });
    let panels: Vec<_> = (0..4)
        .map(|i| workspace.read_with(vcx, |w, _| w.test_panel(i).unwrap()))
        .collect();
    vcx.run_until_parked();
    for _ in 0..3 {
        workspace.update(vcx, |_, cx| cx.notify());
        vcx.run_until_parked();
    }
    let before: Vec<_> = panels.iter().map(count).collect();
    workspace.update_in(vcx, |w, window, cx| w.focus_left(&FocusLeft, window, cx));
    vcx.run_until_parked();
    let after: Vec<_> = panels.iter().map(count).collect();
    assert_eq!(
        after, before,
        "moving focus must reuse every panel's render"
    );
    workspace.update(vcx, |w, cx| w.push_test_panel("p4", cx));
    vcx.run_until_parked();
    let after: Vec<_> = panels.iter().map(count).collect();
    assert_eq!(after, before, "opening a panel must not rebuild the others");
}

#[gpui::test]
fn camera_pan_frames_skip_offscreen_panels(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        for name in ["p0", "p1", "p2", "p3"] {
            workspace.push_test_panel(name, cx);
        }
        workspace
    });
    let panels: Vec<_> = (0..4)
        .map(|i| workspace.read_with(vcx, |w, _| w.test_panel(i).unwrap()))
        .collect();
    vcx.run_until_parked();
    for _ in 0..3 {
        workspace.update(vcx, |_, cx| cx.notify());
        vcx.run_until_parked();
    }
    let before: Vec<_> = panels.iter().map(count).collect();
    for step in 0..10 {
        workspace.update(vcx, |w, cx| {
            let row = w.active_row;
            w.camera_target[row] = step as f32 * 13.0;
            w.camera_x[row] = step as f32 * 13.0;
            w.camera_started[row] = None;
            w.camera_dirty[row] = false;
            cx.notify();
        });
        vcx.run_until_parked();
    }
    let after: Vec<_> = panels.iter().map(count).collect();
    // Moving the camera changes every panel's bounds, so on-screen panels
    // are drawn again. Panels outside the viewport must not be built at all.
    let untouched = before.iter().zip(&after).filter(|(b, a)| b == a).count();
    assert!(
        untouched >= 1,
        "off-screen panels were rebuilt on every camera frame: {before:?} -> {after:?}"
    );
}

#[gpui::test]
fn panel_rebuilds_reuse_transcript_derived_data_until_items_change(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel("derived", cx);
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    panel.update(vcx, |panel, cx| {
        panel
            .items
            .extend((0..50).map(|n| crate::panel::Item::Assistant(format!("message {n}"))));
        cx.notify();
    });
    vcx.run_until_parked();
    let rows = |vcx: &mut gpui::VisualTestContext| {
        panel.read_with(vcx, |panel, _| panel.test_transcript_rows_ptr())
    };
    let before = rows(vcx);
    assert!(before.is_some());
    // Panel rebuilt without a transcript change: rows are reused, not rebuilt.
    for _ in 0..5 {
        panel.update(vcx, |_, cx| cx.notify());
        vcx.run_until_parked();
    }
    assert_eq!(
        rows(vcx),
        before,
        "unchanged transcript must reuse its rows"
    );
    panel.update(vcx, |panel, cx| {
        panel
            .items
            .push(crate::panel::Item::Assistant("one more".into()));
        cx.notify();
    });
    vcx.run_until_parked();
    assert_ne!(
        rows(vcx),
        before,
        "a transcript change must rebuild its rows"
    );
    assert_eq!(
        panel.read_with(vcx, |panel, _| panel.test_transcript_row_count()),
        51
    );
}

#[gpui::test]
fn large_transcript_stream_keeps_one_live_row_and_reuses_settled_cache(
    cx: &mut gpui::TestAppContext,
) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel("large-cache-stream", cx);
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    panel.update(vcx, |panel, cx| {
        panel.items.clear();
        panel.items.extend(
            (0..4_800).map(|n| crate::panel::Item::Assistant(format!("Settled message {n}"))),
        );
        cx.notify();
    });
    vcx.run_until_parked();
    assert_eq!(
        panel.read_with(vcx, |p, _| p.test_transcript_row_count()),
        4_800
    );
    for index in 0..200 {
        panel.update(vcx, |panel, cx| {
            panel.apply(
                &jcode_sdk::ApiEvent::TextDelta {
                    message_id: None,
                    session_id: "large-cache-stream".into(),
                    text: format!("chunk {index} "),
                },
                cx,
            );
        });
        vcx.run_until_parked();
        assert_eq!(panel.read_with(vcx, |p, _| p.items.len()), 4_800);
        assert_eq!(
            panel.read_with(vcx, |p, _| p.test_transcript_row_count()),
            4_801
        );
        if index % 25 == 0 {
            let before = panel.read_with(vcx, |p, _| p.test_transcript_rows_ptr());
            panel.update(vcx, |_, cx| cx.notify());
            vcx.run_until_parked();
            assert_eq!(
                panel.read_with(vcx, |p, _| p.test_transcript_rows_ptr()),
                before
            );
        }
    }
    panel.update(vcx, |panel, cx| {
        panel.apply(
            &jcode_sdk::ApiEvent::TurnDone {
                session_id: "large-cache-stream".into(),
                pending_soft_interrupts: None,
            },
            cx,
        );
    });
    vcx.run_until_parked();
    assert_eq!(
        panel.read_with(vcx, |p, _| p.test_transcript_row_count()),
        4_801
    );
    // The current derived row set must follow replacement. Release of every
    // old allocation is a separate memory-instrumented check.
    panel.update(vcx, |panel, cx| {
        panel.items.clear();
        panel
            .items
            .push(crate::panel::Item::Assistant("replacement".into()));
        cx.notify();
    });
    vcx.run_until_parked();
    assert_eq!(
        panel.read_with(vcx, |p, _| p.test_transcript_row_count()),
        1
    );
}

/// Renders of `name` recorded by `render_stats` on this test's thread.
fn view_renders(name: &str) -> u64 {
    crate::render_stats::snapshot()
        .into_iter()
        .find(|(view, _)| *view == name)
        .map_or(0, |(_, stat)| stat.renders)
}

#[gpui::test]
fn working_session_animation_ticks_do_not_rebuild_workspace_or_panel(
    cx: &mut gpui::TestAppContext,
) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel("working-ticks", cx);
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    panel.update(vcx, |panel, cx| {
        panel.status = "running_tools".into();
        cx.notify();
    });
    vcx.run_until_parked();
    for _ in 0..3 {
        vcx.executor()
            .advance_clock(std::time::Duration::from_millis(34));
        vcx.run_until_parked();
    }
    let workspace_before = view_renders("Workspace");
    let panel_before = view_renders("Panel");
    let ring_before = view_renders("TabOutline");
    let spinner_before = view_renders("Spinner");
    for _ in 0..30 {
        vcx.executor()
            .advance_clock(std::time::Duration::from_millis(17));
        vcx.run_until_parked();
    }
    let workspace = view_renders("Workspace") - workspace_before;
    let panel = view_renders("Panel") - panel_before;
    let ring = view_renders("TabOutline") - ring_before;
    let spinner = view_renders("Spinner") - spinner_before;
    eprintln!("workspace={workspace} panel={panel} ring={ring} spinner={spinner}");
    assert!(
        ring + spinner > 10,
        "working indicators must keep animating (ring {ring}, spinner {spinner})"
    );
    assert!(
        workspace <= 2 && panel <= 2,
        "decorative ticks rebuilt Workspace {workspace} and Panel {panel} times in 30 frames"
    );
}

#[gpui::test]
fn running_tool_row_pulses_do_not_rebuild_the_panel(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel("running-tool", cx);
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    panel.update(vcx, |panel, cx| {
        panel.apply(
            &jcode_sdk::ApiEvent::ToolStart {
                session_id: "running-tool".into(),
                call_id: "call-1".into(),
                name: "bash".into(),
            },
            cx,
        );
    });
    vcx.run_until_parked();
    for _ in 0..3 {
        vcx.executor()
            .advance_clock(std::time::Duration::from_millis(50));
        vcx.run_until_parked();
    }
    let workspace_before = view_renders("Workspace");
    let panel_before = view_renders("Panel");
    let ticker_before = view_renders("Ticker");
    for _ in 0..20 {
        vcx.executor()
            .advance_clock(std::time::Duration::from_millis(50));
        vcx.run_until_parked();
    }
    let workspace = view_renders("Workspace") - workspace_before;
    let panel = view_renders("Panel") - panel_before;
    let ticker = view_renders("Ticker") - ticker_before;
    eprintln!("running tool: workspace={workspace} panel={panel} ticker={ticker}");
    assert!(
        ticker >= 15,
        "the running tool's pulse and clock must keep animating ({ticker} ticks)"
    );
    assert!(
        workspace <= 2 && panel <= 4,
        "a running tool row rebuilt Workspace {workspace} and Panel {panel} times in one second"
    );
}

/// Per-view and per-phase milliseconds since `before`, busiest first.
fn profile_breakdown(before: &[(&'static str, crate::render_stats::ViewStat)]) -> String {
    crate::render_stats::delta(before, &crate::render_stats::snapshot())
        .iter()
        .map(|(name, stat)| format!("{name}={}x/{:.1}ms", stat.renders, stat.nanos as f64 / 1e6))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Profile the real "open an old WSL session" frame: a sidebar listing the
/// user's WSL sessions and a panel that just received a large history.
/// Fixtures come from `JCODE_PROFILE_DIR` (`sessions.json` from `list_sessions`,
/// `history.json` from `get_history`). Run with
/// `open_session_frame_profile -- --ignored --nocapture`.
#[gpui::test]
#[ignore = "manual profiler over captured fixtures"]
fn open_session_frame_profile(cx: &mut gpui::TestAppContext) {
    let dir = std::path::PathBuf::from(std::env::var("JCODE_PROFILE_DIR").unwrap_or_else(|_| {
        // The Windows checkout's sibling `profile` directory.
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../../profile").into()
    }));
    let sessions: Vec<jcode_sdk::SessionInfo> =
        serde_json::from_slice(&std::fs::read(dir.join("sessions.json")).unwrap()).unwrap();
    let sessions: Vec<_> = sessions
        .into_iter()
        .map(|mut session| {
            session.session_id = format!("ssh://wsl/{}", session.session_id);
            session.parent_session_id = session
                .parent_session_id
                .map(|id| format!("ssh://wsl/{id}"));
            session
        })
        .collect();
    #[derive(serde::Deserialize)]
    struct History {
        messages: Vec<jcode_sdk::HistoryMessage>,
    }
    let history: History =
        serde_json::from_slice(&std::fs::read(dir.join("history.json")).unwrap()).unwrap();
    let opened = sessions[0].session_id.clone();
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel(&opened, cx);
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    let before = crate::render_stats::snapshot();
    let started = std::time::Instant::now();
    workspace.update(vcx, |w, cx| {
        w.apply(Update::Sessions { sessions }, cx);
        cx.notify();
    });
    vcx.run_until_parked();
    println!(
        "PROFILE sessions_applied_frame={:?} {}",
        started.elapsed(),
        profile_breakdown(&before)
    );
    let before = crate::render_stats::snapshot();
    let started = std::time::Instant::now();
    panel.update(vcx, |panel, cx| {
        panel.load_history(history.messages, Vec::new(), cx);
    });
    let load_ms = started.elapsed();
    vcx.run_until_parked();
    println!(
        "PROFILE load_history={load_ms:?} first_frame_total={:?} {}",
        started.elapsed(),
        profile_breakdown(&before)
    );
    for label in ["workspace", "panel"] {
        let before = crate::render_stats::snapshot();
        let mut samples = Vec::new();
        for _ in 0..20 {
            let started = std::time::Instant::now();
            if label == "workspace" {
                workspace.update(vcx, |_, cx| cx.notify());
            } else {
                panel.update(vcx, |_, cx| cx.notify());
            }
            vcx.run_until_parked();
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "PROFILE repaint_{label} p50={:.2}ms p95={:.2}ms max={:.2}ms {}",
            samples[10],
            samples[18],
            samples[19],
            profile_breakdown(&before)
        );
    }
}

/// Streamed deltas reach the panel through `Workspace::apply`, the path every
/// bridge update takes. GPUI rebuilds the Workspace around a notified panel,
/// so the sidebar model (every listed session grouped by project, hundreds of
/// rows for a long WSL history) must come from its memo rather than be built
/// again once per token.
#[gpui::test]
fn streamed_deltas_reuse_the_sidebar_model(cx: &mut gpui::TestAppContext) {
    let session_id = "delta-stream";
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel(session_id, cx);
        let mut sessions = (0..300)
            .map(|n| super::tests::session_info(&format!("session_other_{n}"), Some("other")))
            .collect::<Vec<_>>();
        sessions.push(super::tests::session_info(session_id, Some("streaming")));
        workspace.sessions = sessions;
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    let send = |vcx: &mut gpui::VisualTestContext, event: jcode_sdk::ApiEvent| {
        workspace.update(vcx, |workspace, cx| {
            let session_id = session_id.to_string();
            if workspace.apply(Update::Event { session_id, event }, cx) {
                cx.notify();
            }
        });
        vcx.run_until_parked();
    };
    send(
        vcx,
        jcode_sdk::ApiEvent::SessionStatus {
            session_id: session_id.into(),
            status: "processing".into(),
            pending_soft_interrupts: None,
        },
    );
    let builds_before = view_renders("Workspace.sidebar.model");
    let sidebar_before = view_renders("Workspace.sidebar");
    let panel_before = count(&panel);
    let started = std::time::Instant::now();
    for index in 0..100 {
        send(
            vcx,
            jcode_sdk::ApiEvent::TextDelta {
                message_id: None,
                session_id: session_id.into(),
                text: format!("token {index} "),
            },
        );
    }
    let elapsed = started.elapsed();
    let builds = view_renders("Workspace.sidebar.model") - builds_before;
    let sidebars = view_renders("Workspace.sidebar") - sidebar_before;
    let panel_renders = count(&panel) - panel_before;
    eprintln!(
        "100 deltas in {elapsed:?}: sidebar renders={sidebars} model builds={builds} panel={panel_renders}"
    );
    assert!(
        panel_renders >= 50,
        "the panel must paint the stream ({panel_renders})"
    );
    // The recency bump is throttled to once a second, so a burst rebuilds the
    // model about once per second of wall time (debug Windows runs take
    // several), never once per delta.
    let allowed = elapsed.as_secs() + 2;
    assert!(
        builds <= allowed,
        "100 streamed deltas in {elapsed:?} built the sidebar model {builds} times (allowed {allowed})"
    );
    assert!(
        panel.read_with(vcx, |p, _| p.sidebar_activity().is_some()),
        "the sidebar spinner still reports the streaming panel"
    );
}

/// The memo must not hide real changes: a new session list, a renamed panel
/// or a folded project rebuilds the rows.
#[gpui::test]
fn sidebar_model_rebuilds_when_its_inputs_change(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel("memo-open", cx);
        workspace.sessions = vec![super::tests::session_info("session_memo_1", Some("first"))];
        workspace
    });
    vcx.run_until_parked();
    let rows = |vcx: &mut gpui::VisualTestContext| {
        workspace.read_with(vcx, |w, _| {
            w.sidebar_model
                .as_ref()
                .map(|(_, model, _)| {
                    model
                        .ordered_sessions
                        .iter()
                        .map(|row| row.session.session_id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    };
    assert!(rows(vcx).contains(&"session_memo_1".to_string()));
    let builds = view_renders("Workspace.sidebar.model");
    workspace.update(vcx, |_, cx| cx.notify());
    vcx.run_until_parked();
    assert_eq!(
        view_renders("Workspace.sidebar.model"),
        builds,
        "an unchanged sidebar reuses its rows"
    );
    workspace.update(vcx, |w, cx| {
        w.apply(
            Update::Sessions {
                sessions: vec![
                    super::tests::session_info("session_memo_1", Some("first")),
                    super::tests::session_info("session_memo_2", Some("second")),
                ],
            },
            cx,
        );
        cx.notify();
    });
    vcx.run_until_parked();
    assert!(rows(vcx).contains(&"session_memo_2".to_string()));
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    let before = view_renders("Workspace.sidebar.model");
    panel.update(vcx, |panel, cx| {
        panel.title = "renamed".into();
        cx.notify();
    });
    vcx.run_until_parked();
    assert!(
        view_renders("Workspace.sidebar.model") > before,
        "a renamed panel rebuilds its sidebar row"
    );
}
