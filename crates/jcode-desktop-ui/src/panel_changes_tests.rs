//! Changes review: pure snapshot helpers, plus GPUI flows on a real repo.
use super::*;
use crate::workspace::Workspace;
use jcode_desktop_harness::session_git::BranchInfo;

fn file(area: Area, path: &str, status: char, added: usize, removed: usize) -> ChangedFile {
    ChangedFile {
        area,
        path: path.into(),
        previous_path: None,
        status,
        added,
        removed,
        binary: false,
    }
}

#[test]
fn changes_command_accepts_only_bare_and_staged_forms() {
    assert_eq!(changes_command("/changes"), Some(false));
    assert_eq!(changes_command("  /changes  "), Some(false));
    assert_eq!(changes_command("/changes --staged"), Some(true));
    assert_eq!(changes_command("/changes --cached"), Some(true));
    // Invalid arguments keep the transcript command's usage error.
    assert_eq!(changes_command("/changes --bogus"), None);
    assert_eq!(changes_command("/changes --staged x"), None);
    assert_eq!(changes_command("/changesx"), None);
    assert_eq!(changes_command("/git"), None);
}

#[test]
fn snapshot_pairs_each_area_with_its_own_patch() {
    let tree = WorkingTree {
        branch: BranchInfo {
            head: Some("main".into()),
            ahead: 2,
            behind: 1,
            ..Default::default()
        },
        files: vec![
            file(Area::Untracked, "new.txt", '?', 2, 0),
            file(Area::Unstaged, "a.rs", 'M', 1, 1),
            file(Area::Staged, "a.rs", 'M', 1, 0),
            ChangedFile {
                binary: true,
                ..file(Area::Untracked, "bin.dat", '?', 0, 0)
            },
        ],
    };
    let patches = WorkingTreePatches {
        staged: "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n one\n+staged\n"
            .into(),
        unstaged: "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-one\n+ONE\n"
            .into(),
        untracked:
            "diff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n"
                .into(),
        truncated: false,
    };
    let snapshot = review_snapshot(tree, patches);
    let areas: Vec<_> = snapshot
        .tree
        .files
        .iter()
        .map(|f| (f.area, f.path.as_str()))
        .collect();
    assert_eq!(
        areas,
        [
            (Area::Staged, "a.rs"),
            (Area::Unstaged, "a.rs"),
            (Area::Untracked, "bin.dat"),
            (Area::Untracked, "new.txt"),
        ]
    );
    let files = &snapshot.preview.files;
    assert_eq!(files.len(), snapshot.tree.files.len(), "one view per row");
    let text = |index: usize| -> Vec<String> {
        files[index]
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| l.text.clone()))
            .collect()
    };
    assert!(text(0).contains(&"staged".to_string()), "{:?}", text(0));
    assert!(text(1).contains(&"ONE".to_string()), "{:?}", text(1));
    assert!(files[2].hunks.is_empty());
    assert!(files[2].note.as_deref().unwrap().contains("Binary"));
    assert_eq!(files[3].kind, "Added");
    assert_eq!(text(3), ["x", "y"]);
    let rows = grouped_rows(&snapshot.tree);
    assert!(matches!(rows[0], ChangesRow::Group(Area::Staged, 1)));
    assert!(matches!(rows[2], ChangesRow::Group(Area::Unstaged, 1)));
    assert!(matches!(rows[4], ChangesRow::Group(Area::Untracked, 2)));
    assert_eq!(rows.len(), 7);
    let summary = summary_label(&snapshot.tree);
    assert!(summary.starts_with("main  ↑2  ↓1 · 3 files · +4 −1"), "{summary}");
    assert!(summary.ends_with("1 staged · 1 unstaged · 2 untracked"), "{summary}");
}

