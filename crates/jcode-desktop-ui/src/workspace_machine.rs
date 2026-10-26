//! The window's target machine, VS Code Remote style.
//!
//! Windows (local), a WSL distribution, or a saved SSH host. New work (Open
//! folder, New session, recent projects) follows the chosen machine while
//! existing panels keep the machine they were created on. WSL directories are
//! listed through `wsl.exe` on background threads via [`Location`], never by
//! reading `\\wsl$` paths from Windows.
use super::*;
use jcode_desktop_harness::session_git::Location;

/// Directory listings stay valid this long before a revisit refreshes them.
const LISTING_TTL: Duration = Duration::from_secs(60);
/// The `~` repository scan is expensive, so it is reused for five minutes.
const REPO_SCAN_TTL: Duration = Duration::from_secs(300);
/// Recent projects shown in the machine menu.
const RECENT_LIMIT: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Machine {
    Local,
    /// `None` is the default distribution (`wsl` host).
    Wsl(Option<String>),
    Ssh(String),
}

impl Machine {
    pub(super) fn of_host(host: Option<&str>) -> Self {
        match host {
            None => Self::Local,
            Some(host) => match wsl_host_distro(host) {
                Some(distro) => Self::Wsl(distro),
                None => Self::Ssh(host.to_owned()),
            },
        }
    }
}

/// `wsl` is the default distribution, `<Distro>.wsl` a named one.
pub(super) fn wsl_host_distro(host: &str) -> Option<Option<String>> {
    if host.eq_ignore_ascii_case("wsl") {
        return Some(None);
    }
    let lower = host.to_ascii_lowercase();
    let stem = lower.strip_suffix(".wsl")?;
    (!stem.is_empty() && !stem.contains('@')).then(|| Some(host[..stem.len()].to_owned()))
}

/// The host whose sessions run on `distro`, preferring the plain `wsl` host
/// for the default distribution so it matches existing `ssh://wsl/` sessions.
pub(super) fn host_for_distro(distro: &str, default_distro: Option<&str>) -> String {
    match default_distro {
        Some(default) if !default.eq_ignore_ascii_case(distro) => format!("{distro}.wsl"),
        _ => "wsl".into(),
    }
}

/// Translate `\\wsl.localhost\Ubuntu\home\me` or `\\wsl$\Ubuntu\...` (either
/// slash style) into the distribution name and the Linux path inside it.
pub(super) fn wsl_unc_to_linux(path: &str) -> Option<(String, String)> {
    let normalized = path.trim().replace('\\', "/");
    let rest = normalized.strip_prefix("//")?;
    let (server, rest) = rest.split_once('/')?;
    if !server.eq_ignore_ascii_case("wsl.localhost") && !server.eq_ignore_ascii_case("wsl$") {
        return None;
    }
    let (distro, linux) = match rest.split_once('/') {
        Some((distro, linux)) => (distro, linux),
        None => (rest, ""),
    };
    if distro.is_empty() {
        return None;
    }
    let linux = linux.trim_end_matches('/');
    Some((distro.to_owned(), format!("/{linux}")))
}

/// wsl.exe prints its own messages (and `-l`) in UTF-16LE.
pub(super) fn decode_wsl_text(bytes: &[u8]) -> String {
    let nuls = bytes.iter().filter(|b| **b == 0).count();
    if bytes.len() >= 2 && nuls * 3 >= bytes.len() {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&units).trim_start_matches('\u{feff}').to_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Distro {
    pub name: String,
    pub default: bool,
}

/// Parse `wsl.exe -l -v`: a header row, then `* Name  State  Version` rows.
pub(super) fn parse_distros(raw: &str) -> Vec<Distro> {
    raw.lines()
        .skip(1)
        .filter_map(|line| {
            let line = line.trim_matches(|c: char| c.is_whitespace() || c == '\0');
            let (default, rest) = match line.strip_prefix('*') {
                Some(rest) => (true, rest.trim_start()),
                None => (false, line),
            };
            let name = rest.split_whitespace().next()?;
            (!name.is_empty()).then(|| Distro {
                name: name.to_owned(),
                default,
            })
        })
        .collect()
}

/// First line is the absolute directory (`pwd`), the rest are child names.
pub(super) fn parse_listing(raw: &str) -> Option<(String, Vec<String>)> {
    let mut lines = raw.lines().map(str::trim_end).filter(|line| !line.is_empty());
    let dir = lines.next()?.to_owned();
    if !dir.starts_with('/') {
        return None;
    }
    let mut entries: Vec<String> = lines.map(str::to_owned).collect();
    entries.sort_by_key(|name| name.to_lowercase());
    entries.dedup();
    Some((dir, entries))
}

/// First line is `$HOME`, then `./relative/.git` paths from `find`.
pub(super) fn parse_repo_scan(raw: &str) -> Vec<String> {
    let mut lines = raw.lines().map(str::trim_end).filter(|line| !line.is_empty());
    let Some(home) = lines.next().filter(|home| home.starts_with('/')) else {
        return Vec::new();
    };
    let home = home.trim_end_matches('/');
    let mut repos: Vec<String> = lines
        .filter_map(|line| {
            let relative = line.strip_prefix("./")?.strip_suffix("/.git")?;
            Some(format!("{home}/{relative}"))
        })
        .collect();
    repos.sort();
    repos.dedup();
    repos
}

pub(super) fn parent_dir(dir: &str) -> Option<String> {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return None;
    }
    let (parent, _) = trimmed.rsplit_once('/')?;
    Some(if parent.is_empty() { "/".into() } else { parent.into() })
}

