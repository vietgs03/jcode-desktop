//! Agents board: which swarm agents a session owns and what each is doing.
//!
//! A coordinator panel carries a calm one-line strip ("5 agents · 3 working ·
//! 1 done · 1 failed") that expands into a board with one row per agent:
//! status dot, task label, friendly name, the current activity line and how
//! long ago the swarm last reported. Clicking a row opens or focuses that
//! agent's conversation through the same path as the sidebar.
//!
//! Data comes from the bridge's swarm roster (local and WSL snapshots, see
//! `harness::swarm_state`) and from swarm ownership on catalog sessions.
//! Statuses describe the latest persisted snapshot, not a liveness probe.
use super::*;
use crate::harness::swarm_state::{Roster, SwarmMember};

/// Lifecycle buckets, in summary order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum AgentState {
    Working,
    Starting,
    Ready,
    Done,
    Blocked,
    Failed,
    Stopped,
    Other,
}

impl AgentState {
    pub(super) fn of(status: &str) -> Self {
        match status.trim().to_ascii_lowercase().as_str() {
            "running" | "working" | "active" | "busy" | "thinking" | "streaming" => Self::Working,
            "spawned" | "spawning" | "queued" | "starting" | "pending" => Self::Starting,
            "ready" | "idle" | "waiting" => Self::Ready,
            "completed" | "complete" | "done" => Self::Done,
            "blocked" | "waiting_network" => Self::Blocked,
            "failed" | "error" | "crashed" => Self::Failed,
            "stopped" | "closed" | "cancelled" | "canceled" => Self::Stopped,
            _ => Self::Other,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Other => "other",
        }
    }

    /// Board order: what needs the user first, then live work, then history.
    fn rank(self) -> u8 {
        match self {
            Self::Failed => 0,
            Self::Blocked => 1,
            Self::Working => 2,
            Self::Starting => 3,
            Self::Ready => 4,
            Self::Other => 5,
            Self::Done => 6,
            Self::Stopped => 7,
        }
    }

