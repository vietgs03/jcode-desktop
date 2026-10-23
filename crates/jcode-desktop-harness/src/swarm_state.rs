//! Live-ish swarm roster for the Agents board: who each agent is and what it
//! is doing now, read from the daemon's durable swarm snapshots.
//!
//! Local sessions read `~/.jcode/state/swarm` directly. When Desktop runs on
//! Windows against a WSL daemon, the snapshots live inside the distribution,
//! so they are streamed through one `wsl.exe --exec sh` call (never the slow
//! `\\wsl.localhost` share) and every session ID is namespaced into the same
//! `ssh://<host>/` address space as the WSL session list.
//!
//! The roster describes the latest persisted snapshot, not a liveness probe.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use jcode_sdk::SessionInfo;

/// Snapshots older than this are history, not a running swarm. Bounding the
/// read keeps a refresh cheap even with months of accumulated swarms.
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Large snapshots are dominated by plan history. Skip pathological ones.
const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024;
/// ASCII record separator between concatenated snapshot files.
const RECORD_SEPARATOR: u8 = 0x1e;
const WSL_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// One swarm member, keyed in [`Roster`] by its Desktop session ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwarmMember {
    /// Desktop session ID (namespaced for remote hosts).
    pub session_id: String,
    /// The session this agent reports to (namespaced like `session_id`).
    pub parent_session_id: Option<String>,
    pub swarm_id: Option<String>,
    /// Raw lifecycle status (`running`, `ready`, `completed`, `failed`, ...).
    pub status: String,
    /// What the agent is doing now: its current task or latest message.
    pub detail: Option<String>,
    /// Stable lane/task label assigned at spawn.
    pub task_label: Option<String>,
    /// Short animal name (`boar`, `camel`, ...).
    pub friendly_name: Option<String>,
    /// `coordinator` or `agent`.
    pub role: Option<String>,
    pub is_headless: bool,
    pub latest_completion_report: Option<String>,
    pub working_dir: Option<String>,
    /// When the snapshot containing this record was written.
    pub updated_at_ms: Option<u64>,
    /// When the member entered a terminal state, if it did.
    pub terminal_since_ms: Option<u64>,
}

impl SwarmMember {
    pub fn is_coordinator(&self) -> bool {
        self.role.as_deref() == Some("coordinator")
    }
}

/// Session ID -> member, newest record per member.
pub type Roster = HashMap<String, SwarmMember>;

#[derive(serde::Deserialize)]
struct Snapshot {
    #[serde(default)]
    updated_at_unix_ms: u64,
    #[serde(default)]
    members: Vec<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct RawMember {
    session_id: String,
    #[serde(default)]
    swarm_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    task_label: Option<String>,
    #[serde(default)]
    friendly_name: Option<String>,
    #[serde(default)]
    report_back_to_session_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    is_headless: bool,
    #[serde(default)]
    latest_completion_report: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    terminal_since_unix_ms: Option<u64>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// Merge one snapshot's members into `roster`. The newest snapshot wins as a
/// whole record, never a mix of fields from different snapshots.
fn merge_snapshot(roster: &mut Roster, text: &[u8], host: Option<&str>) {
    let Ok(snapshot) = serde_json::from_slice::<Snapshot>(text) else {
        return;
    };
    let address = |id: String| match host {
        Some(host) => super::remote::namespace(host, &id),
        None => id,
    };
    for value in snapshot.members {
        let Ok(raw) = serde_json::from_value::<RawMember>(value) else {
            continue;
        };
        if raw.session_id.trim().is_empty() {
            continue;
        }
        let session_id = address(raw.session_id.clone());
        if roster.get(&session_id).is_some_and(|known| {
            known.updated_at_ms.unwrap_or_default() > snapshot.updated_at_unix_ms
        }) {
            continue;
        }
        let parent_session_id = non_empty(raw.report_back_to_session_id)
            .filter(|parent| parent != &raw.session_id)
            .map(address);
        roster.insert(
            session_id.clone(),
            SwarmMember {
                session_id,
                parent_session_id,
                swarm_id: non_empty(raw.swarm_id),
                status: raw.status.unwrap_or_default().trim().to_owned(),
                detail: non_empty(raw.detail),
                task_label: non_empty(raw.task_label),
                friendly_name: non_empty(raw.friendly_name),
                role: non_empty(raw.role),
                is_headless: raw.is_headless,
                latest_completion_report: non_empty(raw.latest_completion_report),
                working_dir: non_empty(raw.working_dir),
                updated_at_ms: (snapshot.updated_at_unix_ms > 0)
                    .then_some(snapshot.updated_at_unix_ms),
                terminal_since_ms: raw.terminal_since_unix_ms,
            },
        );
    }
}

/// Parse snapshot files concatenated with ASCII record separators, as
/// streamed from WSL. A truncated or malformed record only drops itself.
pub fn parse_stream(bytes: &[u8], host: Option<&str>, roster: &mut Roster) {
    for record in bytes.split(|byte| *byte == RECORD_SEPARATOR) {
        if record.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        merge_snapshot(roster, record, host);
    }
}

/// Read every recent snapshot in a local directory.
pub fn read_dir(dir: &Path, roster: &mut Roster) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let recent = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_none_or(|age| age <= MAX_SNAPSHOT_AGE);
        if !recent || metadata.len() > MAX_SNAPSHOT_BYTES {
            continue;
        }
        if let Ok(text) = std::fs::read(&path) {
            merge_snapshot(roster, &text, None);
        }
    }
}