pub(super) fn join_dir(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn base_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(path)
}

fn run(location: &Location, directory: &str, script: &str) -> Result<String, String> {
    let mut command = location.command(directory, "sh");
    command.arg("-c").arg(script);
    let output = location
        .output(&mut command)
        .map_err(|error| format!("Could not run wsl.exe: {error}"))?;
    let stdout = decode_wsl_text(&output.stdout);
    if !output.status.success() && stdout.trim().is_empty() {
        let stderr = decode_wsl_text(&output.stderr);
        return Err(match stderr.trim() {
            "" => format!("could not open {directory}"),
            message => message.to_owned(),
        });
    }
    if !output.status.success() && !stdout.starts_with('/') {
        return Err(stdout.trim().to_owned());
    }
    Ok(stdout)
}

/// List child directories of `directory` (`~` allowed) in one wsl.exe call.
pub(super) fn list_dirs(location: &Location, directory: &str) -> Result<(String, Vec<String>), String> {
    let raw = run(
        location,
        directory,
        "pwd; find . -mindepth 1 -maxdepth 1 -type d ! -name '.*' -printf '%f\\n' 2>/dev/null",
    )?;
    parse_listing(&raw).ok_or_else(|| format!("could not open {directory}"))
}

/// Git repositories under `~`, up to three directories deep.
pub(super) fn scan_repos(location: &Location) -> Result<Vec<String>, String> {
    let raw = run(
        location,
        "~",
        "pwd; find . -maxdepth 4 \\( -name .git -print -prune \\) -o \\( \\( -name '.?*' -o -name node_modules -o -name target \\) -prune \\) 2>/dev/null",
    )?;
    Ok(parse_repo_scan(&raw))
}

fn list_distros() -> Vec<Distro> {
    let mut command = std::process::Command::new("wsl.exe");
    command.args(["-l", "-v"]);
    jcode_desktop_harness::platform::no_console_window(&mut command);
    command
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| parse_distros(&decode_wsl_text(&output.stdout)))
        .unwrap_or_default()
}

/// Background work is skipped in unit tests unless a real WSL is provided.
fn wsl_probes_enabled() -> bool {
    cfg!(windows) && (!cfg!(test) || std::env::var_os("JCODE_WSL_TEST_REPO").is_some())
}

#[derive(Clone, Debug)]
pub(super) enum Listing {
    Loading,
    Ready(Vec<String>),
    Failed(String),
}

/// The folder picker browsing a WSL machine.
pub(super) struct Browse {
    pub host: String,
    pub location: Location,
    /// `~` until the first listing resolves it.
    pub dir: String,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) menu_open: bool,
    pub(super) browse: Option<Browse>,
    listings: HashMap<(String, String), (Listing, Instant)>,
    listing_tasks: HashMap<(String, String), gpui::Task<()>>,
    repos: HashMap<String, (Vec<String>, Instant)>,
    repo_tasks: HashMap<String, gpui::Task<()>>,
    distros: Option<Vec<Distro>>,
    distro_task: Option<gpui::Task<()>>,
}

impl State {
    pub(super) fn default_distro(&self) -> Option<&str> {
        self.distros
            .as_ref()?
            .iter()
            .find(|distro| distro.default)
            .map(|distro| distro.name.as_str())
    }

    pub(super) fn listing(&self, host: &str, dir: &str) -> Option<&Listing> {
        self.listings
            .get(&(host.to_owned(), dir.to_owned()))
            .map(|(listing, _)| listing)
    }
}

impl Workspace {
    pub(super) fn current_machine(&self) -> Machine {
        Machine::of_host(self.remotes.default_host.as_deref())
    }

    pub(super) fn machine_label(&self, host: Option<&str>) -> String {
        match Machine::of_host(host) {
            Machine::Local => "Windows".into(),
            Machine::Wsl(Some(distro)) => format!("WSL: {distro}"),
            Machine::Wsl(None) => match self.machine.default_distro() {
                Some(distro) => format!("WSL: {distro}"),
                None => "WSL".into(),
            },
            Machine::Ssh(host) if host == remotes::MANAGED_CLOUD_HOST => "Jcode Cloud".into(),
            Machine::Ssh(host) => format!("SSH: {host}"),
        }
    }

