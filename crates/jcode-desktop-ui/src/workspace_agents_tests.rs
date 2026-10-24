//! Agents board: model, header strip, board rows and navigation.
use super::*;
use crate::harness::swarm_state::{Roster, SwarmMember};

const LEAD: &str = "ssh://wsl/session_calf_1792418161586_1c56f91f73f8ba66";
const BOAR: &str = "ssh://wsl/session_boar_1792769349475_c52fdd3939f335c6";
const CAMEL: &str = "ssh://wsl/session_camel_1792768582232_8c2cff24d9e9301a";
const DINGO: &str = "ssh://wsl/session_dingo_1792769000000_0000000000000001";
const EAGLE: &str = "ssh://wsl/session_eagle_1792769100000_0000000000000002";

fn member(id: &str, parent: Option<&str>, status: &str) -> SwarmMember {
    SwarmMember {
        session_id: id.into(),
        parent_session_id: parent.map(str::to_owned),
        status: status.into(),
        updated_at_ms: Some(u64::try_from(unix_now_ms()).unwrap() - 120_000),
        ..Default::default()
    }
}

/// A WSL coordinator with four agents: working, done, failed, ready.
fn roster() -> Roster {
    let mut lead = member(LEAD, None, "running");
    lead.role = Some("coordinator".into());
    lead.friendly_name = Some("calf".into());
    lead.detail = Some("Swarm await finished".into());
    let mut boar = member(BOAR, Some(LEAD), "running");
    boar.task_label = Some("pvp".into());
    boar.friendly_name = Some("boar".into());
    boar.detail = Some("Running   the PvP\n e2e suite against the private server".into());
    let mut camel = member(CAMEL, Some(LEAD), "completed");
    camel.task_label = Some("e2e-events".into());
    camel.friendly_name = Some("camel".into());
    camel.detail = Some("DM from calf: good job".into());
    camel.latest_completion_report =
        Some("## Summary\n**191/191** checks pass on a private server.\n| a | b |".into());
    camel.terminal_since_ms = Some(u64::try_from(unix_now_ms()).unwrap() - 3_600_000);
    let mut dingo = member(DINGO, Some(LEAD), "failed");
    dingo.task_label = Some("migration".into());
    dingo.detail = Some("cargo test failed: 3 errors".into());
    let mut eagle = member(EAGLE, Some(LEAD), "ready");
    eagle.task_label = Some("docs".into());
    [lead, boar, camel, dingo, eagle]
        .into_iter()
        .map(|m| (m.session_id.clone(), m))
        .collect()
}

fn sessions() -> Vec<jcode_sdk::SessionInfo> {
    let mut sessions: Vec<_> = [LEAD, BOAR, CAMEL, DINGO, EAGLE]
        .into_iter()
        .map(|id| super::super::tests::session_info(id, Some(id)))
        .collect();
    let roster = roster();
    crate::harness::swarm_state::enrich_sessions(&mut sessions, &roster);
    sessions
}

#[test]
fn agents_board_counts_orders_and_describes_each_agent() {
    let roster = roster();
    let sessions = sessions();
    let board = Index::new(&roster, &sessions).board(LEAD);
    assert_eq!(
        board.summary(),
        "4 agents · 1 working · 1 ready · 1 done · 1 failed"
    );
    // Failed first, then live work, then idle, then finished.
    let labels: Vec<_> = board.rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["migration", "pvp", "docs", "e2e-events"]);
    let pvp = &board.rows[1];
    assert_eq!(pvp.name.as_deref(), Some("boar"));
    assert_eq!(
        pvp.activity.as_deref(),
        Some("Running the PvP e2e suite against the private server")
    );
    // The current detail wins over a previous completion report.
    assert_eq!(
        board.rows[3].activity.as_deref(),
        Some("DM from calf: good job")
    );
    assert!(board.rows[3].detail.contains("191/191 checks pass"));
    // The coordinator is not its own agent, and agents have no board.
    assert!(board.rows.iter().all(|r| r.session_id != LEAD));
    assert!(Index::new(&roster, &sessions).board(BOAR).is_empty());
    assert!(Index::new(&roster, &sessions).has_agents(LEAD));
}

