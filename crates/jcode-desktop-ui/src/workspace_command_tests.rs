use super::*;
use crate::panel::Item;

#[gpui::test]
fn workspace_commands_route_fork_and_transfer_without_replacing_source(
    cx: &mut gpui::TestAppContext,
) {
    let (bridge, commands) = harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|window, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.bridge = bridge;
        workspace.push_test_panel("source", cx);
        workspace.restore_focus(window, cx);
        workspace
    });
    while commands.try_recv().is_ok() {}
    for (content, expected_prompt) in [
        ("/fork", None),
        ("/btw side question", Some("side question")),
        ("/transfer", None),
    ] {
        vcx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let source = workspace.slots[0].panel.entity_id();
                workspace.workspace_slash_command(
                    &WorkspaceSlashCommand {
                        source,
                        content: content.into(),
                    },
                    window,
                    cx,
                );
                assert_eq!(workspace.slots[0].panel.read(cx).session_id, "source");
                assert_eq!(workspace.slots.len(), 1);
            })
        });
        match commands.try_recv().expect("real lifecycle command") {
            Command::Fork { session_id } if content == "/fork" => assert_eq!(session_id, "source"),
            Command::ForkWithPrompt { session_id, prompt } => {
                assert_eq!(session_id, "source");
                assert_eq!(Some(prompt.as_str()), expected_prompt);
            }
            Command::Transfer { session_id } if content == "/transfer" => {
                assert_eq!(session_id, "source")
            }
            _ => panic!("slash command must not become a generic model prompt"),
        }
    }
}

#[gpui::test]
fn workspace_mirror_is_read_only_live_and_has_independent_scroll(cx: &mut gpui::TestAppContext) {
    let (bridge, commands) = harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|window, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.bridge = bridge;
        workspace.push_test_panel("source", cx);
        workspace.restore_focus(window, cx);
        workspace
    });
    while commands.try_recv().is_ok() {}
    vcx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            let source = workspace.slots[0].panel.entity_id();
            workspace.workspace_slash_command(
                &WorkspaceSlashCommand {
                    source,
                    content: "/splitview on".into(),
                },
                window,
                cx,
            );
        })
    });
    vcx.run_until_parked();
    workspace.update(vcx, |workspace, cx| {
        assert_eq!(workspace.slots.len(), 2);
        let mirror = workspace.slots[1].panel.clone();
        assert!(mirror.read(cx).is_side_document());
        assert!(!mirror.read(cx).can_fork());
        workspace.slots[0].panel.update(cx, |panel, cx| {
            panel.workspace_command_notice("New source content", cx)
        });
    });
    vcx.run_until_parked();
    workspace.read_with(vcx, |workspace, cx| {
        let mirror = workspace.slots[1]
            .panel
            .read(cx)
            .snapshot(cx)
            .side_document
            .unwrap();
        assert!(mirror.page.content.contains("New source content"));
        assert_eq!(mirror.owner_session_id, "desktop-live://source");
    });
    assert!(
        commands.try_recv().is_err(),
        "a mirror does not create a runtime session"
    );
    vcx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            let source = workspace.slots[0].panel.entity_id();
            workspace.workspace_slash_command(
                &WorkspaceSlashCommand {
                    source,
                    content: "/splitview off".into(),
                },
                window,
                cx,
            );
            assert_eq!(workspace.slots[0].panel.read(cx).session_id, "source");
        })
    });
}

#[gpui::test]
fn workspace_gateway_commands_use_the_source_runtime_transport(cx: &mut gpui::TestAppContext) {
    let (bridge, commands) = harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|window, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.bridge = bridge;
        workspace.push_test_panel("ssh://host/source", cx);
        workspace.restore_focus(window, cx);
        workspace
    });
    while commands.try_recv().is_ok() {}
    for (content, command, args) in [
        (
            "/remote status",
            jcode_sdk::RuntimeCommand::Remote,
            vec!["status".to_owned()],
        ),
        (
            "/server-reload",
            jcode_sdk::RuntimeCommand::ServerReload,
            Vec::new(),
        ),
    ] {
        vcx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let source = workspace.slots[0].panel.entity_id();
                workspace.workspace_slash_command(
                    &WorkspaceSlashCommand {
                        source,
                        content: content.into(),
                    },
                    window,
                    cx,
                );
            })
        });
        match commands.try_recv().expect("server-owned command") {
            Command::RuntimeCommand {
                session_id,
                command: actual,
                args: actual_args,
            } => {
                assert_eq!(session_id, "ssh://host/source");
                assert_eq!(actual, command);
                assert_eq!(actual_args, args);
            }
            _ => panic!(
                "gateway control must use the source server, not local Desktop process state"
            ),
        }
    }
}

