//! Swarm view: grouping, counts, filter, navigation and entry points.
use super::*;
use crate::workspace::agents::AgentState;
use crate::harness::swarm_state::{Roster, SwarmMember};

const LEAD: &str = "ssh://wsl/session_calf_1792418161586_1c56f91f73f8ba66";
const BOAR: &str = "ssh://wsl/session_boar_1792769349475_c52fdd3939f335c6";
const CAMEL: &str = "ssh://wsl/session_camel_1792768582232_8c2cff24d9e9301a";
const DINGO: &str = "ssh://wsl/session_dingo_1792769000000_0000000000000001";
const EAGLE: &str = "ssh://wsl/session_eagle_1792769100000_0000000000000002";
const OLD_LEAD: &str = "session_fox_1792000000000_00000000000000aa";
const OLD_AGENT: &str = "session_goat_1792000000001_00000000000000bb";

fn now() -> u64 {
    u64::try_from(unix_now_ms()).unwrap()
}

fn member(id: &str, swarm: &str, parent: Option<&str>, status: &str, age_ms: u64) -> SwarmMember {
    SwarmMember {
        session_id: id.into(),
        swarm_id: Some(swarm.into()),
        parent_session_id: parent.map(str::to_owned),
        status: status.into(),
        updated_at_ms: Some(now() - age_ms),
        role: Some(if parent.is_some() { "agent" } else { "coordinator" }.into()),
        ..Default::default()
    }
}

/// A live WSL swarm (coordinator + 4 agents) and a finished local swarm.
fn roster() -> Roster {
    let mut lead = member(LEAD, "wsl-swarm", None, "running", 30_000);
    lead.friendly_name = Some("calf".into());
    lead.task_label = Some("desktop polish".into());
    lead.working_dir = Some("/home/viethx/jcode-desktop-test".into());
    let mut boar = member(BOAR, "wsl-swarm", Some(LEAD), "running", 30_000);
    boar.friendly_name = Some("boar".into());
    boar.task_label = Some("swarm-view".into());
    boar.detail = Some("Building   the swarm\n view".into());
    let mut camel = member(CAMEL, "wsl-swarm", Some(LEAD), "completed", 60_000);
    camel.friendly_name = Some("camel".into());
    camel.task_label = Some("markdown".into());
    camel.latest_completion_report = Some("## Summary\n**All** tables render.\nMore detail".into());
    camel.is_headless = true;
    let mut dingo = member(DINGO, "wsl-swarm", Some(LEAD), "failed", 90_000);
    dingo.friendly_name = Some("dingo".into());
    dingo.detail = Some("cargo test failed: 3 errors".into());
    let mut eagle = member(EAGLE, "wsl-swarm", Some(LEAD), "ready", 120_000);
    eagle.friendly_name = Some("eagle".into());
    let mut fox = member(OLD_LEAD, "local-swarm", None, "ready", 3_600_000);
    fox.friendly_name = Some("fox".into());
    fox.working_dir = Some("/tmp/project".into());
    let mut goat = member(OLD_AGENT, "local-swarm", Some(OLD_LEAD), "completed", 3_600_000);
    goat.friendly_name = Some("goat".into());
    [lead, boar, camel, dingo, eagle, fox, goat]
        .into_iter()
        .map(|m| (m.session_id.clone(), m))
        .collect()
}

#[test]
fn swarm_view_groups_active_first_with_counts() {
    let groups = swarm_view::groups(&roster(), now());
    assert_eq!(groups.len(), 2);
    let live = &groups[0];
    assert_eq!(live.key, "wsl-swarm");
    assert_eq!(live.title(), "calf · desktop polish");
    assert_eq!(live.counts_text(), "1 running · 1 ready · 1 done · 1 failed");
    assert_eq!(
        live.working_dir.as_deref(),
        Some("wsl · /home/viethx/jcode-desktop-test")
    );
    let names: Vec<_> = live.members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["dingo", "boar", "eagle", "camel"]);
    assert_eq!(live.members[1].activity.as_deref(), Some("Building the swarm view"));
    assert!(live.members[3].headless);
    assert_eq!(groups[1].counts_text(), "1 done");
    assert!(!groups[1].is_active());
    assert_eq!(swarm_view::running_count(&roster(), now()), 1);
    // Stale swarms drop out of the view.
    assert!(swarm_view::groups(&roster(), now() + 2 * swarm_view::RECENT_MS).is_empty());
}

#[test]
fn swarm_view_slash_command_parses() {
    use crate::panel::workspace_commands::parse::{WorkspaceCommand, parse};
    assert_eq!(parse("/swarms"), Some(WorkspaceCommand::Swarms));
    assert_eq!(parse("/agents"), Some(WorkspaceCommand::Swarms));
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
        w.apply(Update::SwarmState { roster: roster() }, cx);
        cx.notify();
    });
    vcx.run_until_parked();
    while commands.try_recv().is_ok() {}
    (workspace, vcx, commands)
}

fn shown(vcx: &mut gpui::VisualTestContext, selector: &str) -> bool {
    vcx.debug_bounds(selector.to_owned().leak()).is_some()
}

fn click(vcx: &mut gpui::VisualTestContext, selector: &str) {
    let bounds = vcx
        .debug_bounds(selector.to_owned().leak())
        .unwrap_or_else(|| panic!("{selector} not rendered"));
    vcx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::default());
    vcx.run_until_parked();
    let bounds = vcx
        .debug_bounds(selector.to_owned().leak())
        .unwrap_or_else(|| panic!("{selector} not rendered after hover"));
    vcx.simulate_click(bounds.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
}

fn open(workspace: &Entity<Workspace>, vcx: &mut gpui::VisualTestContext) {
    workspace.update(vcx, |w, cx| w.open_swarm_view(cx));
    vcx.run_until_parked();
}