#[test]
fn snapshot_explains_missing_content() {
    let tree = WorkingTree {
        files: vec![
            file(Area::Untracked, "empty.txt", '?', 0, 0),
            file(Area::Unstaged, "big.rs", 'M', 9, 9),
        ],
        ..Default::default()
    };
    let snapshot = review_snapshot(
        tree,
        WorkingTreePatches {
            truncated: true,
            ..Default::default()
        },
    );
    assert!(snapshot.truncated);
    let notes: Vec<_> = snapshot
        .preview
        .files
        .iter()
        .map(|f| f.note.clone().unwrap())
        .collect();
    assert!(notes[0].contains("size limit"), "{notes:?}");
    assert!(notes[1].contains("Empty untracked"), "{notes:?}");
    assert_eq!(branch_label(&snapshot.tree), "No commits");
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

/// Staged edit, unstaged deletion of a line and one untracked file.
fn temp_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "--quiet", "--initial-branch=main"]);
    git(dir, &["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(dir.join("b.txt"), "keep\ndrop\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "--quiet", "-m", "init"]);
    std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    git(dir, &["add", "a.txt"]);
    std::fs::write(dir.join("b.txt"), "keep\n").unwrap();
    std::fs::write(dir.join("new.txt"), "x\ny\n").unwrap();
    repo
}

/// Real git runs on the background executor, so tick until `done`.
fn wait_until(
    vcx: &mut gpui::VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        vcx.run_until_parked();
        if done(vcx) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn summary_totals(panel: &Entity<Panel>, vcx: &mut gpui::VisualTestContext) -> Option<(usize, usize)> {
    panel.read_with(vcx, |panel, _| {
        panel
            .changes_summary
            .as_ref()
            .and_then(|summary| summary.as_ref().ok())
            .map(WorkingTree::totals)
    })
}

fn workspace_with_panel<'a>(
    cx: &'a mut gpui::TestAppContext,
    session_id: &'static str,
    working_dir: &str,
) -> (Entity<Workspace>, Entity<Panel>, &'a mut gpui::VisualTestContext) {
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
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    let working_dir = working_dir.to_owned();
    panel.update(vcx, |panel, cx| {
        panel.working_dir = Some(working_dir);
        cx.notify();
    });
    (workspace, panel, vcx)
}

fn review_panel(workspace: &Entity<Workspace>, vcx: &mut gpui::VisualTestContext) -> Option<Entity<Panel>> {
    workspace.read_with(vcx, |w, cx| {
        (0..8)
            .filter_map(|index| w.test_panel(index))
            .find(|panel| panel.read(cx).working_tree_review.is_some())
    })
}

fn review_loaded(review: &Entity<Panel>, vcx: &mut gpui::VisualTestContext) -> bool {
    review.read_with(vcx, |panel, _| {
        panel
            .working_tree_review
            .as_ref()
            .is_some_and(|review| !review.loading && (review.tree.is_some() || review.error.is_some()))
    })
}

