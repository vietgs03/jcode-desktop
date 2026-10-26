//! Review pane: scopes, agent turns, history, attach to chat and maximize.
use super::*;
use crate::workspace::Workspace;

#[test]
fn review_and_changes_commands_open_the_pane() {
    assert_eq!(review_command("/review"), Some(false));
    assert_eq!(review_command("  /review "), Some(false));
    assert_eq!(review_command("/changes"), Some(false));
    assert_eq!(review_command("/changes --staged"), Some(true));
    assert_eq!(review_command("/review openai:model check"), None, "one-shot review sessions keep working");
}

#[test]
fn relative_dates_are_compact() {
    assert_eq!(relative_date(100, 110), "just now");
    assert_eq!(relative_date(0, 3_600 * 5), "5h ago");
    assert_eq!(relative_date(0, 86_400 * 3), "3d ago");
    assert_eq!(relative_date(10, 0), "just now");
}

#[test]
fn wanted_scope_follows_the_visible_tab() {
    let mut pane = PaneState::new();
    assert_eq!(pane.wanted_scope(), None);
    pane.changes_scope = ChangesScope::Branch("main".into());
    assert_eq!(pane.wanted_scope(), Some(Scope::Branch("main".into())));
    pane.tab = Tab::Turns;
    assert_eq!(pane.wanted_scope(), None, "no turns yet");
    pane.turns = agent_turns(&crate::review_turns::fixture());
    pane.turn_selected = Some(1);
    assert_eq!(pane.wanted_scope(), Some(Scope::Turn(1)));
    pane.tab = Tab::History;
    pane.commits = vec![Commit {
        hash: "abc".into(),
        ..Default::default()
    }];
    pane.commit_selected = Some(0);
    assert_eq!(pane.wanted_scope(), Some(Scope::Commit("abc".into())));
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.autocrlf=false"])
        .args(args)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn lines(prefix: &str, count: usize) -> String {
    (0..count).map(|i| format!("{prefix} {i}\n")).collect()
}

/// main with two commits, a `feature` branch with one more commit and an
/// uncommitted change spread over three long files.
fn history_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "--quiet", "--initial-branch=main"]);
    git(dir, &["config", "core.autocrlf", "false"]);
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), lines(name, 60)).unwrap();
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "--quiet", "-m", "init"]);
    std::fs::write(dir.join("a.txt"), lines("main a", 60)).unwrap();
    git(dir, &["commit", "--quiet", "-am", "second on main"]);
    git(dir, &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(dir.join("feature.txt"), "feature\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "--quiet", "-m", "feature work"]);
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), lines(&format!("edited {name}"), 60)).unwrap();
    }
    repo
}

fn wait_until(
    vcx: &mut gpui::VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        vcx.run_until_parked();
        if done(vcx) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn open_review<'a>(
    cx: &'a mut gpui::TestAppContext,
    session_id: &'static str,
    working_dir: &str,
) -> (Entity<Workspace>, Entity<Panel>, Entity<Panel>, &'a mut gpui::VisualTestContext) {
    cx.executor().allow_parking();
    cx.update(crate::bind_workspace_keys);
    cx.update(crate::input::bind_keys);
    let (bridge, _commands) = crate::harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(crate::learning::Coach::new(), cx);
        workspace.set_test_bridge(bridge);
        workspace.push_test_panel(session_id, cx);
        workspace
    });
    let source = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    let working_dir = working_dir.to_owned();
    source.update(vcx, |panel, cx| {
        panel.working_dir = Some(working_dir);
        *panel.items = crate::review_turns::fixture();
        cx.notify();
    });
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        let handle = source.read(cx).input_focus_handle(cx);
        window.focus(&handle, cx);
    });
    // Ctrl+Alt+G opens the pane for the active conversation.
    vcx.simulate_keystrokes("ctrl-alt-g");
    vcx.run_until_parked();
    let review = workspace
        .read_with(vcx, |w, cx| {
            (0..8)
                .filter_map(|index| w.test_panel(index))
                .find(|panel| panel.read(cx).working_tree_review.is_some())
        })
        .expect("Ctrl+Alt+G opened the Review pane");
    wait_until(vcx, "working tree", |vcx| {
        review.read_with(vcx, |panel, _| {
            panel
                .working_tree_review
                .as_ref()
                .is_some_and(|r| !r.loading && (r.tree.is_some() || r.error.is_some()))
        })
    });
    (workspace, source, review, vcx)
}

fn click(vcx: &mut gpui::VisualTestContext, selector: &'static str) {
    vcx.run_until_parked();
    let bounds = vcx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} paints"));
    vcx.simulate_click(bounds.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
}

fn pane<R>(review: &Entity<Panel>, vcx: &mut gpui::VisualTestContext, f: impl FnOnce(&PaneState) -> R) -> R {
    review.read_with(vcx, |panel, _| f(panel.review_pane().unwrap()))
}

