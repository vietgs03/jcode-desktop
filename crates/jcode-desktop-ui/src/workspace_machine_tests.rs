//! GPUI tests for the window machine: indicator, WSL Open folder, and new
//! sessions that follow the active panel.
use super::*;

fn drain(commands: &std::sync::mpsc::Receiver<Command>) {
    while commands.try_recv().is_ok() {}
}

fn remote_creates(commands: &std::sync::mpsc::Receiver<Command>) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    while let Ok(command) = commands.try_recv() {
        if let Command::CreateRemoteSession { host, working_dir, .. } = command {
            out.push((host, working_dir));
        }
    }
    out
}

#[gpui::test]
fn remote_indicator_switches_machine(cx: &mut gpui::TestAppContext) {
    let (bridge, _commands) = harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.bridge = bridge;
        workspace.remotes.default_host = None;
        workspace
    });
    vcx.run_until_parked();
    let indicator = vcx.debug_bounds("remote-indicator").expect("indicator rendered");
    vcx.simulate_click(indicator.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("remote-indicator-menu").is_some(), "menu opens");
    let wsl = vcx
        .debug_bounds("machine-menu-wsl")
        .or_else(|| vcx.debug_bounds("machine-menu-wsl-0"))
        .expect("WSL entry");
    vcx.simulate_click(wsl.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
    workspace.read_with(vcx, |w, _| {
        assert_eq!(w.remotes.default_host.as_deref(), Some("wsl"));
        assert!(!w.machine.menu_open);
        assert!(w.machine_label(w.remotes.default_host.as_deref()).starts_with("WSL"));
    });
    // And back to Windows.
    let indicator = vcx.debug_bounds("remote-indicator").unwrap();
    vcx.simulate_click(indicator.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
    let windows = vcx.debug_bounds("machine-menu-windows").expect("Windows entry");
    vcx.simulate_click(windows.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
    workspace.read_with(vcx, |w, _| assert_eq!(w.remotes.default_host, None));
}

#[gpui::test]
fn new_session_inherits_the_active_wsl_panel_directory(cx: &mut gpui::TestAppContext) {
    let (bridge, commands) = harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.bridge = bridge;
        workspace.remotes.default_host = None;
        workspace.push_test_panel("ssh://wsl/session_repo", cx);
        workspace
    });
    let panel = workspace.read_with(vcx, |w, _| w.test_panel(0).unwrap());
    panel.update(vcx, |panel, _| panel.working_dir = Some("/home/me/go-chathub".into()));
    drain(&commands);
    workspace.update(vcx, |w, cx| w.open_new_session(cx));
    vcx.run_until_parked();
    assert_eq!(
        remote_creates(&commands),
        vec![("wsl".to_string(), Some("/home/me/go-chathub".to_string()))]
    );
    // The new draft carries the directory, so a further Ctrl+N from it agrees.
    let draft_dir = workspace.read_with(vcx, |w, cx| w.slots[w.active].panel.read(cx).working_dir.clone());
    assert_eq!(draft_dir.as_deref(), Some("/home/me/go-chathub"));
    workspace.update(vcx, |w, cx| w.open_new_session(cx));
    vcx.run_until_parked();
    assert_eq!(
        remote_creates(&commands),
        vec![("wsl".to_string(), Some("/home/me/go-chathub".to_string()))]
    );
}

#[gpui::test]
fn pasted_wsl_unc_path_opens_a_wsl_session_there(cx: &mut gpui::TestAppContext) {
    let (bridge, commands) = harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.bridge = bridge;
        workspace.remotes.default_host = None;
        workspace
    });
    drain(&commands);
    workspace.update_in(vcx, |w, window, cx| {
        w.open_folder(&OpenFolder, window, cx);
        w.open_searched_folder(r"\\wsl.localhost\Ubuntu\home\me\proj", cx);
    });
    vcx.run_until_parked();
    let created = remote_creates(&commands);
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(created[0].1.as_deref(), Some("/home/me/proj"));
    workspace.read_with(vcx, |w, _| assert!(w.folder_picker_dir.is_none()));
}

#[gpui::test]
fn ctrl_o_on_wsl_lists_real_directories_and_enter_opens_there(cx: &mut gpui::TestAppContext) {
    let Ok(repo) = std::env::var("JCODE_WSL_TEST_REPO") else {
        return;
    };
    let parent = machine::parent_dir(&repo).expect("repo has a parent");
    let name = repo.rsplit('/').next().unwrap().to_owned();
    cx.executor().allow_parking();
    let (bridge, commands) = harness::spawn_recording();
    let (workspace, vcx) = cx.add_window_view(|_, cx| {
        let mut workspace = Workspace::for_test(learning::Coach::new(), cx);
        workspace.bridge = bridge;
        workspace.remotes.default_host = Some("wsl".into());
        workspace
    });
    workspace.update_in(vcx, |w, window, cx| w.open_folder(&OpenFolder, window, cx));
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("folder-picker-machine").is_some(), "WSL picker shown");
    workspace.update(vcx, |w, cx| w.wsl_browse_to(parent.clone(), cx));
    let deadline = Instant::now() + Duration::from_secs(30);
    let entries = loop {
        vcx.run_until_parked();
        let listing = workspace.read_with(vcx, |w, _| w.machine.listing("wsl", &parent).cloned());
        match listing {
            Some(machine::Listing::Ready(entries)) => break entries,
            Some(machine::Listing::Failed(error)) => panic!("listing failed: {error}"),
            _ if Instant::now() > deadline => panic!("listing timed out"),
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    eprintln!("WSL entries of {parent}: {entries:?}");
    assert!(entries.contains(&name), "{name} in {entries:?}");
    vcx.run_until_parked();
    assert!(vcx.debug_bounds("wsl-folder-entry-0").is_some(), "entries rendered");
    // Click into the repo, then Enter on an empty search opens it.
    let index = entries.iter().position(|e| *e == name).unwrap();
    let row = vcx.debug_bounds(Box::leak(format!("wsl-folder-entry-{index}").into_boxed_str())).expect("repo row");
    vcx.simulate_click(row.center(), gpui::Modifiers::default());
    vcx.run_until_parked();
    workspace.read_with(vcx, |w, _| assert_eq!(w.machine.browse.as_ref().unwrap().dir, repo));
    drain(&commands);
    workspace.update(vcx, |w, cx| w.open_searched_folder("", cx));
    vcx.run_until_parked();
    assert_eq!(remote_creates(&commands), vec![("wsl".to_string(), Some(repo.clone()))]);
    workspace.read_with(vcx, |w, cx| {
        assert!(w.folder_picker_dir.is_none() && w.machine.browse.is_none());
        assert_eq!(w.slots[w.active].panel.read(cx).working_dir.as_deref(), Some(repo.as_str()));
    });
}