#[gpui::test]
fn pill_opens_changes_review_with_grouped_files(cx: &mut gpui::TestAppContext) {
    let repo = temp_repo();
    let path = repo.path().to_str().unwrap().to_owned();
    let (workspace, panel, vcx) = workspace_with_panel(cx, "local-changes", &path);
    wait_until(vcx, "pill summary", |vcx| summary_totals(&panel, vcx).is_some());
    // a.txt +2 -1 staged, b.txt -1 unstaged, new.txt +2 untracked.
    assert_eq!(summary_totals(&panel, vcx), Some((4, 2)));
    vcx.run_until_parked();
    let pill = vcx.debug_bounds("panel-changes").expect("Changes pill paints");
    vcx.simulate_click(pill.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
    let review = review_panel(&workspace, vcx).expect("pill opened the Changes review");
    assert_ne!(review, panel, "review opens in its own column");
    review.read_with(vcx, |review, _| {
        assert!(review.is_change_review());
        assert_eq!(review.title.as_ref(), "Changes");
    });
    wait_until(vcx, "review load", |vcx| review_loaded(&review, vcx));
    review.read_with(vcx, |review, _| {
        let state = review.working_tree_review.as_ref().unwrap();
        assert!(state.error.is_none(), "{:?}", state.error);
        let tree = state.tree.as_ref().unwrap();
        let rows: Vec<_> = tree
            .files
            .iter()
            .map(|f| (f.area, f.path.as_str(), f.status, f.added, f.removed))
            .collect();
        assert_eq!(
            rows,
            [
                (Area::Staged, "a.txt", 'M', 2, 1),
                (Area::Unstaged, "b.txt", 'M', 0, 1),
                (Area::Untracked, "new.txt", '?', 2, 0),
            ]
        );
        assert_eq!(state.rows.len(), 6, "three groups and three files");
        assert!(state.content.is_some(), "rich diff content is ready");
        assert_eq!(state.selected, 0);
    });
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("working-tree-review").is_some());
    assert!(vcx.debug_bounds("changes-file-list").is_some());
    assert!(vcx.debug_bounds("diff-view").is_some(), "existing diff view renders");
    // Selecting the untracked file shows it as an added file.
    let untracked = vcx.debug_bounds("changes-file-2").expect("untracked row paints");
    vcx.simulate_click(untracked.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
    review.read_with(vcx, |review, _| {
        assert_eq!(review.working_tree_review.as_ref().unwrap().selected, 2)
    });
    // Refresh picks up a new edit and keeps the selection by path.
    std::fs::write(repo.path().join("new.txt"), "x\ny\nz\n").unwrap();
    let refresh = vcx.debug_bounds("changes-refresh").expect("refresh paints");
    vcx.simulate_click(refresh.center(), gpui::Modifiers::default());
    wait_until(vcx, "refresh", |vcx| {
        review.read_with(vcx, |review, _| {
            let state = review.working_tree_review.as_ref().unwrap();
            !state.loading
                && state
                    .tree
                    .as_ref()
                    .is_some_and(|tree| tree.files.iter().any(|f| f.path == "new.txt" && f.added == 3))
        })
    });
    review.read_with(vcx, |review, _| {
        assert_eq!(review.working_tree_review.as_ref().unwrap().selected, 2)
    });
    // Esc closes the review column and returns to the conversation.
    vcx.simulate_keystrokes("escape");
    vcx.run_until_parked();
    vcx.update(|window, cx| {
        assert!(panel.read(cx).input_focus_handle(cx).is_focused(window));
    });
}

#[gpui::test]
fn turn_done_refreshes_the_pill_once_per_throttle_window(cx: &mut gpui::TestAppContext) {
    let repo = temp_repo();
    let path = repo.path().to_str().unwrap().to_owned();
    let (_workspace, panel, vcx) = workspace_with_panel(cx, "local-turns", &path);
    wait_until(vcx, "pill summary", |vcx| summary_totals(&panel, vcx).is_some());
    assert_eq!(summary_totals(&panel, vcx), Some((4, 2)));
    // The agent edits a file during the turn.
    std::fs::write(repo.path().join("b.txt"), "keep\nmore\nlines\n").unwrap();
    panel.update(vcx, |panel, cx| {
        let session_id = panel.session_id.clone();
        panel.apply(
            &ApiEvent::TurnDone {
                session_id: session_id.clone(),
                pending_soft_interrupts: None,
            },
            cx,
        );
        // A burst of turn ends coalesces into one timer, not more git runs.
        panel.apply(
            &ApiEvent::TurnDone {
                session_id,
                pending_soft_interrupts: None,
            },
            cx,
        );
        assert!(panel.changes_refresh_timer || panel.changes_refresh_in_flight);
    });
    vcx.executor().advance_clock(SUMMARY_THROTTLE + Duration::from_millis(100));
    wait_until(vcx, "turn refresh", |vcx| summary_totals(&panel, vcx) == Some((6, 2)));
    panel.read_with(vcx, |panel, _| {
        assert!(!panel.changes_refresh_in_flight);
        assert!(!panel.changes_refresh_pending);
        assert!(!panel.changes_refresh_timer);
    });
}

#[gpui::test]
fn clean_or_unreachable_checkouts_show_no_pill(cx: &mut gpui::TestAppContext) {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "--quiet", "--initial-branch=main"]);
    let path = repo.path().to_str().unwrap().to_owned();
    let (_workspace, panel, vcx) = workspace_with_panel(cx, "local-clean", &path);
    wait_until(vcx, "clean summary", |vcx| summary_totals(&panel, vcx).is_some());
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("panel-changes").is_none(), "clean tree hides the pill");
    // Arbitrary SSH hosts are never probed.
    panel.update(vcx, |panel, cx| {
        panel.session_id = "ssh://devbox/session_x".into();
        assert!(panel.git_changes_context().is_none());
        cx.notify();
    });
}