    pub(super) fn color(self) -> gpui::Rgba {
        let theme = Theme::global();
        match self {
            Self::Working => theme.ACCENT,
            Self::Starting => theme.ACCENT_MUTED,
            Self::Ready => theme.TEXT_DIM,
            Self::Done => theme.OK,
            Self::Blocked => theme.WARN,
            Self::Failed => theme.ERROR,
            Self::Stopped | Self::Other => theme.TEXT_FAINT,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct AgentRow {
    pub session_id: String,
    /// Stable task label, else the friendly name, else the session title.
    pub label: String,
    /// Friendly animal name, when it adds information beside the label.
    pub name: Option<String>,
    pub state: AgentState,
    /// The reported lifecycle string, kept for unknown states and tooltips.
    pub status: String,
    /// One-line, whitespace-collapsed current activity.
    pub activity: Option<String>,
    /// Full activity and latest report for the hover card.
    pub detail: String,
    pub updated_at_ms: Option<u64>,
    pub terminal_since_ms: Option<u64>,
    pub working_dir: Option<String>,
    pub depth: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct AgentsBoard {
    pub rows: Vec<AgentRow>,
}

impl AgentsBoard {
    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(super) fn count(&self, state: AgentState) -> usize {
        self.rows.iter().filter(|row| row.state == state).count()
    }

    /// `5 agents · 3 working · 1 done · 1 failed`.
    pub(super) fn summary(&self) -> String {
        let total = self.rows.len();
        let mut parts = vec![format!(
            "{total} agent{}",
            if total == 1 { "" } else { "s" }
        )];
        parts.extend(
            self.counts()
                .into_iter()
                .map(|(state, n)| format!("{n} {}", state.label())),
        );
        parts.join(" · ")
    }

    /// Non-zero counts per state, in summary order.
    pub(super) fn counts(&self) -> Vec<(AgentState, usize)> {
        [
            AgentState::Working,
            AgentState::Starting,
            AgentState::Ready,
            AgentState::Done,
            AgentState::Blocked,
            AgentState::Failed,
            AgentState::Stopped,
            AgentState::Other,
        ]
        .into_iter()
        .map(|state| (state, self.count(state)))
        .filter(|(_, n)| *n > 0)
        .collect()
    }

    /// Multi-line text for compact surfaces such as the sidebar badge.
    pub(super) fn tooltip(&self) -> String {
        let mut lines = vec![self.summary()];
        for row in &self.rows {
            let mut line = format!("● {} · {}", row.label, row.state_text());
            if let Some(activity) = &row.activity {
                line.push_str(&format!("\n   {}", clip(activity, 110)));
            }
            lines.push(line);
        }
        lines.push("Latest swarm snapshot, not a live connection check.".into());
        lines.join("\n")
    }
}

impl AgentRow {
    /// `working`, or the raw string for statuses Desktop does not know.
    pub(super) fn state_text(&self) -> String {
        if self.state == AgentState::Other && !self.status.is_empty() {
            self.status.clone()
        } else {
            self.state.label().to_owned()
        }
    }
}

/// Parent -> children lookup over the roster and the session catalog.
pub(super) struct Index<'a> {
    roster: &'a Roster,
    catalog: HashMap<&'a str, &'a jcode_sdk::SessionInfo>,
    children: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> Index<'a> {
    pub(super) fn new(roster: &'a Roster, sessions: &'a [jcode_sdk::SessionInfo]) -> Self {
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut seen = HashSet::new();
        for member in roster.values() {
            if let Some(parent) = member.parent_session_id.as_deref()
                && parent != member.session_id
                && seen.insert(member.session_id.as_str())
            {
                children
                    .entry(parent)
                    .or_default()
                    .push(member.session_id.as_str());
            }
        }
        let mut catalog = HashMap::new();
        for session in sessions {
            catalog.insert(session.session_id.as_str(), session);
            if let Some(parent) = session.parent_session_id.as_deref()
                && parent != session.session_id
                && seen.insert(session.session_id.as_str())
            {
                children
                    .entry(parent)
                    .or_default()
                    .push(session.session_id.as_str());
            }
        }
        Self {
            roster,
            catalog,
            children,
        }
    }

    pub(super) fn has_agents(&self, session_id: &str) -> bool {
        self.children.contains_key(session_id)
    }

    /// Every agent this session owns, depth-first, cycle-safe.
    pub(super) fn board(&self, session_id: &str) -> AgentsBoard {
        let mut rows = Vec::new();
        let mut visited = HashSet::from([session_id]);
        let mut pending = vec![(session_id, 0usize)];
        while let Some((parent, depth)) = pending.pop() {
            let Some(children) = self.children.get(parent) else {
                continue;
            };
            for child in children {
                if !visited.insert(child) {
                    continue;
                }
                rows.push(self.row(child, depth + 1));
                pending.push((child, depth + 1));
            }
        }
        rows.sort_by(|a, b| {
            a.state
                .rank()
                .cmp(&b.state.rank())
                .then_with(|| spawn_order(&a.session_id).cmp(&spawn_order(&b.session_id)))
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        AgentsBoard { rows }
    }

    fn row(&self, session_id: &str, depth: usize) -> AgentRow {
        let member = self.roster.get(session_id);
        let session = self.catalog.get(session_id).copied();
        row_from(session_id, member, session, depth)
    }
}

fn row_from(
    session_id: &str,
    member: Option<&SwarmMember>,
    session: Option<&jcode_sdk::SessionInfo>,
    depth: usize,
) -> AgentRow {
    let status = member
        .map(|m| m.status.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| session.and_then(|s| s.swarm_status.clone()))
        .unwrap_or_default();
    let state = AgentState::of(&status);
    let friendly = member
        .and_then(|m| m.friendly_name.clone())
        .or_else(|| jcode_core::id::extract_session_name(raw_id(session_id)).map(str::to_owned));
    let task = member
        .and_then(|m| m.task_label.clone())
        .or_else(|| session.and_then(|s| s.agent_label.clone()))
        .filter(|label| !label.trim().is_empty());
    let label = task
        .clone()
        .or_else(|| friendly.clone())
        .or_else(|| session.and_then(|s| s.title.clone()))
        .unwrap_or_else(|| raw_id(session_id).chars().take(12).collect());
    let name = friendly.filter(|name| task.is_some() && name != &label);
    let detail = member.and_then(|m| m.detail.clone());
    let report = member.and_then(|m| m.latest_completion_report.clone());
    // `detail` is the current activity, even on a ready/finished member.
    // A prior completion report must never conceal a new assignment or wait.
    let activity = detail
        .as_deref()
        .map(collapse)
        .filter(|line| !line.is_empty())
        .or_else(|| report.as_deref().and_then(first_meaningful_line));
    let mut full = Vec::new();
    full.push(format!(
        "{label} · {}",
        if status.is_empty() {
            "unknown".into()
        } else {
            status.clone()
        }
    ));
    if let Some(detail) = &detail {
        full.push(format!("Activity: {}", collapse(detail)));
    }
    if let Some(report) = &report {
        let plain = report.replace("**", "").replace('`', "");
        full.push(format!("Latest report: {}", clip(&collapse(&plain), 600)));
    }
    if let Some(dir) = member
        .and_then(|m| m.working_dir.clone())
        .or_else(|| session.and_then(|s| s.working_dir.clone()))
    {
        full.push(format!("Directory: {dir}"));
    }
    AgentRow {
        session_id: session_id.to_owned(),
        label: collapse(&label),
        name,
        state,
        status,
        activity,
        detail: full.join("\n"),
        updated_at_ms: member.and_then(|m| m.updated_at_ms),
        terminal_since_ms: member.and_then(|m| m.terminal_since_ms),
        working_dir: member
            .and_then(|m| m.working_dir.clone())
            .or_else(|| session.and_then(|s| s.working_dir.clone())),
        depth,
    }
}

/// The SDK session ID inside a namespaced `ssh://host/id` address.
fn raw_id(session_id: &str) -> &str {
    session_id
        .strip_prefix("ssh://")
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, id)| id)
        .unwrap_or(session_id)
}

/// Session IDs embed their creation time, which is spawn order for agents.
fn spawn_order(session_id: &str) -> u64 {
    raw_id(session_id)
        .split('_')
        .filter_map(|part| part.parse::<u64>().ok())
        .find(|value| (1_000_000_000_000..10_000_000_000_000).contains(value))
        .unwrap_or(u64::MAX)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped = text.chars().take(max).collect::<String>();
    clipped.push('…');
    clipped
}

/// First prose line of a markdown report, without heading/emphasis markers.
fn first_meaningful_line(text: &str) -> Option<String> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| {
            line.trim()
                .trim_start_matches(['#', '>', '-', '*', ' '])
                .replace("**", "")
                .replace('`', "")
        })
        .map(|line| collapse(&line))
        .find(|line| !line.is_empty() && !line.starts_with('|'))
}

/// Swarm tool calls change membership or assignments. Refresh the catalog
/// (and with it the roster) instead of polling.
pub(super) fn refresh_after(event: &jcode_sdk::ApiEvent) -> bool {
    match event {
        jcode_sdk::ApiEvent::ToolDone { name, .. } => matches!(
            name.trim_start_matches("functions."),
            "swarm" | "communicate"
        ),
        _ => false,
    }
}

pub(super) fn ago(ms: u64, now_ms: u64) -> String {
    format_time_ago(ms, now_ms)
}

pub(super) struct AgentTooltip(pub String);

impl Render for AgentTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let _render_scope = crate::render_stats::scope("AgentTooltip");
        div()
            .px_3()
            .py_2()
            .max_w(px(440.0))
            .rounded_md()
            .bg(Theme::global().HEADER_BG)
            .border_1()
            .border_color(Theme::global().PANEL_BORDER)
            .text_size(px(11.0))
            .line_height(relative(1.45))
            .text_color(Theme::global().TEXT)
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(self.0.lines().map(|line| div().child(line.to_owned())))
    }
}