/// This machine's snapshot directory, matching the SDK's resolution order.
fn local_dir() -> Option<std::path::PathBuf> {
    if let Ok(runtime) = std::env::var("JCODE_RUNTIME_DIR") {
        return Some(
            std::path::PathBuf::from(runtime)
                .join("durable-state")
                .join("swarm"),
        );
    }
    Some(super::jcode_home()?.join("state").join("swarm"))
}

/// The shell script run inside WSL. Only fixed text, no interpolation. It is
/// written to `sh -s` on stdin: Windows command-line quoting through
/// `wsl.exe` mangles nested quotes, and then `cat` waits on the terminal.
fn wsl_script() -> String {
    format!(
        "d=\"${{JCODE_HOME:-$HOME/.jcode}}/state/swarm\"; [ -d \"$d\" ] || exit 0; \
         find \"$d\" -maxdepth 1 -name '*.json' -mtime -{days} -size -{kib}k \
         -exec sh -c 'for f do cat \"$f\"; printf \"\\036\"; done' sh {{}} +\n",
        days = MAX_SNAPSHOT_AGE.as_secs() / 86_400,
        kib = MAX_SNAPSHOT_BYTES / 1024,
    )
}

/// Stream a WSL distribution's snapshots through one `wsl.exe` process.
fn read_wsl(host: &str, roster: &mut Roster) {
    let Some(distro) = jcode_sdk::wsl_distro(host) else {
        return;
    };
    let started = Instant::now();
    let mut command = std::process::Command::new("wsl.exe");
    if let Some(distro) = distro {
        command.arg("--distribution").arg(distro);
    }
    command
        .arg("--exec")
        .arg("sh")
        .arg("-s")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    crate::platform::no_console_window(&mut command);
    let Ok(mut child) = command.spawn() else {
        eprintln!("jcode desktop: {host} swarm state: could not start wsl.exe");
        return;
    };
    // Dropping stdin after the script closes it, so `sh -s` runs and exits.
    let script_written = child.stdin.take().is_some_and(|mut stdin| {
        std::io::Write::write_all(&mut stdin, wsl_script().as_bytes()).is_ok()
    });
    let Some(mut stdout) = child.stdout.take().filter(|_| script_written) else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut bytes);
        bytes
    });
    // A wedged distribution must not hold the session refresh forever.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < WSL_READ_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("jcode desktop: {host} swarm state timed out");
                return;
            }
        }
    }
    let Ok(bytes) = reader.join() else {
        return;
    };
    let before = roster.len();
    parse_stream(&bytes, Some(host), roster);
    eprintln!(
        "jcode desktop: {host} swarm state read in {:.1}ms ({} bytes, {} members)",
        started.elapsed().as_secs_f64() * 1_000.0,
        bytes.len(),
        roster.len() - before
    );
}