#[gpui::test]
fn changes_command_opens_the_review_from_the_composer(cx: &mut gpui::TestAppContext) {
    let repo = temp_repo();
    let path = repo.path().to_str().unwrap().to_owned();
    let (workspace, panel, vcx) = workspace_with_panel(cx, "local-command", &path);
    vcx.update(|window, cx| {
        let handle = panel.read(cx).input.read(cx).focus_handle.clone();
        window.focus(&handle, cx);
    });
    vcx.simulate_input("/changes --staged");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    let review = review_panel(&workspace, vcx).expect("/changes opened the review");
    wait_until(vcx, "review load", |vcx| review_loaded(&review, vcx));
    review.read_with(vcx, |review, _| {
        let state = review.working_tree_review.as_ref().unwrap();
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(tree.files[state.selected].area, Area::Staged);
    });
    // The command never reached the transcript as a git-diff card.
    panel.read_with(vcx, |panel, _| {
        assert!(!panel.items.iter().any(|item| matches!(item, Item::Tool { call_id, .. } if call_id.starts_with("git-diff-"))));
    });
}

/// Windows only, opt-in: `JCODE_WSL_TEST_REPO=/home/me/repo` with a pending change.
#[gpui::test]
fn wsl_changes_pill_and_review_end_to_end(cx: &mut gpui::TestAppContext) {
    let Ok(repo) = std::env::var("JCODE_WSL_TEST_REPO") else {
        return;
    };
    let (workspace, panel, vcx) = workspace_with_panel(cx, "ssh://wsl/session_changes_e2e", &repo);
    panel.read_with(vcx, |panel, _| {
        assert_eq!(
            panel.git_changes_context().map(|(location, _)| location),
            Some(Location::Wsl(None))
        );
    });
    let started = std::time::Instant::now();
    wait_until(vcx, "WSL pill summary", |vcx| {
        panel.read_with(vcx, |panel, _| panel.changes_summary.is_some())
    });
    let summary = panel.read_with(vcx, |panel, _| panel.changes_summary.clone().unwrap());
    let tree = summary.expect("WSL working tree");
    eprintln!("WSL pill summary in {:?}: {}", started.elapsed(), summary_label(&tree));
    assert!(!tree.is_clean(), "repo needs a pending change");
    vcx.run_until_parked();
    let pill = vcx.debug_bounds("panel-changes").expect("Changes pill paints for WSL");
    vcx.simulate_click(pill.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
    let review = review_panel(&workspace, vcx).expect("pill opened the review");
    wait_until(vcx, "WSL review load", |vcx| review_loaded(&review, vcx));
    review.read_with(vcx, |review, _| {
        let state = review.working_tree_review.as_ref().unwrap();
        assert!(state.error.is_none(), "{:?}", state.error);
        let tree = state.tree.as_ref().unwrap();
        assert!(tree.files.iter().any(|f| f.path.ends_with("main.rs")), "{:?}", tree.files);
        assert!(state.content.is_some());
    });
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("diff-view").is_some());
    eprintln!("WSL Changes review loaded in {:?}", started.elapsed());
}
