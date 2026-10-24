//! Swarm view: a workspace-level overview of every recent swarm.
//!
//! Opened from the sidebar "swarms" button, Super+Shift+W / Ctrl+Alt+S, or
//! `/swarms`. One section per swarm (active first): coordinator, directory,
//! per-state counts and last update, then one row per member with status,
//! name, task, current activity, latest report and age. Clicking a member or
//! coordinator opens (or focuses) its conversation through `open_agent`.
//!
//! Data is the bridge roster (`Workspace.swarm_roster`); it refreshes with
//! the normal roster updates. Opening the view requests one catalog refresh.
use super::agents::{AgentState, AgentTooltip, ago};
use super::*;
use crate::harness::swarm_state::{Roster, SwarmMember};

/// Swarms with no update in this window are hidden.
pub(super) const RECENT_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Default)]
pub(super) struct SwarmViewState {
    pub running_only: bool,
    /// Members whose completion report is unfolded.
    pub expanded_reports: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct MemberRow {
    pub session_id: String,
    pub name: String,
    pub task: Option<String>,
    pub state: AgentState,
    pub status: String,
    pub activity: Option<String>,
    pub report: Option<String>,
    pub headless: bool,
    pub updated_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct SwarmGroup {
    pub key: String,
    pub coordinator: Option<MemberRow>,
    pub working_dir: Option<String>,
    pub members: Vec<MemberRow>,
    pub updated_at_ms: Option<u64>,
}

impl AgentState {
    pub(super) fn is_live(self) -> bool {
        matches!(self, Self::Working | Self::Starting | Self::Blocked)
    }
}

impl SwarmGroup {
    pub(super) fn count(&self, states: &[AgentState]) -> usize {
        self.members
            .iter()
            .filter(|m| states.contains(&m.state))
            .count()
    }

    /// `2 running · 1 ready · 1 done · 1 failed`, zero buckets omitted.
    pub(super) fn counts_text(&self) -> String {
        let buckets: [(&str, &[AgentState]); 4] = [
            (
                "running",
                &[AgentState::Working, AgentState::Starting, AgentState::Blocked],
            ),
            ("ready", &[AgentState::Ready, AgentState::Other]),
            ("done", &[AgentState::Done, AgentState::Stopped]),
            ("failed", &[AgentState::Failed]),
        ];
        let parts: Vec<String> = buckets
            .iter()
            .map(|(label, states)| (label, self.count(states)))
            .filter(|(_, n)| *n > 0)
            .map(|(label, n)| format!("{n} {label}"))
            .collect();
        if parts.is_empty() {
            "no agents".into()
        } else {
            parts.join(" · ")
        }
    }

    pub(super) fn is_active(&self) -> bool {
        self.members.iter().any(|m| m.state.is_live())
            || self.coordinator.as_ref().is_some_and(|c| c.state.is_live())
    }

    pub(super) fn title(&self) -> String {
        match &self.coordinator {
            Some(c) => match &c.task {
                Some(task) if task != &c.name => format!("{} · {task}", c.name),
                _ => c.name.clone(),
            },
            None => format!("swarm {}", clip(&self.key, 18)),
        }
    }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn raw_id(session_id: &str) -> &str {
    session_id
        .strip_prefix("ssh://")
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, id)| id)
        .unwrap_or(session_id)
}

fn member_row(m: &SwarmMember) -> MemberRow {
    let name = m
        .friendly_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| jcode_core::id::extract_session_name(raw_id(&m.session_id)).map(str::to_owned))
        .unwrap_or_else(|| raw_id(&m.session_id).chars().take(12).collect());
    let report = m
        .latest_completion_report
        .as_deref()
        .map(|r| r.replace("**", "").replace('`', ""))
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty());
    MemberRow {
        session_id: m.session_id.clone(),
        name,
        task: m.task_label.clone().map(|t| collapse(&t)).filter(|t| !t.is_empty()),
        state: AgentState::of(&m.status),
        status: m.status.clone(),
        activity: m.detail.as_deref().map(collapse).filter(|d| !d.is_empty()),
        report,
        headless: m.is_headless,
        updated_at_ms: m.updated_at_ms.or(m.terminal_since_ms),
    }
}

fn rank(state: AgentState) -> u8 {
    match state {
        AgentState::Failed => 0,
        AgentState::Blocked => 1,
        AgentState::Working => 2,
        AgentState::Starting => 3,
        AgentState::Ready => 4,
        AgentState::Other => 5,
        AgentState::Done => 6,
        AgentState::Stopped => 7,
    }
}