#[gpui::test]
fn workspace_resume_preserves_search_and_active_filter(cx: &mut gpui::TestAppContext) {
    let (workspace, vcx) = cx.add_window_view(|window, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.push_test_panel("source", cx);
        workspace.restore_focus(window, cx);
        workspace
    });
    vcx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            let source = workspace.slots[0].panel.entity_id();
            workspace.workspace_slash_command(
                &WorkspaceSlashCommand {
                    source,
                    content: "/active exact query".into(),
                },
                window,
                cx,
            );
            match workspace.resume_snapshot(cx) {
                resume::Snapshot::Open {
                    query,
                    search,
                    filter,
                    ..
                } => {
                    assert_eq!(query, "exact query");
                    assert_eq!(search.content, "exact query");
                    assert_eq!(filter, resume::Filter::Active);
                }
                _ => panic!("active opens the native session picker"),
            }
            assert_eq!(workspace.slots[0].panel.read(cx).session_id, "source");
        })
    });
}

/// Real end-to-end check on Windows with WSL, opt-in:
/// `JCODE_WSL_TEST_REPO=/home/me/repo` (with an uncommitted tracked change).
/// Drives the actual slash handlers of a `ssh://wsl/...` panel: `/changes`
/// must render a reviewable diff card that opens the review, and `/worktree`
/// must create the checkout through wsl.exe and request a WSL session in it.
#[gpui::test]
fn wsl_changes_and_worktree_commands_end_to_end(cx: &mut gpui::TestAppContext) {
    let Ok(repo) = std::env::var("JCODE_WSL_TEST_REPO") else {
        return;
    };
    cx.executor().allow_parking();
    cx.update(crate::input::bind_keys);
    let (bridge, commands) = harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|window, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.bridge = bridge;
        workspace.push_test_panel("ssh://wsl/session_e2e", cx);
        workspace.restore_focus(window, cx);
        workspace
    });
    while commands.try_recv().is_ok() {}
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    panel.update(vcx, |panel, cx| {
        panel.working_dir = Some(repo.clone());
        assert!(panel.handle_slash_command_for_test("/changes", cx));
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        vcx.run_until_parked();
        let done = panel.read_with(vcx, |panel, _| {
            panel.items.iter().any(|item| matches!(item, Item::Tool { call_id, .. } if call_id.starts_with("git-diff-")))
                || panel.items.iter().any(|item| matches!(item, Item::Error(_)))
        });
        if done || std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let (files, errors) = panel.read_with(vcx, |panel, _| {
        let files = panel.items.iter().find_map(|item| match item {
            Item::Tool { call_id, name, input, .. } if call_id.starts_with("git-diff-") => {
                Some(crate::diff::tool_diffs(name, input))
            }
            _ => None,
        });
        let errors: Vec<String> = panel.items.iter().filter_map(|item| match item {
            Item::Error(e) => Some(e.clone()),
            _ => None,
        }).collect();
        (files, errors)
    });
    assert!(errors.is_empty(), "{errors:?}");
    let files = files.expect("/changes produced a diff card");
    assert!(!files.is_empty());
    assert!(files.iter().all(|f| !f.path.starts_with("a/") && !f.path.starts_with("i/")), "{:?}", files.iter().map(|f| &f.path).collect::<Vec<_>>());
    assert!(files.iter().any(|f| f.added() + f.removed() > 0));
    eprintln!("/changes files: {:?}", files.iter().map(|f| (&f.path, f.added(), f.removed())).collect::<Vec<_>>());
    // The rendered card opens the same review panel as agent edits.
    let counts = vcx.debug_bounds("edit-change-counts-0").expect("diff card rendered");
    vcx.simulate_click(counts.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
    let opened = workspace.read_with(vcx, |w, cx| {
        w.slots.iter().any(|slot| slot.panel.read(cx).is_change_review())
    });
    assert!(opened, "Review panel opened from /changes card");

    let branch = format!("e2e-ui-{}", std::process::id());
    let status_before = jcode_desktop_harness::session_git::Location::Wsl(None)
        .git(&repo, &["status", "--porcelain"])
        .unwrap();
    while commands.try_recv().is_ok() {}
    vcx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            let source = workspace.slots[0].panel.entity_id();
            workspace.workspace_slash_command(
                &WorkspaceSlashCommand { source, content: format!("/wt {branch}").into() },
                window,
                cx,
            );
        })
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let created = loop {
        vcx.run_until_parked();
        match commands.try_recv() {
            Ok(Command::CreateRemoteSession { host, working_dir, .. }) => break Some((host, working_dir)),
            Ok(_) => continue,
            Err(_) if std::time::Instant::now() > deadline => break None,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    };
    let (host, dir) = created.expect("/wt requested a WSL session");
    assert_eq!(host, "wsl");
    let dir = dir.expect("worktree directory");
    assert!(dir.ends_with(&format!("-worktrees/{branch}")), "{dir}");
    let wsl = jcode_desktop_harness::session_git::Location::Wsl(None);
    assert_eq!(wsl.git(&dir, &["branch", "--show-current"]).unwrap().trim(), branch);
    assert_eq!(wsl.git(&repo, &["status", "--porcelain"]).unwrap(), status_before);
    eprintln!("/wt created {dir} and requested a WSL session there");
}