fn scoped_ready(review: &Entity<Panel>, vcx: &mut gpui::VisualTestContext) -> bool {
    pane(review, vcx, |pane| {
        pane.scoped_loading.is_none()
            && (pane.scoped_error.is_some()
                || pane
                    .scoped
                    .as_ref()
                    .is_some_and(|s| Some(&s.scope) == pane.wanted_scope().as_ref()))
    })
}

#[gpui::test]
fn review_pane_scopes_turns_history_attach_and_maximize(cx: &mut gpui::TestAppContext) {
    let repo = history_repo();
    let path = repo.path().to_str().unwrap().to_owned();
    let (workspace, source, review, vcx) = open_review(cx, "local-review-pane", &path);
    review_pane_flow(&workspace, &source, &review, vcx, true);
}

/// Windows only, opt-in: `JCODE_WSL_TEST_REPO` with commits and a pending change.
#[gpui::test]
fn wsl_review_pane_end_to_end(cx: &mut gpui::TestAppContext) {
    let Ok(repo) = std::env::var("JCODE_WSL_TEST_REPO") else {
        return;
    };
    let started = std::time::Instant::now();
    let (workspace, source, review, vcx) = open_review(cx, "ssh://wsl/session_review_pane", &repo);
    review_pane_flow(&workspace, &source, &review, vcx, false);
    eprintln!("WSL Review pane flow in {:?}", started.elapsed());
}