/// Group the roster into swarms updated within `RECENT_MS` of `now_ms`,
/// active swarms first, then most recent.
pub(super) fn groups(roster: &Roster, now_ms: u64) -> Vec<SwarmGroup> {
    // Swarm key: swarm_id, else the coordinator a member reports to, else itself.
    let key_of = |m: &SwarmMember| -> String {
        m.swarm_id
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                m.parent_session_id
                    .as_deref()
                    .and_then(|p| roster.get(p))
                    .and_then(|p| p.swarm_id.clone())
            })
            .or_else(|| m.parent_session_id.clone())
            .unwrap_or_else(|| m.session_id.clone())
    };
    let mut by_key: HashMap<String, Vec<&SwarmMember>> = HashMap::new();
    for m in roster.values() {
        by_key.entry(key_of(m)).or_default().push(m);
    }
    let mut out = Vec::new();
    for (key, members) in by_key {
        let parents: HashSet<&str> = members
            .iter()
            .filter_map(|m| m.parent_session_id.as_deref())
            .collect();
        let coordinator = members
            .iter()
            .find(|m| m.is_coordinator())
            .or_else(|| members.iter().find(|m| parents.contains(m.session_id.as_str())))
            .or_else(|| {
                // A coordinator referenced by agents but missing from this group.
                None
            })
            .copied();
        let mut rows: Vec<MemberRow> = members
            .iter()
            .filter(|m| Some(m.session_id.as_str()) != coordinator.map(|c| c.session_id.as_str()))
            .map(|m| member_row(m))
            .collect();
        if rows.is_empty() && coordinator.is_none() {
            continue;
        }
        rows.sort_by(|a, b| {
            rank(a.state)
                .cmp(&rank(b.state))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        let updated_at_ms = members
            .iter()
            .filter_map(|m| m.updated_at_ms.or(m.terminal_since_ms))
            .max();
        if updated_at_ms.is_some_and(|at| now_ms.saturating_sub(at) > RECENT_MS) {
            continue;
        }
        let working_dir = coordinator
            .and_then(|c| c.working_dir.clone())
            .or_else(|| members.iter().find_map(|m| m.working_dir.clone()));
        let working_dir = working_dir.map(|dir| {
            match coordinator
                .map(|c| c.session_id.as_str())
                .or_else(|| members.first().map(|m| m.session_id.as_str()))
                .and_then(harness::remote_host)
            {
                Some(host) => format!("{host} · {dir}"),
                None => compact_working_dir(&dir),
            }
        });
        out.push(SwarmGroup {
            key,
            coordinator: coordinator.map(member_row),
            working_dir,
            members: rows,
            updated_at_ms,
        });
    }
    out.sort_by(|a, b| {
        b.is_active()
            .cmp(&a.is_active())
            .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
            .then_with(|| a.key.cmp(&b.key))
    });
    out
}

/// Agents (not coordinators) currently running across recent swarms.
pub(super) fn running_count(roster: &Roster, now_ms: u64) -> usize {
    groups(roster, now_ms)
        .iter()
        .map(|g| g.members.iter().filter(|m| m.state.is_live()).count())
        .sum()
}

fn dot(state: AgentState, size: f32) -> gpui::Div {
    div()
        .flex_none()
        .size(px(size))
        .rounded_full()
        .bg(state.color())
}

fn now_ms() -> u64 {
    u64::try_from(unix_now_ms()).unwrap_or_default()
}

impl Workspace {
    /// Sidebar row: "Swarms · 3 running". Always present so the view is
    /// discoverable; the count only shows while agents run.
    pub(super) fn render_sidebar_swarms_entry(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::global();
        let running = self.swarm_running_count();
        let open = self.swarm_view.is_some();
        div()
            .flex_none()
            .w_full()
            .min_w_0()
            .pl_2()
            .pr(px(crate::scrollbar::GUTTER + 4.0))
            .pt_2()
            .child(
                div()
                    .id("sidebar-swarms")
                    .debug_selector(|| "sidebar-swarms".into())
                    .w_full()
                    .min_w_0()
                    .h(px(26.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(11.0))
                    .text_color(if open { theme.TEXT } else { theme.TEXT_DIM })
                    .when(open, |el| el.bg(theme.TOOL_BG))
                    .hover(|el| el.bg(theme.TOOL_BG).text_color(theme.TEXT))
                    .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_mouse_up(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.toggle_swarm_view(&ToggleSwarmView, window, cx);
                        }),
                    )
                    .tooltip(|_, cx| {
                        cx.new(|_| {
                            AgentTooltip(
                                "Swarm view: every recent swarm and what each agent is doing.\nCtrl+Alt+S (Super+Shift+W), or /swarms."
                                    .into(),
                            )
                        })
                        .into()
                    })
                    .child(
                        gpui::svg()
                            .data(include_bytes!("../../../assets/icons/swarm.svg").as_slice())
                            .flex_none()
                            .size(px(12.0))
                            .text_color(theme.TEXT_DIM),
                    )
                    .child(div().flex_none().child("Swarms"))
                    .when(running > 0, |el| {
                        el.child(
                            div()
                                .debug_selector(|| "sidebar-swarms-count".into())
                                .min_w_0()
                                .truncate()
                                .text_color(theme.ACCENT)
                                .child(format!("· {running} running")),
                        )
                    }),
            )
            .into_any_element()
    }

    pub(super) fn swarm_running_count(&self) -> usize {
        running_count(&self.swarm_roster, now_ms())
    }

    pub(super) fn toggle_swarm_view(
        &mut self,
        _: &ToggleSwarmView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.swarm_view.is_some() {
            self.close_swarm_view(window, cx);
        } else {
            self.open_swarm_view(cx);
        }
    }

    pub(super) fn open_swarm_view(&mut self, cx: &mut Context<Self>) {
        if self.swarm_view.is_none() {
            self.swarm_view = Some(SwarmViewState::default());
            // One on-demand refresh; liveness then follows normal roster updates.
            self.bridge.send(Command::RefreshSessions);
        }
        self.overview = false;
        self.overview_progress.set(0.0, Instant::now());
        cx.notify();
    }

    pub(super) fn close_swarm_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.swarm_view.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn swarm_view_open_member(&mut self, session_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.swarm_view = None;
        self.open_agent(session_id, window, cx);
        cx.notify();
    }

    pub(super) fn render_swarm_view(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::global();
        let state = self.swarm_view.clone().unwrap_or_default();
        let now = now_ms();
        let all = groups(&self.swarm_roster, now);
        let latest = all.iter().filter_map(|g| g.updated_at_ms).max();
        let running = running_count(&self.swarm_roster, now);
        let visible: Vec<SwarmGroup> = all
            .into_iter()
            .filter_map(|mut g| {
                if state.running_only {
                    if !g.is_active() {
                        return None;
                    }
                    g.members.retain(|m| m.state.is_live());
                }
                Some(g)
            })
            .collect();

        let filter_button = |id: &'static str, label: &'static str, active: bool, running_only: bool, cx: &mut Context<Self>| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .flex_none()
                .px_2()
                .py(px(3.0))
                .rounded_md()
                .text_size(px(11.0))
                .cursor_pointer()
                .border_1()
                .border_color(if active { theme.ACCENT_MUTED } else { theme.PANEL_BORDER })
                .text_color(if active { theme.TEXT } else { theme.TEXT_DIM })
                .when(active, |el| el.bg(theme.TOOL_BG))
                .hover(|el| el.bg(theme.TOOL_BG))
                .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(view) = this.swarm_view.as_mut() {
                            view.running_only = running_only;
                        }
                        cx.notify();
                    }),
                )
                .child(label)
        };

        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .h(px(44.0))
            .border_b_1()
            .border_color(theme.PANEL_BORDER)
            .bg(theme.HEADER_BG)
            .child(
                gpui::svg()
                    .data(include_bytes!("../../../assets/icons/swarm.svg").as_slice())
                    .size(px(14.0))
                    .text_color(theme.TEXT_DIM),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(14.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.TEXT)
                    .child("Swarms"),
            )
            .child(
                div()
                    .debug_selector(|| "swarm-view-running".into())
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(if running > 0 { theme.ACCENT } else { theme.TEXT_FAINT })
                    .child(format!("{running} running")),
            )
            .child(
                div()
                    .debug_selector(|| "swarm-view-updated".into())
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.0))
                    .text_color(theme.TEXT_FAINT)
                    .child(match latest {
                        Some(at) => format!("updated {}", ago(at, now)),
                        None => "no recent snapshots".into(),
                    }),
            )
            .child(filter_button(
                "swarm-view-filter-running",
                "Running",
                state.running_only,
                true,
                cx,
            ))
            .child(filter_button(
                "swarm-view-filter-all",
                "All",
                !state.running_only,
                false,
                cx,
            ))
            .child(
                div()
                    .id("swarm-view-close")
                    .debug_selector(|| "swarm-view-close".into())
                    .flex_none()
                    .px_2()
                    .py(px(3.0))
                    .rounded_md()
                    .text_size(px(11.0))
                    .text_color(theme.TEXT_DIM)
                    .cursor_pointer()
                    .hover(|el| el.bg(theme.TOOL_BG).text_color(theme.TEXT))
                    .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_mouse_up(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_swarm_view(window, cx);
                        }),
                    )
                    .child("Close (Esc)"),
            );

        let body: gpui::AnyElement = if visible.is_empty() {
            div()
                .debug_selector(|| "swarm-view-empty".into())
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_8()
                .text_size(px(12.0))
                .text_color(theme.TEXT_DIM)
                .child(if state.running_only {
                    "No swarm agents are running right now. Switch to All to see recent swarms."
                } else {
                    "No swarms in the last 24 hours. Ask a session to spawn agents with the swarm tool and they will appear here."
                })
                .into_any_element()
        } else {
            let sections: Vec<gpui::AnyElement> = visible
                .iter()
                .enumerate()
                .map(|(gi, group)| self.render_swarm_group(gi, group, &state, now, cx))
                .collect();
            div()
                .id("swarm-view-list")
                .debug_selector(|| "swarm-view-list".into())
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p_4()
                .flex()
                .flex_col()
                .gap_4()
                .children(sections)
                .into_any_element()
        };

        div()
            .id("swarm-view")
            .debug_selector(|| "swarm-view".into())
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme.BG)
            // Windows draws caption buttons over the top-right corner.
            .when(crate::window_caption::enabled(), |el| {
                el.pt(px(crate::window_caption::HEIGHT))
            })
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_swarm_group(
        &self,
        gi: usize,
        group: &SwarmGroup,
        state: &SwarmViewState,
        now: u64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::global();
        let title = group.title();
        let coordinator_id = group.coordinator.as_ref().map(|c| c.session_id.clone());
        let coordinator_state = group.coordinator.as_ref().map(|c| c.state);
        let counts = group.counts_text();
        let mut head = div()
            .id(gpui::SharedString::from(format!("swarm-view-head-{}", group.key)))
            .debug_selector(move || format!("swarm-head-{gi}"))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.PANEL_BORDER)
            .bg(theme.HEADER_BG)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .children(coordinator_state.map(|s| dot(s, 8.0)))
                    .child(
                        div()
                            .debug_selector(move || format!("swarm-title-{gi}"))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.TEXT)
                            .child(title),
                    )
                    .child(
                        div()
                            .debug_selector(move || format!("swarm-counts-{gi}"))
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(theme.TEXT_DIM)
                            .child(counts),
                    )
                    .children(group.updated_at_ms.map(|at| {
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(theme.TEXT_FAINT)
                            .child(ago(at, now))
                    })),
            )
            .children(group.working_dir.clone().map(|dir| {
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.0))
                    .text_color(theme.TEXT_FAINT)
                    .child(dir)
            }));
        if let Some(id) = coordinator_id {
            head = head
                .cursor_pointer()
                .hover(|el| el.bg(theme.TOOL_BG))
                .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.swarm_view_open_member(&id, window, cx);
                    }),
                )
                .tooltip(|_, cx| {
                    cx.new(|_| AgentTooltip("Click to open the coordinator's conversation.".into()))
                        .into()
                });
        }
        let rows: Vec<gpui::AnyElement> = group
            .members
            .iter()
            .enumerate()
            .map(|(mi, row)| self.render_swarm_member(gi, mi, row, state, now, cx))
            .collect();
        div()
            .debug_selector(move || format!("swarm-group-{gi}"))
            .flex_none()
            .w_full()
            .min_w_0()
            .rounded_md()
            .border_1()
            .border_color(theme.PANEL_BORDER)
            .bg(theme.PANEL_BG)
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(head)
            .when(rows.is_empty(), |el| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_size(px(11.0))
                        .text_color(theme.TEXT_FAINT)
                        .child("No agents"),
                )
            })
            .children(rows)
            .into_any_element()
    }

    fn render_swarm_member(
        &self,
        gi: usize,
        mi: usize,
        row: &MemberRow,
        state: &SwarmViewState,
        now: u64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::global();
        let open_id = row.session_id.clone();
        let toggle_id = row.session_id.clone();
        let expanded = state.expanded_reports.contains(&row.session_id);
        let viewing = self
            .slots
            .iter()
            .any(|slot| !slot.closing && slot.panel.read(cx).session_id == row.session_id);
        let state_text = if row.state == AgentState::Other && !row.status.is_empty() {
            row.status.clone()
        } else {
            row.state.label().to_owned()
        };
        let mut tooltip = vec![format!(
            "{}{} · {state_text}",
            row.name,
            row.task.as_deref().map(|t| format!(" · {t}")).unwrap_or_default()
        )];
        if let Some(a) = &row.activity {
            tooltip.push(format!("Activity: {}", clip(a, 900)));
        }
        if let Some(r) = &row.report {
            tooltip.push(format!("Latest report: {}", clip(&collapse(r), 600)));
        }
        tooltip.push(format!(
            "Click to {} this agent's conversation.",
            if viewing { "focus" } else { "open" }
        ));
        let tooltip = tooltip.join("\n");
        let show_report = matches!(row.state, AgentState::Done | AgentState::Stopped | AgentState::Failed)
            && row.report.is_some();
        let report = row.report.clone();
        div()
            .id(gpui::SharedString::from(format!("swarm-view-member-{}", row.session_id)))
            .debug_selector(move || format!("swarm-member-{gi}-{mi}"))
            .w_full()
            .min_w_0()
            .px_3()
            .py(px(6.0))
            .border_b_1()
            .border_color(theme.PANEL_BORDER)
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
                    this.swarm_view_open_member(&open_id, window, cx);
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
                            .flex_none()
                            .w(px(64.0))
                            .truncate()
                            .text_size(px(10.0))
                            .text_color(row.state.color())
                            .child(state_text),
                    )
                    .child(
                        div()
                            .debug_selector(move || format!("swarm-member-name-{gi}-{mi}"))
                            .flex_none()
                            .max_w(px(140.0))
                            .truncate()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.TEXT)
                            .child(row.name.clone()),
                    )
                    .children(row.task.clone().map(|task| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.TEXT_DIM)
                            .child(task)
                    }))
                    .when(row.task.is_none(), |el| el.child(div().flex_1()))
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
                            .text_color(theme.TEXT_FAINT)
                            .child(if row.headless { "headless" } else { "visible" }),
                    )
                    .children(row.updated_at_ms.map(|at| {
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(theme.TEXT_FAINT)
                            .child(ago(at, now))
                    })),
            )
            .children(row.activity.clone().map(|activity| {
                div()
                    .debug_selector(move || format!("swarm-member-activity-{gi}-{mi}"))
                    .pl(px(15.0))
                    .min_w_0()
                    .line_clamp(2)
                    .text_size(px(11.0))
                    .text_color(theme.TEXT_DIM)
                    .child(activity)
            }))
            .when(show_report, |el| {
                let report = report.unwrap_or_default();
                let summary = report
                    .lines()
                    .map(|l| l.trim().trim_start_matches(['#', '>', '-', '*', ' ']).trim())
                    .find(|l| !l.is_empty() && !l.starts_with('|'))
                    .unwrap_or_default()
                    .to_owned();
                el.child(
                    div()
                        .id(gpui::SharedString::from(format!("swarm-view-report-{toggle_id}")))
                        .debug_selector(move || format!("swarm-member-report-{gi}-{mi}"))
                        .ml(px(15.0))
                        .min_w_0()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(theme.TOOL_BG)
                        .text_size(px(11.0))
                        .text_color(theme.TEXT_DIM)
                        .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                        })
                        .on_mouse_up(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(view) = this.swarm_view.as_mut()
                                    && !view.expanded_reports.remove(&toggle_id)
                                {
                                    view.expanded_reports.insert(toggle_id.clone());
                                }
                                cx.notify();
                            }),
                        )
                        .child(if expanded {
                            div()
                                .flex()
                                .flex_col()
                                .children(
                                    report
                                        .lines()
                                        .filter(|l| !l.trim().is_empty())
                                        .take(40)
                                        .map(|l| div().min_w_0().child(l.to_owned())),
                                )
                                .into_any_element()
                        } else {
                            div()
                                .min_w_0()
                                .truncate()
                                .child(format!("Report: {summary}  (click to expand)"))
                                .into_any_element()
                        }),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "workspace_swarm_view_tests.rs"]
mod tests;