#[test]
fn agents_activity_uses_report_only_as_a_fallback_and_hover_keeps_full_detail() {
    assert_eq!(
        first_meaningful_line("## Summary\n\n**191/191** checks pass.\n| a | b |"),
        Some("191/191 checks pass.".into())
    );
    let mut agent = member("boar", Some("lead"), "ready");
    agent.latest_completion_report = Some("## Done\nOld task finished".into());
    agent.detail = Some(format!("Waiting for next task {}", "detail ".repeat(150)));
    let row = row_from("boar", Some(&agent), None, 1);
    assert!(row.activity.as_deref().unwrap().starts_with("Waiting for next task"));
    assert!(row.detail.contains(&collapse(agent.detail.as_deref().unwrap())));
    agent.detail = None;
    let row = row_from("boar", Some(&agent), None, 1);
    assert_eq!(row.activity.as_deref(), Some("Old task finished"));
}

#[test]
fn agents_board_keeps_unknown_statuses_and_survives_cycles() {
    let mut roster = Roster::new();
    let mut a = member("a", Some("b"), "hibernating");
    a.task_label = Some("odd".into());
    roster.insert("a".into(), a);
    roster.insert("b".into(), member("b", Some("a"), "running"));
    let board = Index::new(&roster, &[]).board("a");
    assert_eq!(board.rows.len(), 1);
    assert_eq!(board.rows[0].session_id, "b");
    let board = Index::new(&roster, &[]).board("b");
    assert_eq!(board.rows[0].state_text(), "hibernating");
    assert!(board.summary().contains("1 other"));
}

#[test]
fn agents_board_falls_back_to_catalog_ownership_without_a_roster() {
    let mut child = super::super::tests::session_info("child", Some("Child"));
    child.parent_session_id = Some("root".into());
    child.agent_label = Some("Verify".into());
    child.swarm_status = Some("running".into());
    let sessions = vec![super::super::tests::session_info("root", None), child];
    let roster = Roster::new();
    let board = Index::new(&roster, &sessions).board("root");
    assert_eq!(board.rows.len(), 1);
    assert_eq!(board.rows[0].label, "Verify");
    assert_eq!(board.rows[0].state, AgentState::Working);
}

#[test]
fn agents_refresh_after_swarm_tool_calls_only() {
    let done = |name: &str| jcode_sdk::ApiEvent::ToolDone {
        session_id: "s".into(),
        call_id: "c".into(),
        name: name.into(),
        output: String::new(),
        error: None,
    };
    assert!(refresh_after(&done("swarm")));
    assert!(refresh_after(&done("functions.communicate")));
    assert!(!refresh_after(&done("bash")));
}

#[test]
fn sidebar_swarm_activity_line_is_for_agents_only() {
    let roster = roster();
    assert_eq!(
        sidebar_swarm::activity(&roster, BOAR).as_deref(),
        Some("working · Running the PvP e2e suite against the private server")
    );
    assert_eq!(
        sidebar_swarm::activity(&roster, EAGLE).as_deref(),
        Some("ready")
    );
    assert_eq!(sidebar_swarm::activity(&roster, LEAD), None);
    assert_eq!(sidebar_swarm::activity(&roster, "unknown"), None);
}

fn setup(
    cx: &mut gpui::TestAppContext,
) -> (
    Entity<Workspace>,
    &mut gpui::VisualTestContext,
    std::sync::mpsc::Receiver<Command>,
) {
    let (bridge, commands) = harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut w = Workspace::for_test(learning::Coach::new(), cx);
        w.set_test_bridge(bridge);
        w.push_test_panel(LEAD, cx);
        w
    });
    workspace.update(vcx, |w, cx| {
        w.apply(
            Update::Sessions {
                sessions: sessions(),
            },
            cx,
        );
        w.apply(Update::SwarmState { roster: roster() }, cx);
        // Start folded: these tests drive the fold explicitly.
        w.agents_collapsed.insert(LEAD.into());
        cx.notify();
    });
    vcx.run_until_parked();
    while commands.try_recv().is_ok() {}
    (workspace, vcx, commands)
}