    fn ensure_distros(&mut self, cx: &mut Context<Self>) {
        if self.machine.distros.is_some() || self.machine.distro_task.is_some() || !wsl_probes_enabled() {
            return;
        }
        let task = cx.background_executor().spawn(async { list_distros() });
        self.machine.distro_task = Some(cx.spawn(async move |this, cx| {
            let distros = task.await;
            let _ = this.update(cx, |this, cx| {
                this.machine.distros = Some(distros);
                this.machine.distro_task = None;
                cx.notify();
            });
        }));
    }

    /// Recent working directories of sessions on `host` (local when `None`),
    /// most recently active first.
    pub(super) fn recent_projects(&self, host: Option<&str>) -> Vec<String> {
        let mut sessions: Vec<&jcode_sdk::SessionInfo> = self
            .sessions
            .iter()
            .filter(|session| harness::remote_host(&session.session_id).as_deref() == host)
            .filter(|session| session.working_dir.as_deref().is_some_and(|dir| !dir.is_empty()))
            .collect();
        sessions.sort_by_key(|session| {
            std::cmp::Reverse(session.last_active_at_ms.or(session.updated_at_ms).unwrap_or(0))
        });
        let mut seen = HashSet::new();
        sessions
            .into_iter()
            .filter_map(|session| session.working_dir.clone())
            .filter(|dir| seen.insert(dir.clone()))
            .collect()
    }