fn review_pane_flow(
    workspace: &Entity<Workspace>,
    source: &Entity<Panel>,
    review: &Entity<Panel>,
    vcx: &mut gpui::VisualTestContext,
    local: bool,
) {
    review.read_with(vcx, |panel, _| {
        let state = panel.working_tree_review.as_ref().unwrap();
        assert!(state.error.is_none(), "{:?}", state.error);
        assert!(state.content.is_some(), "stacked diff is ready");
    });
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("review-stack-scroll").is_some());
    assert!(vcx.debug_bounds("review-stack-file-0").is_some(), "files are stacked");

    // Tree click scrolls the stacked diff to that file.
    let files = review.read_with(vcx, |panel, _| {
        panel.working_tree_review.as_ref().unwrap().tree.as_ref().unwrap().files.len()
    });
    if files >= 3 {
        click(vcx, "changes-file-2");
        vcx.run_until_parked();
        review.read_with(vcx, |panel, _| {
            let state = panel.working_tree_review.as_ref().unwrap();
            assert_eq!(state.selected, 2);
            assert_eq!(state.content.as_ref().unwrap().selected(), 2);
        });
        let top = vcx.debug_bounds("review-stack-scroll").unwrap().top();
        let file = vcx.debug_bounds("review-stack-file-2").unwrap().top();
        assert!(
            (file - top).abs() < gpui::px(24.),
            "file 2 is scrolled to the top: {file:?} vs {top:?}"
        );
    }

    // Split toggle reaches every stacked view.
    click(vcx, "review-split");
    review.read_with(vcx, |panel, _| {
        let state = panel.working_tree_review.as_ref().unwrap();
        assert!(state.pane.split && state.content.as_ref().unwrap().split());
    });
    click(vcx, "review-unified");

    // Scope: vs the auto-detected base branch.
    wait_until(vcx, "branches", |vcx| pane(review, vcx, |p| !p.branches.is_empty()));
    if local {
        assert_eq!(pane(review, vcx, |p| p.base.clone()), Some("main".into()));
    }
    if pane(review, vcx, |p| p.base.is_none()) {
        // Single-branch checkout: compare against its own branch instead.
        let first = pane(review, vcx, |p| p.branches[0].clone());
        review.update(vcx, |panel, cx| panel.set_changes_scope(ChangesScope::Branch(first), cx));
    } else {
        click(vcx, "review-scope-base");
    }
    wait_until(vcx, "branch diff", |vcx| scoped_ready(review, vcx));
    pane(review, vcx, |p| {
        assert!(p.scoped_error.is_none(), "{:?}", p.scoped_error);
        let scoped = p.scoped.as_ref().unwrap();
        assert!(matches!(scoped.scope, Scope::Branch(_)));
        if local {
            let paths: Vec<_> = scoped.files.iter().map(|f| f.path.as_str()).collect();
            assert!(paths.contains(&"feature.txt"), "{paths:?}");
            assert!(paths.contains(&"b.txt"), "working tree edits are included: {paths:?}");
        }
    });
    if pane(review, vcx, |p| p.scoped.as_ref().unwrap().files.len()) > 1 {
        click(vcx, "review-scope-file-1");
        assert_eq!(pane(review, vcx, |p| p.scoped.as_ref().unwrap().stack.selected()), 1);
    }
    // Branch picker lists branches and picks one.
    click(vcx, "review-scope-branch");
    wait_until(vcx, "branches", |vcx| pane(review, vcx, |p| !p.branches.is_empty()));
    assert!(vcx.debug_bounds("review-branch-menu").is_some());
    click(vcx, "review-branch-0");
    assert!(pane(review, vcx, |p| !p.branch_menu));
    click(vcx, "review-scope-uncommitted");
    assert_eq!(pane(review, vcx, |p| p.changes_scope.clone()), ChangesScope::Uncommitted);

    // Agent turns from the conversation transcript, newest first.
    click(vcx, "review-tab-turns");
    pane(review, vcx, |p| {
        assert_eq!(p.turns.len(), 2);
        assert_eq!(p.turns[0].number, 3);
        assert_eq!(p.wanted_scope(), Some(Scope::Turn(3)));
        assert_eq!(p.scoped.as_ref().unwrap().files[0].path, "src/nav.rs");
    });
    click(vcx, "review-turn-1");
    pane(review, vcx, |p| {
        assert_eq!(p.wanted_scope(), Some(Scope::Turn(1)));
        assert_eq!(p.scoped.as_ref().unwrap().files[0].path, "src/hello.rs");
    });
    click(vcx, "review-turn-file-1000");
    assert_eq!(pane(review, vcx, |p| p.scoped.as_ref().unwrap().stack.selected()), 0);

    // Attach to chat writes the fenced diff into the source composer.
    source.update(vcx, |panel, cx| {
        panel.input.update(cx, |input, cx| input.set_content("Please check".into(), cx))
    });
    click(vcx, "diff-attach-0");
    let draft = source.read_with(vcx, |panel, cx| panel.input.read(cx).content.to_string());
    assert!(draft.starts_with("Please check\n```diff\n"), "{draft}");
    assert!(draft.contains("src/hello.rs"), "{draft}");

    // History: commits with a graph; selecting one shows its diff.
    click(vcx, "review-tab-history");
    wait_until(vcx, "history", |vcx| {
        pane(review, vcx, |p| !p.history_loading && !p.commits.is_empty())
    });
    wait_until(vcx, "first commit diff", |vcx| scoped_ready(review, vcx));
    pane(review, vcx, |p| {
        assert_eq!(p.graph.len(), p.commits.len());
        if local {
            assert_eq!(p.commits.len(), 3);
            assert_eq!(p.commits[0].subject, "feature work");
            assert!(p.history_done);
        }
    });
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("review-history").is_some());
    let pick = pane(review, vcx, |p| p.commits.len().min(2) - 1);
    click(vcx, if pick == 1 { "review-commit-1" } else { "review-commit-0" });
    wait_until(vcx, "commit diff", |vcx| scoped_ready(review, vcx));
    pane(review, vcx, |p| {
        assert!(p.scoped_error.is_none(), "{:?}", p.scoped_error);
        let scoped = p.scoped.as_ref().unwrap();
        assert!(!scoped.files.is_empty(), "commit diff has files");
        assert_eq!(scoped.scope, Scope::Commit(p.commits[pick].hash.clone()));
        if local {
            assert_eq!(scoped.files.len(), 1);
            assert_eq!(scoped.files[0].path, "a.txt");
        }
    });
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("review-history").is_some(), "graph stays visible");
    assert!(vcx.debug_bounds("review-stack-file-0").is_some());

    // Maximize to the workspace width and back, by button and by key.
    let (active, before) = workspace.read_with(vcx, |w, _| w.test_active_slot());
    assert_eq!(active, *review);
    assert!(before < 1.0);
    click(vcx, "review-maximize");
    workspace.read_with(vcx, |w, _| assert_eq!(w.test_active_slot(), (review.clone(), 1.0)));
    vcx.simulate_keystrokes("ctrl-shift-enter");
    vcx.run_until_parked();
    workspace.read_with(vcx, |w, _| assert_eq!(w.test_active_slot(), (review.clone(), before)));

    // Ctrl+Alt+G from the pane closes back to the conversation.
    vcx.simulate_keystrokes("ctrl-alt-g");
    vcx.run_until_parked();
    workspace.read_with(vcx, |w, _| assert_eq!(w.test_active_slot().0, *source));
}

#[gpui::test]
fn review_changes_button_appears_under_editing_turns(cx: &mut gpui::TestAppContext) {
    let repo = history_repo();
    let path = repo.path().to_str().unwrap().to_owned();
    cx.executor().allow_parking();
    cx.update(crate::bind_workspace_keys);
    let (bridge, _commands) = crate::harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(crate::learning::Coach::new(), cx);
        workspace.set_test_bridge(bridge);
        workspace.push_test_panel("local-turn-button", cx);
        workspace
    });
    let source = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    source.update(vcx, |panel, cx| {
        panel.working_dir = Some(path);
        let mut items = crate::review_turns::fixture();
        items.truncate(4);
        items.push(Item::ResponseStats(super::response_stats::fixture()));
        *panel.items = items;
        cx.notify();
    });
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("turn-review-changes-4").is_some());
    click(vcx, "turn-review-changes-4");
    let opened = workspace.read_with(vcx, |w, cx| {
        (0..4)
            .filter_map(|index| w.test_panel(index))
            .any(|panel| panel.read(cx).working_tree_review.is_some())
    });
    assert!(opened, "Review changes opened the pane");
}