#[gpui::test]
fn agents_board_unfolds_by_default_while_agents_work(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx, _commands) = setup(cx);
    workspace.update(vcx, |w, cx| {
        w.agents_collapsed.clear();
        cx.notify();
    });
    vcx.run_until_parked();
    assert!(shown(vcx, "agents-board"), "working agents unfold the board");
    click(vcx, "agents-strip");
    assert!(!shown(vcx, "agents-board"), "the user's fold sticks");
    workspace.read_with(vcx, |w, _| assert!(w.agents_collapsed.contains(LEAD)));
}

fn shown(vcx: &mut gpui::VisualTestContext, selector: &str) -> bool {
    vcx.debug_bounds(selector.to_owned().leak()).is_some()
}

/// Let Instant-driven tweens (camera pans) finish before reading bounds.
fn settle(workspace: &Entity<Workspace>, vcx: &mut gpui::VisualTestContext) {
    std::thread::sleep(CAMERA_DURATION + std::time::Duration::from_millis(32));
    workspace.update(vcx, |_, cx| cx.notify());
    vcx.run_until_parked();
}

fn click(vcx: &mut gpui::VisualTestContext, selector: &str) {
    let bounds = vcx
        .debug_bounds(selector.to_owned().leak())
        .unwrap_or_else(|| panic!("{selector} not rendered"));
    // Hover first, as a pointer would: hover can reveal row actions that
    // reflow the target between mouse down and up.
    vcx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::default());
    vcx.run_until_parked();
    let bounds = vcx
        .debug_bounds(selector.to_owned().leak())
        .unwrap_or_else(|| panic!("{selector} not rendered after hover"));
    vcx.simulate_click(bounds.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
}

#[gpui::test]
fn agents_strip_shows_counts_and_board_rows_open_agents(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx, commands) = setup(cx);
    // Collapsed: one calm strip with per-state counts, no board.
    assert!(shown(vcx, "agents-strip"));
    for state in ["working", "ready", "done", "failed"] {
        assert!(
            shown(vcx, &format!("agents-count-{state}")),
            "{state} count"
        );
    }
    assert!(!shown(vcx, "agents-count-blocked"));
    assert!(!shown(vcx, "agents-board"));
    let summary = workspace.read_with(vcx, |w, _| w.agents_board(LEAD).summary());
    assert_eq!(
        summary,
        "4 agents · 1 working · 1 ready · 1 done · 1 failed"
    );

    // The strip sits above the chat, inside the coordinator's panel.
    let strip = vcx.debug_bounds("agents-strip").unwrap();
    let panel = vcx.debug_bounds("panel-0").unwrap();
    assert!(panel.contains(&strip.center()));
    assert!(strip.origin.y - panel.origin.y < px(4.0));

    // Expanding shows one row per agent with label and activity.
    click(vcx, "agents-strip");
    assert!(shown(vcx, "agents-board"));
    for index in 0..4 {
        assert!(shown(vcx, &format!("agents-row-{index}")), "row {index}");
        assert!(shown(vcx, &format!("agents-row-label-{index}")));
    }
    assert!(!shown(vcx, "agents-row-4"));
    for index in 0..4 {
        assert!(
            shown(vcx, &format!("agents-row-activity-{index}")) == (index != 2),
            "activity line for row {index}"
        );
    }
    let rows = workspace.read_with(vcx, |w, _| w.agents_board(LEAD).rows);
    assert_eq!(rows[1].session_id, BOAR);
    // Rows stack top to bottom in board order.
    let first = vcx.debug_bounds("agents-row-0").unwrap();
    let second = vcx.debug_bounds("agents-row-1").unwrap();
    assert!(second.origin.y > first.origin.y);
    assert!(commands.try_recv().is_err(), "expanding sends nothing");

    // Clicking a row opens that agent's conversation through Watch.
    click(vcx, "agents-row-1");
    let watched: Vec<String> = std::iter::from_fn(|| commands.try_recv().ok())
        .filter_map(|command| match command {
            Command::Watch { session_id } => Some(session_id),
            _ => None,
        })
        .collect();
    assert_eq!(watched, [BOAR.to_string()]);
    let (open, active) = workspace.read_with(vcx, |w, cx| {
        let open = w
            .slots
            .iter()
            .filter(|slot| slot.panel.read(cx).session_id == BOAR)
            .count();
        (open, w.slots[w.active].panel.read(cx).session_id.clone())
    });
    assert_eq!(open, 1);
    assert_eq!(active, BOAR);
    // The agent has no agents of its own, so it gets no strip.
    let boar_slot = workspace.read_with(vcx, |w, cx| {
        w.slots
            .iter()
            .position(|slot| slot.panel.read(cx).session_id == BOAR)
            .unwrap()
    });
    let boar_panel = vcx
        .debug_bounds(format!("panel-{boar_slot}").leak())
        .unwrap();
    if let Some(strip) = vcx.debug_bounds("agents-strip") {
        assert!(!boar_panel.contains(&strip.center()));
    }

    // Clicking it again focuses the same panel without a second Watch.
    workspace.update_in(vcx, |w, window, cx| {
        let lead = w
            .slots
            .iter()
            .position(|slot| slot.panel.read(cx).session_id == LEAD)
            .unwrap();
        w.set_active(lead, cx);
        w.focus_active(window, cx);
        cx.notify();
    });
    vcx.run_until_parked();
    // Windows opens chats full width, so the camera pans back to the lead.
    settle(&workspace, vcx);
    while commands.try_recv().is_ok() {}
    click(vcx, "agents-row-1");
    assert!(
        std::iter::from_fn(|| commands.try_recv().ok())
            .all(|command| !matches!(command, Command::Watch { .. })),
        "focusing an open agent reuses its panel"
    );
    let (count, active) = workspace.read_with(vcx, |w, cx| {
        (
            w.slots
                .iter()
                .filter(|slot| slot.panel.read(cx).session_id == BOAR)
                .count(),
            w.slots[w.active].panel.read(cx).session_id.clone(),
        )
    });
    assert_eq!((count, active.as_str()), (1, BOAR));
}