    /// Recent session directories first, then scanned repositories.
    fn wsl_project_candidates(&self, host: &str) -> Vec<(String, &'static str)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for dir in self.recent_projects(Some(host)) {
            if seen.insert(dir.clone()) {
                out.push((dir, "recent"));
            }
        }
        if let Some((repos, _)) = self.machine.repos.get(host) {
            for repo in repos {
                if seen.insert(repo.clone()) {
                    out.push((repo.clone(), "git repo"));
                }
            }
        }
        out
    }

    /// Ranked suggestions for a non-empty search query.
    pub(super) fn wsl_folder_matches(&self, host: &str, dir: &str, query: &str) -> Vec<(String, &'static str)> {
        let needle = query.trim().to_lowercase();
        let mut candidates = Vec::new();
        if let Some(Listing::Ready(entries)) = self.machine.listing(host, dir) {
            for name in entries {
                candidates.push((join_dir(dir, name), "folder"));
            }
        }
        candidates.extend(self.wsl_project_candidates(host));
        let mut seen = HashSet::new();
        let mut scored: Vec<(usize, usize, (String, &'static str))> = candidates
            .into_iter()
            .enumerate()
            .filter(|(_, (path, _))| seen.insert(path.clone()))
            .filter_map(|(order, (path, reason))| {
                let name = base_name(&path).to_lowercase();
                let score = if name == needle {
                    0
                } else if name.starts_with(&needle) {
                    1
                } else if name.contains(&needle) {
                    2
                } else if path.to_lowercase().contains(&needle) {
                    3
                } else {
                    return None;
                };
                Some((score, order, (path, reason)))
            })
            .collect();
        scored.sort_by_key(|(score, order, _)| (*score, *order));
        scored.into_iter().take(50).map(|(_, _, item)| item).collect()
    }

    fn request_listing(&mut self, dir: String, force: bool, cx: &mut Context<Self>) {
        let Some(browse) = &self.machine.browse else {
            return;
        };
        let key = (browse.host.clone(), dir.clone());
        let fresh = self
            .machine
            .listings
            .get(&key)
            .is_some_and(|(listing, at)| !matches!(listing, Listing::Failed(_)) && at.elapsed() < LISTING_TTL);
        if (!force && fresh) || self.machine.listing_tasks.contains_key(&key) {
            return;
        }
        if !self.machine.listings.contains_key(&key) {
            self.machine.listings.insert(key.clone(), (Listing::Loading, Instant::now()));
        }
        if !wsl_probes_enabled() {
            return;
        }
        let location = browse.location.clone();
        let task = cx.background_executor().spawn({
            let dir = dir.clone();
            async move { list_dirs(&location, &dir) }
        });
        self.machine.listing_tasks.insert(
            key.clone(),
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.machine.listing_tasks.remove(&key);
                    let (host, requested) = key;
                    let now = Instant::now();
                    match result {
                        Ok((resolved, entries)) => {
                            this.machine.listings.insert((host.clone(), resolved.clone()), (Listing::Ready(entries), now));
                            if requested != resolved {
                                this.machine.listings.remove(&(host.clone(), requested.clone()));
                            }
                            if let Some(browse) = this.machine.browse.as_mut().filter(|b| b.host == host && b.dir == requested) {
                                browse.dir = resolved;
                            }
                        }
                        Err(error) => {
                            this.machine.listings.insert((host, requested), (Listing::Failed(error), now));
                        }
                    }
                    cx.notify();
                });
            }),
        );
    }

    fn request_repo_scan(&mut self, cx: &mut Context<Self>) {
        let Some(browse) = &self.machine.browse else {
            return;
        };
        let host = browse.host.clone();
        let fresh = self.machine.repos.get(&host).is_some_and(|(_, at)| at.elapsed() < REPO_SCAN_TTL);
        if fresh || self.machine.repo_tasks.contains_key(&host) || !wsl_probes_enabled() {
            return;
        }
        let location = browse.location.clone();
        let task = cx.background_executor().spawn(async move { scan_repos(&location) });
        self.machine.repo_tasks.insert(
            host.clone(),
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.machine.repo_tasks.remove(&host);
                    if let Ok(repos) = result {
                        this.machine.repos.insert(host, (repos, Instant::now()));
                    }
                    cx.notify();
                });
            }),
        );
    }

    /// Ctrl+O on a WSL machine: browse that machine instead of Windows.
    /// Returns false when the current machine has no Desktop-side browser.
    pub(super) fn begin_machine_folder_picker(&mut self, cx: &mut Context<Self>) -> bool {
        let host = match &self.remotes.default_host {
            Some(host) => host.clone(),
            None => return false,
        };
        let Machine::Wsl(distro) = Machine::of_host(Some(&host)) else {
            return false;
        };
        self.start_wsl_browse(host, Location::Wsl(distro), None, cx);
        true
    }

    fn start_wsl_browse(&mut self, host: String, location: Location, dir: Option<String>, cx: &mut Context<Self>) {
        self.ensure_distros(cx);
        let dir = dir.unwrap_or_else(|| "~".into());
        // A resolved home listing makes reopening instant.
        let dir = if dir == "~" {
            self.machine
                .listings
                .keys()
                .filter(|(h, _)| *h == host)
                .find_map(|(_, d)| self.wsl_home_guess(&host, d))
                .unwrap_or(dir)
        } else {
            dir
        };
        self.machine.browse = Some(Browse { host, location, dir: dir.clone() });
        // Keep the shared overlay flag set. Its path is not used for WSL.
        self.folder_picker_dir = Some(PathBuf::from(&dir));
        self.request_listing(dir, false, cx);
        self.request_repo_scan(cx);
        cx.notify();
    }

    fn wsl_home_guess(&self, host: &str, dir: &str) -> Option<String> {
        let _ = host;
        let mut parts = dir.trim_start_matches('/').split('/');
        (parts.next() == Some("home")).then(|| parts.next().map(|user| format!("/home/{user}"))).flatten()
    }

    pub(super) fn wsl_browse_to(&mut self, dir: String, cx: &mut Context<Self>) {
        if let Some(browse) = self.machine.browse.as_mut() {
            browse.dir = dir.clone();
            self.folder_picker_dir = Some(PathBuf::from(&dir));
            self.folder_picker_error = None;
            if let Some(search) = &self.folder_search {
                search.update(cx, |search, cx| search.set_content(String::new(), cx));
            }
            self.request_listing(dir, false, cx);
            cx.notify();
        }
    }

    pub(super) fn end_machine_folder_picker(&mut self) {
        self.machine.browse = None;
    }

    /// Switch the open picker between Windows and the window's WSL machine.
    fn toggle_picker_machine(&mut self, cx: &mut Context<Self>) {
        if self.machine.browse.take().is_some() {
            self.folder_picker_dir = Some(
                self.default_working_dir()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/")),
            );
            cx.notify();
        } else {
            let host = match self.current_machine() {
                Machine::Wsl(_) => self.remotes.default_host.clone().unwrap_or_else(|| "wsl".into()),
                _ => "wsl".into(),
            };
            let distro = wsl_host_distro(&host).unwrap_or(None);
            self.start_wsl_browse(host, Location::Wsl(distro), None, cx);
        }
    }

    /// Open a new session in `directory` on WSL host `host`.
    pub(super) fn open_wsl_folder(&mut self, host: String, directory: String, cx: &mut Context<Self>) {
        let distro = wsl_host_distro(&host).unwrap_or(None);
        self.machine.browse = None;
        self.folder_picker_dir = None;
        self.folder_picker_error = None;
        self.folder_search = None;
        self.focus_pending = true;
        let title = base_name(&directory).to_owned();
        self.open_session_in(&Location::Wsl(distro), directory, title, cx);
        cx.notify();
    }

    /// Enter in the folder search. Handles pasted `\\wsl.localhost` paths on
    /// any machine, and every query while browsing WSL. Returns true if handled.
    pub(super) fn machine_open_searched(&mut self, query: &str, cx: &mut Context<Self>) -> bool {
        if self.folder_picker_sets_default {
            return false;
        }
        let query = query.trim();
        if let Some((distro, linux)) = wsl_unc_to_linux(query) {
            let host = match &self.machine.browse {
                Some(browse) if wsl_host_distro(&browse.host).flatten().is_none() && self.machine.default_distro().is_none() => browse.host.clone(),
                _ => host_for_distro(&distro, self.machine.default_distro()),
            };
            self.open_wsl_folder(host, linux, cx);
            return true;
        }
        let Some(browse) = &self.machine.browse else {
            return false;
        };
        let host = browse.host.clone();
        let dir = browse.dir.clone();
        let home = self.wsl_home_guess(&host, &dir);
        let target = if query.is_empty() || query == "." {
            Some(dir.clone())
        } else if query == "~" {
            Some(home.clone().unwrap_or_else(|| "~".into()))
        } else if let Some(rest) = query.strip_prefix("~/") {
            home.as_ref().map(|home| join_dir(home, rest))
        } else if query.starts_with('/') {
            Some(query.trim_end_matches('/').to_owned()).map(|p| if p.is_empty() { "/".into() } else { p })
        } else {
            self.wsl_folder_matches(&host, &dir, query).into_iter().next().map(|(path, _)| path)
        };
        match target {
            Some(path) => self.open_wsl_folder(host, path, cx),
            None => {
                self.folder_picker_error = Some(format!("no folder matches ‘{query}’"));
                cx.notify();
            }
        }
        true
    }

    /// New session (Ctrl+N, sidebar +) in the active panel's machine and
    /// directory, like opening another terminal tab in the same folder.
    /// Returns false when the active panel has no session to follow.
    pub(super) fn open_session_like_active(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(slot) = self.slots.get(self.active).filter(|slot| !slot.closing) else {
            return false;
        };
        let panel = slot.panel.read(cx);
        let id = panel.session_id.clone();
        let directory = panel.working_dir.clone().filter(|dir| !dir.is_empty());
        let remote = harness::remote_host(&id).or_else(|| pending::remote_draft_host(&id).map(str::to_owned));
        match remote {
            Some(host) => {
                let host = remotes::new_session_host(&host).to_owned();
                self.open_remote_draft_in(host, directory, cx);
                true
            }
            None => {
                let local_session = !id.contains("://")
                    || (id.starts_with("startup://draft/") && !id.starts_with("startup://draft/remote/"));
                match directory.filter(|_| local_session) {
                    Some(directory) => {
                        self.open_local_draft(Some(directory), cx);
                        true
                    }
                    None => false,
                }
            }
        }
    }

    /// Open a recent project on the window's machine.
    fn open_recent_project(&mut self, directory: String, cx: &mut Context<Self>) {
        self.machine.menu_open = false;
        self.focus_pending = true;
        match self.current_machine() {
            Machine::Local => self.open_local_draft(Some(directory), cx),
            Machine::Wsl(_) => {
                let host = self.remotes.default_host.clone().unwrap_or_else(|| "wsl".into());
                self.open_wsl_folder(host, directory, cx);
            }
            Machine::Ssh(host) => self.open_remote_draft_in(host, Some(directory), cx),
        }
        cx.notify();
    }

    fn choose_machine(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        self.machine.menu_open = false;
        self.set_default_machine(host, cx);
    }

    /// `>< WSL: Ubuntu` at the bottom-left of the sidebar, with its menu.
    pub(super) fn render_remote_indicator(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::global();
        let host = self.remotes.default_host.clone();
        let label = self.machine_label(host.as_deref());
        let open = self.machine.menu_open;
        let mut root = div()
            .debug_selector(|| "remote-indicator-area".into())
            .flex_none()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.PANEL_BORDER)
            .text_size(px(11.0));
        if open {
            root = root.child(self.render_machine_menu(host.as_deref(), cx));
        }
        root.child(
            div()
                .id("remote-indicator")
                .debug_selector(|| "remote-indicator".into())
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1p5()
                .cursor_pointer()
                .hover(|el| el.bg(theme.TOOL_BG))
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.machine.menu_open = !this.machine.menu_open;
                    if this.machine.menu_open {
                        this.ensure_distros(cx);
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.ACCENT_DIM)
                        .text_color(theme.ACCENT)
                        .child("><"),
                )
                .child(
                    div()
                        .debug_selector(|| "remote-indicator-label".into())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.TEXT)
                        .child(label),
                )
                .child(div().flex_none().text_color(theme.TEXT_DIM).child(if open { "▾" } else { "▴" })),
        )
        .into_any_element()
    }

    fn render_machine_menu(&self, host: Option<&str>, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::global();
        let current = Machine::of_host(host);
        let row = |id: String, label: String, selected: bool| {
            let selector = id.clone();
            div()
                .id(gpui::ElementId::Name(id.into()))
                .debug_selector(move || selector.clone())
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .cursor_pointer()
                .rounded_md()
                .hover(|el| el.bg(theme.TOOL_BG))
                .when(selected, |el| el.text_color(theme.ACCENT))
                .when(!selected, |el| el.text_color(theme.TEXT))
                .child(div().flex_none().w(px(10.0)).child(if selected { "•" } else { "" }))
                .child(div().flex_1().min_w_0().truncate().child(label))
        };
        let section = |title: &'static str| {
            div()
                .px_3()
                .pt_1p5()
                .pb_0p5()
                .text_size(px(10.0))
                .text_color(theme.TEXT_DIM)
                .child(title)
        };
        let mut menu = div()
            .id("remote-indicator-menu")
            .debug_selector(|| "remote-indicator-menu".into())
            .flex()
            .flex_col()
            .py_1()
            .max_h(px(360.0))
            .overflow_y_scroll()
            .bg(theme.PANEL_BG)
            .child(section("Machine"))
            .child(
                row("machine-menu-windows".into(), "Windows (local)".into(), current == Machine::Local)
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.choose_machine(None, cx);
                    })),
            );
        let default_distro = self.machine.default_distro().map(str::to_owned);
        let distros = self.machine.distros.clone().unwrap_or_default();
        if distros.is_empty() {
            menu = menu.child(
                row("machine-menu-wsl".into(), "WSL (default distribution)".into(), current == Machine::Wsl(None))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.choose_machine(Some("wsl".into()), cx);
                    })),
            );
        }
        for (index, distro) in distros.into_iter().enumerate() {
            let distro_host = host_for_distro(&distro.name, default_distro.as_deref());
            let selected = match &current {
                Machine::Wsl(None) => distro_host == "wsl",
                Machine::Wsl(Some(name)) => name.eq_ignore_ascii_case(&distro.name),
                _ => false,
            };
            menu = menu.child(
                row(format!("machine-menu-wsl-{index}"), format!("WSL: {}", distro.name), selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.choose_machine(Some(distro_host.clone()), cx);
                    })),
            );
        }
        let ssh: Vec<String> = self
            .remotes
            .hosts
            .iter()
            .filter(|h| wsl_host_distro(h).is_none() && h.as_str() != remotes::MANAGED_CLOUD_HOST && h.as_str() != "jcode-cloud-alpha")
            .cloned()
            .collect();
        for (index, ssh_host) in ssh.into_iter().enumerate() {
            let selected = current == Machine::Ssh(ssh_host.clone());
            menu = menu.child(
                row(format!("machine-menu-ssh-{index}"), format!("SSH: {ssh_host}"), selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.choose_machine(Some(ssh_host.clone()), cx);
                    })),
            );
        }
        let recent = self.recent_projects(match &current {
            Machine::Local => None,
            _ => host,
        });
        if !recent.is_empty() {
            menu = menu.child(section("Recent projects"));
            for (index, dir) in recent.into_iter().take(RECENT_LIMIT).enumerate() {
                let name = base_name(&dir).to_owned();
                let full = dir.clone();
                menu = menu.child(
                    row(format!("machine-menu-recent-{index}"), name, false)
                        .tooltip(move |_, cx| cx.new(|_| remotes::HeaderTooltip(full.clone().into())).into())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.open_recent_project(dir.clone(), cx);
                        })),
                );
            }
        }
        menu.child(div().mt_1().border_t_1().border_color(theme.PANEL_BORDER))
            .child(
                row("machine-menu-open-folder".into(), "Open folder…  Ctrl+O".into(), false).on_click(
                    cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.machine.menu_open = false;
                        this.open_folder(&OpenFolder, window, cx);
                    }),
                ),
            )
            .child(
                row("machine-menu-manage".into(), "Manage SSH machines…".into(), false).on_click(cx.listener(
                    |this, _, window, cx| {
                        cx.stop_propagation();
                        this.machine.menu_open = false;
                        this.open_machines(window, cx);
                    },
                )),
            )
            .into_any_element()
    }

    /// The Ctrl+O overlay while browsing a WSL machine.
    pub(super) fn render_wsl_folder_picker(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let browse = self.machine.browse.as_ref()?;
        let theme = Theme::global();
        let host = browse.host.clone();
        let dir = browse.dir.clone();
        let query = self
            .folder_search
            .as_ref()
            .map(|search| search.read(cx).content.trim().to_string())
            .unwrap_or_default();
        let mut list = div()
            .id("folder-picker-list")
            .debug_selector(|| "folder-picker-list".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .border_t_1()
            .border_b_1()
            .border_color(theme.PANEL_BORDER);
        let entry = |id: String, label: String, reason: String| {
            let selector = id.clone();
            div()
                .id(gpui::ElementId::Name(id.into()))
                .debug_selector(move || selector.clone())
                .px_4()
                .py_2()
                .cursor_pointer()
                .hover(|el| el.bg(theme.HEADER_BG))
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(div().flex_1().min_w_0().truncate().child(label))
                .when(!reason.is_empty(), |el| {
                    el.child(div().flex_none().text_size(px(10.0)).text_color(theme.TEXT_DIM).child(reason))
                })
        };
        if query.is_empty() {
            if let Some(parent) = parent_dir(&dir) {
                list = list.child(
                    entry("folder-picker-parent".into(), "↰  ..".into(), String::new())
                        .text_color(theme.TEXT_DIM)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.wsl_browse_to(parent.clone(), cx);
                        })),
                );
            }
            match self.machine.listing(&host, &dir) {
                Some(Listing::Ready(entries)) => {
                    if entries.is_empty() {
                        list = list.child(div().px_4().py_2().text_color(theme.TEXT_DIM).child("No folders here"));
                    }
                    for (index, name) in entries.iter().enumerate() {
                        let path = join_dir(&dir, name);
                        list = list.child(
                            entry(format!("wsl-folder-entry-{index}"), format!("▸  {name}"), String::new()).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.wsl_browse_to(path.clone(), cx);
                                }),
                            ),
                        );
                    }
                }
                Some(Listing::Failed(error)) => {
                    list = list.child(
                        div()
                            .debug_selector(|| "wsl-folder-error".into())
                            .px_4()
                            .py_2()
                            .text_color(theme.ERROR)
                            .child(error.clone()),
                    );
                }
                _ => {
                    list = list.child(
                        div()
                            .debug_selector(|| "wsl-folder-loading".into())
                            .px_4()
                            .py_2()
                            .text_color(theme.TEXT_DIM)
                            .child("Listing folders through WSL…"),
                    );
                }
            }
            let projects = self.wsl_project_candidates(&host);
            if !projects.is_empty() {
                list = list.child(
                    div().px_4().pt_3().pb_1().text_size(px(10.0)).text_color(theme.TEXT_DIM).child("Projects"),
                );
                for (index, (path, reason)) in projects.into_iter().take(12).enumerate() {
                    let host = host.clone();
                    let label = format!("{}  ·  {}", base_name(&path), compact_home(&path));
                    list = list.child(
                        entry(format!("wsl-folder-project-{index}"), label, reason.into()).on_click(cx.listener(
                            move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.open_wsl_folder(host.clone(), path.clone(), cx);
                            },
                        )),
                    );
                }
            }
        } else {
            let matches = self.wsl_folder_matches(&host, &dir, &query);
            if matches.is_empty() && wsl_unc_to_linux(&query).is_none() {
                list = list.child(div().px_4().py_2().text_color(theme.TEXT_DIM).child("No matching folders"));
            }
            for (index, (path, reason)) in matches.into_iter().enumerate() {
                let host = host.clone();
                let label = format!("{}  ·  {}", base_name(&path), compact_home(&path));
                list = list.child(
                    entry(format!("wsl-folder-match-{index}"), label, reason.into()).on_click(cx.listener(
                        move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.open_wsl_folder(host.clone(), path.clone(), cx);
                        },
                    )),
                );
            }
        }
        let chip = |id: &'static str, label: String| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .bg(theme.HEADER_BG)
                .hover(|el| el.text_color(theme.TEXT))
                .child(label)
        };
        let machine_label = self.machine_label(Some(&host));
        let card = div()
            .w(px(680.0))
            .h(px(560.0))
            .max_w(relative(0.9))
            .max_h(relative(0.85))
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(theme.PANEL_BORDER_FOCUS)
            .bg(theme.PANEL_BG)
            .child(
                div()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(div().text_size(px(14.0)).child("open folder"))
                            .child(
                                div()
                                    .debug_selector(|| "folder-picker-machine".into())
                                    .px_1p5()
                                    .rounded_sm()
                                    .bg(theme.ACCENT_DIM)
                                    .text_color(theme.ACCENT)
                                    .text_size(px(10.5))
                                    .child(format!(">< {machine_label}")),
                            ),
                    )
                    .child(
                        div()
                            .id("folder-picker-cancel")
                            .debug_selector(|| "folder-picker-cancel".into())
                            .cursor_pointer()
                            .text_color(theme.TEXT_DIM)
                            .hover(|el| el.text_color(theme.TEXT))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_folder_picker(cx);
                            }))
                            .child("cancel"),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "wsl-folder-path".into())
                    .px_4()
                    .pb_3()
                    .text_size(px(11.0))
                    .text_color(theme.TEXT_DIM)
                    .truncate()
                    .child(dir.clone()),
            )
            .child(
                div()
                    .px_4()
                    .pb_3()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .text_size(px(11.0))
                    .text_color(theme.TEXT_DIM)
                    .child(chip("folder-picker-home", "⌂  home".into()).on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.wsl_browse_to("~".into(), cx);
                    })))
                    .child(chip("folder-picker-computer", "▣  /".into()).on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.wsl_browse_to("/".into(), cx);
                    })))
                    .child(chip("folder-picker-refresh", "↻  refresh".into()).on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(dir) = this.machine.browse.as_ref().map(|b| b.dir.clone()) {
                            this.machine.repos.clear();
                            this.request_listing(dir, true, cx);
                            this.request_repo_scan(cx);
                        }
                        cx.notify();
                    })))
                    .child(
                        chip("folder-picker-machine-toggle", "Browse Windows instead".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_picker_machine(cx);
                            })),
                    ),
            )
            .when_some(self.folder_search.clone(), |el, search| {
                el.child(
                    div()
                        .id("folder-picker-search")
                        .debug_selector(|| "folder-picker-search".into())
                        .mx_4()
                        .mb_3()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.INPUT_BORDER)
                        .bg(theme.INPUT_BG)
                        .child(search),
                )
            })
            .child(list)
            .when_some(self.folder_picker_error.clone(), |el, error| {
                el.child(
                    div()
                        .debug_selector(|| "folder-picker-error".into())
                        .px_4()
                        .py_2()
                        .text_color(theme.ERROR)
                        .child(error),
                )
            })
            .child(
                div().p_3().flex().justify_end().child(
                    div()
                        .id("folder-picker-open")
                        .debug_selector(|| "folder-picker-open".into())
                        .px_4()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(theme.ACCENT)
                        .text_color(theme.BG)
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            let query = this
                                .folder_search
                                .as_ref()
                                .map(|s| s.read(cx).content.trim().to_string())
                                .unwrap_or_default();
                            this.machine_open_searched(&query, cx);
                        }))
                        .child("open this folder"),
                ),
            );
        Some(
            div()
                .id("folder-picker-overlay")
                .debug_selector(|| "folder-picker-overlay".into())
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                        if let Some(search) = &this.folder_search {
                            this.focus_pending = false;
                            window.focus(&search.read(cx).focus_handle.clone(), cx);
                        }
                    }),
                )
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x000000cc))
                .child(card)
                .into_any_element(),
        )
    }

    /// Windows picker footer chip: browse WSL instead.
    pub(super) fn render_picker_wsl_chip(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.folder_picker_sets_default || !cfg!(windows) {
            return None;
        }
        let theme = Theme::global();
        Some(
            div()
                .id("folder-picker-machine-toggle")
                .debug_selector(|| "folder-picker-machine-toggle".into())
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .bg(theme.HEADER_BG)
                .hover(|el| el.text_color(theme.TEXT))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_picker_machine(cx);
                    }),
                )
                .child("><  browse WSL")
                .into_any_element(),
        )
    }
}