fn dot(state: AgentState, size: f32) -> gpui::Div {
    div()
        .flex_none()
        .size(px(size))
        .rounded_full()
        .bg(state.color())
}

impl Workspace {
    /// Agents owned by `session_id`, from the roster and catalog ownership.
    pub(super) fn agents_board(&self, session_id: &str) -> AgentsBoard {
        Index::new(&self.swarm_roster, &self.sessions).board(session_id)
    }

    /// Open (or focus) an agent's conversation, reusing a mounted panel.
    pub(super) fn open_agent(
        &mut self,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = self
            .sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .cloned()
            .unwrap_or_else(|| {
                let member = self.swarm_roster.get(session_id);
                jcode_sdk::SessionInfo {
                    user_connected: false,
                    session_id: session_id.to_owned(),
                    working_dir: member.and_then(|m| m.working_dir.clone()),
                    title: member.and_then(|m| m.task_label.clone().or(m.friendly_name.clone())),
                    status: "idle".into(),
                    transcript_bytes: None,
                    saved: false,
                    updated_at_ms: None,
                    last_active_at_ms: None,
                    archived: false,
                    archived_at_ms: None,
                    save_label: None,
                    parent_session_id: member.and_then(|m| m.parent_session_id.clone()),
                    agent_label: member.and_then(|m| m.task_label.clone()),
                    swarm_status: member.map(|m| m.status.clone()).filter(|s| !s.is_empty()),
                    edit_stats: None,
                }
            });
        self.activate_session(session, window, cx);
    }

    /// Open the coordinator and unfold its board (sidebar badge action).
    pub(super) fn open_agents_board(
        &mut self,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.agents_expanded.insert(session_id.to_owned());
        self.open_agent(session_id, window, cx);
    }

    /// Place the Agents strip (and board, when unfolded) above a panel.
    pub(super) fn wrap_with_agents(
        &self,
        session_id: &str,
        panel: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let board = self.agents_board(session_id);
        if board.is_empty() {
            return panel;
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_agents(session_id, &board, cx))
            .child(div().flex_1().min_h_0().child(panel))
            .into_any_element()
    }