#[gpui::test]
fn swarm_view_renders_groups_rows_and_filter(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx, commands) = setup(cx);
    assert!(!shown(vcx, "swarm-view"));
    open(&workspace, vcx);
    assert!(shown(vcx, "swarm-view"));
    // Opening asks for one refresh, not a poll.
    let refreshes = std::iter::from_fn(|| commands.try_recv().ok())
        .filter(|c| matches!(c, Command::RefreshSessions))
        .count();
    assert_eq!(refreshes, 1);
    assert!(shown(vcx, "swarm-view-running"));
    assert!(shown(vcx, "swarm-view-updated"));
    assert!(shown(vcx, "swarm-group-0") && shown(vcx, "swarm-group-1"));
    assert!(shown(vcx, "swarm-counts-0"));
    for mi in 0..4 {
        assert!(shown(vcx, &format!("swarm-member-0-{mi}")), "member {mi}");
    }
    assert!(!shown(vcx, "swarm-member-0-4"));
    assert!(shown(vcx, "swarm-member-activity-0-1"));
    assert!(!shown(vcx, "swarm-member-activity-0-2"));
    // Finished member shows its report summary and expands on click.
    assert!(shown(vcx, "swarm-member-report-0-3"));
    click(vcx, "swarm-member-report-0-3");
    workspace.read_with(vcx, |w, _| {
        let view = w.swarm_view.as_ref().expect("still open");
        assert!(view.expanded_reports.contains(CAMEL));
    });
    // Running filter hides the finished swarm and non-running agents.
    click(vcx, "swarm-view-filter-running");
    assert!(shown(vcx, "swarm-group-0"));
    assert!(!shown(vcx, "swarm-group-1"));
    assert!(shown(vcx, "swarm-member-0-0"));
    assert!(!shown(vcx, "swarm-member-0-1"));
    click(vcx, "swarm-view-filter-all");
    assert!(shown(vcx, "swarm-group-1"));
    // Liveness follows roster updates.
    let mut next = roster();
    next.get_mut(BOAR).unwrap().status = "completed".into();
    workspace.update(vcx, |w, cx| {
        w.apply(Update::SwarmState { roster: next }, cx);
        cx.notify();
    });
    vcx.run_until_parked();
    click(vcx, "swarm-view-filter-running");
    // The coordinator still runs, so its swarm stays, with no running agents.
    assert!(shown(vcx, "swarm-group-0"));
    assert!(!shown(vcx, "swarm-member-0-0"));
    let mut idle = roster();
    for m in idle.values_mut() {
        if AgentState::of(&m.status).is_live() {
            m.status = "ready".into();
        }
    }
    workspace.update(vcx, |w, cx| {
        w.apply(Update::SwarmState { roster: idle }, cx);
        cx.notify();
    });
    vcx.run_until_parked();
    assert!(shown(vcx, "swarm-view-empty"));
}

#[gpui::test]
fn swarm_view_member_click_opens_session(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx, commands) = setup(cx);
    open(&workspace, vcx);
    while commands.try_recv().is_ok() {}
    // Row 1 is boar (failed dingo first).
    click(vcx, "swarm-member-0-1");
    let watched: Vec<String> = std::iter::from_fn(|| commands.try_recv().ok())
        .filter_map(|c| match c {
            Command::Watch { session_id } => Some(session_id),
            _ => None,
        })
        .collect();
    assert_eq!(watched, [BOAR.to_string()]);
    workspace.read_with(vcx, |w, cx| {
        assert!(w.swarm_view.is_none(), "view closes onto the session");
        assert_eq!(w.slots[w.active].panel.read(cx).session_id, BOAR);
    });
    // Coordinator header focuses the already-open lead panel.
    open(&workspace, vcx);
    while commands.try_recv().is_ok() {}
    click(vcx, "swarm-head-0");
    workspace.read_with(vcx, |w, cx| {
        assert_eq!(w.slots[w.active].panel.read(cx).session_id, LEAD);
    });
}

#[gpui::test]
fn swarm_view_entry_points(cx: &mut gpui::TestAppContext) {
    cx.update(crate::bind_workspace_keys);
    let (workspace, vcx, _commands) = setup(cx);
    // Sidebar entry carries the live count.
    assert!(shown(vcx, "sidebar-swarms"));
    assert!(shown(vcx, "sidebar-swarms-count"));
    click(vcx, "sidebar-swarms");
    assert!(shown(vcx, "swarm-view"));
    // Escape closes.
    workspace.update_in(vcx, |w, window, cx| {
        let handle = w.focus_handle.clone();
        window.focus(&handle, cx);
    });
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    assert!(!shown(vcx, "swarm-view"));
    // Keyboard: Ctrl+Alt+S toggles (Windows-safe alias).
    workspace.update_in(vcx, |w, window, cx| {
        let handle = w.focus_handle.clone();
        window.focus(&handle, cx);
    });
    vcx.simulate_keystrokes("ctrl-alt-s");
    vcx.run_until_parked();
    assert!(shown(vcx, "swarm-view"));
    vcx.simulate_keystrokes("ctrl-alt-s");
    vcx.run_until_parked();
    assert!(!shown(vcx, "swarm-view"));
}

#[gpui::test]
fn swarm_view_empty_state(cx: &mut gpui::TestAppContext) {
    let (bridge, _commands) = harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut w = Workspace::for_test(learning::Coach::new(), cx);
        w.set_test_bridge(bridge);
        w
    });
    open(&workspace, vcx);
    assert!(shown(vcx, "swarm-view-empty"));
    assert!(!shown(vcx, "sidebar-swarms-count"));
}