/// Fill missing ownership, labels and statuses on catalog sessions from the
/// roster. Values already supplied by the API (or the SDK's local
/// enrichment) win, exactly like `jcode_sdk::enrich_sessions_from_swarm_state`.
pub fn enrich_sessions(sessions: &mut [SessionInfo], roster: &Roster) {
    for session in sessions {
        let Some(member) = roster.get(&session.session_id) else {
            continue;
        };
        if session.parent_session_id.is_none() {
            session.parent_session_id = member.parent_session_id.clone();
        }
        if session.agent_label.is_none() {
            session.agent_label = member.task_label.clone();
        }
        if session.swarm_status.is_none() && !member.status.is_empty() {
            session.swarm_status = Some(member.status.clone());
        }
    }
}

/// Collect this machine's roster plus the WSL history host's, then enrich
/// the catalog so WSL agents nest under their WSL coordinator.
pub fn refresh(sessions: &mut [SessionInfo], wsl_host: Option<&str>) -> Roster {
    let mut roster = Roster::new();
    if let Some(dir) = local_dir() {
        read_dir(&dir, &mut roster);
    }
    if let Some(host) = wsl_host {
        read_wsl(host, &mut roster);
    }
    enrich_sessions(sessions, &roster);
    roster
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAPSHOT: &str = r#"{
        "swarm_id": "session:lead",
        "coordinator_session_id": "lead",
        "updated_at_unix_ms": 2000,
        "plan": {"items": [{"content": "ignored"}]},
        "members": [
            {"session_id": "lead", "status": "running", "role": "coordinator",
             "friendly_name": "calf", "report_back_to_session_id": null,
             "detail": "Swarm await finished"},
            {"session_id": "boar", "status": "running", "role": "agent",
             "friendly_name": "boar", "task_label": "pvp",
             "report_back_to_session_id": "lead", "is_headless": true,
             "detail": "Read docs/SWARM_TEST_RULES.md first"},
            {"session_id": "camel", "status": "completed", "role": "agent",
             "task_label": "e2e-events", "report_back_to_session_id": "lead",
             "latest_completion_report": "191/191 checks pass",
             "terminal_since_unix_ms": 1900},
            {"status": "running"}
        ]
    }"#;

    #[test]
    fn swarm_state_stream_namespaces_members_and_parents_for_wsl() {
        let mut stream = SNAPSHOT.as_bytes().to_vec();
        stream.push(RECORD_SEPARATOR);
        stream.extend_from_slice(b"{ truncated");
        stream.push(RECORD_SEPARATOR);
        let mut roster = Roster::new();
        parse_stream(&stream, Some("wsl"), &mut roster);
        assert_eq!(roster.len(), 3);
        let boar = &roster["ssh://wsl/boar"];
        assert_eq!(boar.parent_session_id.as_deref(), Some("ssh://wsl/lead"));
        assert_eq!(boar.task_label.as_deref(), Some("pvp"));
        assert_eq!(boar.friendly_name.as_deref(), Some("boar"));
        assert_eq!(
            boar.detail.as_deref(),
            Some("Read docs/SWARM_TEST_RULES.md first")
        );
        assert!(boar.is_headless);
        assert_eq!(boar.updated_at_ms, Some(2000));
        let lead = &roster["ssh://wsl/lead"];
        assert!(lead.is_coordinator());
        assert_eq!(lead.parent_session_id, None);
        let camel = &roster["ssh://wsl/camel"];
        assert_eq!(
            camel.latest_completion_report.as_deref(),
            Some("191/191 checks pass")
        );
        assert_eq!(camel.terminal_since_ms, Some(1900));
    }

    #[test]
    fn swarm_state_newest_snapshot_wins_as_a_whole_record() {
        let older = r#"{"updated_at_unix_ms": 1000, "members": [
            {"session_id": "boar", "status": "failed", "task_label": "old",
             "detail": "stale", "report_back_to_session_id": "other"}]}"#;
        let mut roster = Roster::new();
        parse_stream(SNAPSHOT.as_bytes(), None, &mut roster);
        parse_stream(older.as_bytes(), None, &mut roster);
        let boar = &roster["boar"];
        assert_eq!(boar.status, "running");
        assert_eq!(boar.task_label.as_deref(), Some("pvp"));
        assert_eq!(boar.parent_session_id.as_deref(), Some("lead"));
    }

    #[test]
    fn swarm_state_enriches_wsl_catalog_without_overriding_api_values() {
        let mut roster = Roster::new();
        parse_stream(SNAPSHOT.as_bytes(), Some("wsl"), &mut roster);
        let session = |id: &str| SessionInfo {
            user_connected: false,
            session_id: id.into(),
            working_dir: None,
            title: None,
            status: "idle".into(),
            transcript_bytes: None,
            saved: false,
            updated_at_ms: None,
            last_active_at_ms: None,
            archived: false,
            archived_at_ms: None,
            save_label: None,
            parent_session_id: None,
            agent_label: None,
            swarm_status: None,
            edit_stats: None,
        };
        let mut sessions = vec![
            session("ssh://wsl/lead"),
            session("ssh://wsl/boar"),
            session("ssh://wsl/camel"),
            session("boar"),
        ];
        sessions[2].agent_label = Some("from api".into());
        enrich_sessions(&mut sessions, &roster);
        assert_eq!(sessions[0].parent_session_id, None);
        assert_eq!(
            sessions[1].parent_session_id.as_deref(),
            Some("ssh://wsl/lead")
        );
        assert_eq!(sessions[1].agent_label.as_deref(), Some("pvp"));
        assert_eq!(sessions[1].swarm_status.as_deref(), Some("running"));
        assert_eq!(sessions[2].agent_label.as_deref(), Some("from api"));
        assert_eq!(sessions[2].swarm_status.as_deref(), Some("completed"));
        // A local session with the same raw ID is a different session.
        assert_eq!(sessions[3].parent_session_id, None);
    }

    #[test]
    fn swarm_state_reads_recent_local_snapshots_only_from_json_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("swarm.json"), SNAPSHOT).unwrap();
        std::fs::write(
            dir.path().join("swarm.bak"),
            r#"{"updated_at_unix_ms": 9999, "members": [{"session_id": "boar", "status": "failed"}]}"#,
        )
        .unwrap();
        let mut roster = Roster::new();
        read_dir(dir.path(), &mut roster);
        assert_eq!(roster["boar"].status, "running");
        assert_eq!(roster.len(), 3);
    }

    #[test]
    fn swarm_state_wsl_script_is_fixed_and_separates_records() {
        let script = wsl_script();
        assert!(script.contains("state/swarm"));
        assert!(script.contains("-mtime -30"));
        assert!(script.contains("-size -4096k"));
        assert!(script.contains(r#"printf "\036""#));
    }

    /// Runs the exact script through `sh -s` like `wsl.exe` does, against a
    /// temporary JCODE_HOME, and parses the stream it produces.
    #[cfg(unix)]
    #[test]
    fn swarm_state_wsl_script_streams_snapshots_through_sh_stdin() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("state/swarm");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), SNAPSHOT).unwrap();
        std::fs::write(dir.join("b.json"), r#"{"updated_at_unix_ms": 5, "members": [{"session_id": "solo", "status": "ready"}]}"#).unwrap();
        std::fs::write(dir.join("a.bak"), "not json").unwrap();
        let mut child = std::process::Command::new("sh")
            .arg("-s")
            .env("JCODE_HOME", home.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), wsl_script().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let mut roster = Roster::new();
        parse_stream(&output.stdout, Some("wsl"), &mut roster);
        assert_eq!(roster.len(), 4);
        assert_eq!(roster["ssh://wsl/solo"].status, "ready");
        assert_eq!(
            roster["ssh://wsl/boar"].parent_session_id.as_deref(),
            Some("ssh://wsl/lead")
        );
    }

    /// Real WSL snapshots, when run inside WSL on a developer machine.
    #[test]
    fn swarm_state_parses_the_real_snapshot_dir_when_present() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let dir = Path::new(&home).join(".jcode/state/swarm");
        let mut roster = Roster::new();
        read_dir(&dir, &mut roster);
        for member in roster.values() {
            assert!(!member.session_id.is_empty());
        }
    }
}