#[gpui::test]
fn agents_strip_follows_roster_refreshes(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx, _commands) = setup(cx);
    assert!(shown(vcx, "agents-count-working"));
    let mut next = roster();
    next.get_mut(BOAR).unwrap().status = "completed".into();
    let changed = workspace.update(vcx, |w, cx| {
        let changed = w.apply(
            Update::SwarmState {
                roster: next.clone(),
            },
            cx,
        );
        cx.notify();
        changed
    });
    assert!(changed);
    vcx.run_until_parked();
    assert!(!shown(vcx, "agents-count-working"));
    assert!(shown(vcx, "agents-count-done"));
    // An identical roster is not a visible change.
    let changed = workspace.update(vcx, |w, cx| {
        w.apply(Update::SwarmState { roster: next }, cx)
    });
    assert!(!changed);
}

#[gpui::test]
fn agents_sidebar_badge_opens_the_coordinator_board(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx, _commands) = setup(cx);
    let lead_row = workspace.read_with(vcx, |w, _| {
        w.sidebar_session_layout
            .iter()
            .position(|row| row.session_id == LEAD)
            .expect("coordinator row")
    });
    // Agents nest under the coordinator, not as separate rows.
    let ids = workspace.read_with(vcx, |w, _| {
        w.sidebar_session_layout
            .iter()
            .map(|row| row.session_id.clone())
            .collect::<Vec<_>>()
    });
    assert!(!ids.iter().any(|id| id == BOAR));
    assert!(!shown(vcx, "agents-board"));
    click(vcx, &format!("sidebar-agents-{lead_row}"));
    assert!(shown(vcx, "agents-board"));
    workspace.read_with(vcx, |w, cx| {
        assert_eq!(w.slots[w.active].panel.read(cx).session_id, LEAD);
        assert!(w.agents_expanded.contains(LEAD));
    });
}