    fn render_agents(
        &self,
        session_id: &str,
        board: &AgentsBoard,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::global();
        let expanded = self.agents_expanded.contains(session_id);
        let toggle_id = session_id.to_owned();
        let summary = board.summary();
        let now_ms = u64::try_from(unix_now_ms()).unwrap_or_default();
        let counts = board.counts();
        let strip = div()
            .id("agents-strip")
            .debug_selector(|| "agents-strip".into())
            .flex_none()
            .h(px(28.0))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme.PANEL_BORDER)
            .bg(theme.HEADER_BG)
            .cursor_pointer()
            .hover(|el| el.bg(theme.TOOL_BG))
            .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    if !this.agents_expanded.remove(&toggle_id) {
                        this.agents_expanded.insert(toggle_id.clone());
                    }
                    cx.notify();
                }),
            )
            .child(
                gpui::svg()
                    .data(include_bytes!("../../../assets/icons/swarm.svg").as_slice())
                    .size(px(12.0))
                    .text_color(theme.TEXT_DIM),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.TEXT)
                    .child(format!(
                        "{} agent{}",
                        board.rows.len(),
                        if board.rows.len() == 1 { "" } else { "s" }
                    )),
            )
            .child(
                div()
                    .debug_selector(|| "agents-summary".into())
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .overflow_hidden()
                    .text_size(px(11.0))
                    .text_color(theme.TEXT_DIM)
                    .children(counts.into_iter().map(|(state, n)| {
                        div()
                            .debug_selector(move || format!("agents-count-{}", state.label()))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .child(dot(state, 6.0))
                            .child(format!("{n} {}", state.label()))
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.0))
                    .text_color(theme.TEXT_FAINT)
                    .child(if expanded { "Hide" } else { "Show" }),
            )
            .tooltip(move |_, cx| {
                cx.new(|_| AgentTooltip(format!(
                    "{summary}\nClick to {} the agent board.\nStatus comes from the latest swarm snapshot.",
                    if expanded { "hide" } else { "show" }
                )))
                .into()
            });
        let mut container = div().flex_none().flex().flex_col().child(strip);
        if expanded {
            let rows = board
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| self.render_agent_row(index, row, now_ms, cx));
            container = container.child(
                div()
                    .id("agents-board")
                    .debug_selector(|| "agents-board".into())
                    .flex_none()
                    .max_h(px(264.0))
                    .overflow_y_scroll()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.PANEL_BORDER)
                    .bg(theme.PANEL_BG)
                    .children(rows),
            );
        }
        container.into_any_element()
    }

    fn render_agent_row(
        &self,
        index: usize,
        row: &AgentRow,
        now_ms: u64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::global();
        let open_id = row.session_id.clone();
        let viewing = self
            .slots
            .iter()
            .any(|slot| !slot.closing && slot.panel.read(cx).session_id == row.session_id);
        let when = match (row.state, row.terminal_since_ms, row.updated_at_ms) {
            (AgentState::Done | AgentState::Failed | AgentState::Stopped, Some(at), _) => {
                Some(ago(at, now_ms))
            }
            (_, _, Some(at)) => Some(ago(at, now_ms)),
            _ => None,
        };
        let tooltip = format!(
            "{}{}\nClick to {} this agent's conversation.",
            row.detail,
            when.as_deref()
                .map(|w| format!("\nLast swarm update {w}"))
                .unwrap_or_default(),
            if viewing { "focus" } else { "open" },
        );
        div()
            .id(gpui::SharedString::from(format!("agents-row-{}", row.session_id)))
            .debug_selector(move || format!("agents-row-{index}"))
            .mx_1()
            .px_2()
            .py(px(5.0))
            .pl(px(8.0 + 12.0 * row.depth.saturating_sub(1) as f32))
            .rounded_md()
            .cursor_pointer()
            .hover(|el| el.bg(theme.TOOL_BG))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.open_agent(&open_id, window, cx);
                }),
            )
            .tooltip(move |_, cx| cx.new(|_| AgentTooltip(tooltip.clone())).into())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(dot(row.state, 7.0))
                    .child(
                        div()
                            .debug_selector(move || format!("agents-row-label-{index}"))
                            .flex_none()
                            .max_w(px(220.0))
                            .truncate()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.TEXT)
                            .child(row.label.clone()),
                    )
                    .children(row.name.clone().map(|name| {
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(theme.TEXT_DIM)
                            .child(name)
                    }))
                    .child(div().flex_1())
                    .when(viewing, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(theme.TEXT_FAINT)
                                .child("open"),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(row.state.color())
                            .child(row.state_text()),
                    )
                    .children(when.map(|when| {
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(theme.TEXT_FAINT)
                            .child(when)
                    })),
            )
            .children(row.activity.clone().map(|activity| {
                div()
                    .debug_selector(move || format!("agents-row-activity-{index}"))
                    .pl(px(15.0))
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.0))
                    .text_color(theme.TEXT_DIM)
                    .child(activity)
            }))
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "workspace_agents_tests.rs"]
mod tests;