fn compact_home(path: &str) -> String {
    let mut parts = path.splitn(4, '/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(""), Some("home"), Some(_user), Some(rest)) => format!("~/{rest}"),
        (Some(""), Some("home"), Some(_user), None) => "~".into(),
        _ => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_unc_paths_translate_to_linux_paths() {
        assert_eq!(
            wsl_unc_to_linux(r"\\wsl.localhost\Ubuntu\home\viethx\go-chathub"),
            Some(("Ubuntu".into(), "/home/viethx/go-chathub".into()))
        );
        assert_eq!(
            wsl_unc_to_linux(r"\\wsl$\Ubuntu-22.04\home\me\"),
            Some(("Ubuntu-22.04".into(), "/home/me".into()))
        );
        assert_eq!(wsl_unc_to_linux("//WSL.LOCALHOST/Debian"), Some(("Debian".into(), "/".into())));
        assert_eq!(wsl_unc_to_linux(r"C:\Users\me"), None);
        assert_eq!(wsl_unc_to_linux(r"\\server\share\x"), None);
        assert_eq!(wsl_unc_to_linux("/home/me"), None);
    }

    #[test]
    fn hosts_map_to_machines() {
        assert_eq!(Machine::of_host(None), Machine::Local);
        assert_eq!(Machine::of_host(Some("wsl")), Machine::Wsl(None));
        assert_eq!(Machine::of_host(Some("Ubuntu.wsl")), Machine::Wsl(Some("Ubuntu".into())));
        assert_eq!(Machine::of_host(Some("box")), Machine::Ssh("box".into()));
        assert_eq!(host_for_distro("Ubuntu", Some("Ubuntu")), "wsl");
        assert_eq!(host_for_distro("Debian", Some("Ubuntu")), "Debian.wsl");
        assert_eq!(host_for_distro("Debian", None), "wsl");
    }

    #[test]
    fn wsl_outputs_parse() {
        let utf16: Vec<u8> = "  NAME      STATE    VERSION\r\n* Ubuntu    Running  2\r\n  Debian    Stopped  2\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let distros = parse_distros(&decode_wsl_text(&utf16));
        assert_eq!(
            distros,
            vec![
                Distro { name: "Ubuntu".into(), default: true },
                Distro { name: "Debian".into(), default: false }
            ]
        );
        assert_eq!(
            parse_listing("/home/me\nzeta\nAlpha\n"),
            Some(("/home/me".into(), vec!["Alpha".into(), "zeta".into()]))
        );
        assert_eq!(parse_listing("oops"), None);
        assert_eq!(
            parse_repo_scan("/home/me\n./a/.git\n./b/c/.git\n"),
            vec!["/home/me/a".to_string(), "/home/me/b/c".into()]
        );
        assert_eq!(parent_dir("/home/me"), Some("/home".into()));
        assert_eq!(parent_dir("/home"), Some("/".into()));
        assert_eq!(parent_dir("/"), None);
        assert_eq!(compact_home("/home/me/x/y"), "~/x/y");
    }
}
