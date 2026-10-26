//! Session details for the session browser, read from the stored record.
//!
//! Records can be hundreds of megabytes, so only the head (fields stored
//! before `messages`, plus the first prompt) and the tail (fields stored after
//! it) are read. WSL records are read through one `wsl.exe --exec sh` call,
//! never through `\\wsl$`. Results are cached by (session ID, updated_at), so
//! reselecting a row is free until the session changes. Call off the UI thread.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::session_git::Location;

const WINDOW: usize = 64 * 1024;
const SEPARATOR: u8 = 0x1e;

/// What the browser's info header shows beyond the session list fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionMeta {
    pub title: Option<String>,
    pub short_name: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub last_active_at: Option<String>,
    pub working_dir: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub reasoning_effort: Option<String>,
    pub saved: bool,
    pub save_label: Option<String>,
    pub parent_id: Option<String>,
    /// User messages that start with text: prompts, not tool results (which
    /// are stored as user messages too and usually outnumber prompts 20:1).
    pub user_messages: usize,
    pub assistant_messages: usize,
    /// The first prompt a person typed, without injected system reminders.
    pub first_prompt: Option<String>,
    /// Record size, the basis for the token estimate.
    pub bytes: u64,
}

impl SessionMeta {
    /// About four bytes per token, like the sidebar's estimate.
    pub fn estimated_tokens(&self) -> u64 {
        self.bytes / 4
    }
}

type Key = (String, Option<i64>);
static CACHE: Mutex<Option<HashMap<Key, SessionMeta>>> = Mutex::new(None);

/// Details for a Desktop session ID (local or `ssh://wsl/...`), cached by
/// `updated_at_ms`. Blocks on disk and wsl.exe, so call off the UI thread.
pub fn load(session_id: &str, updated_at_ms: Option<i64>) -> Result<SessionMeta, String> {
    let key = (session_id.to_owned(), updated_at_ms);
    if let Some(hit) = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|cache| cache.get(&key).cloned())
    {
        return Ok(hit);
    }
    let location = Location::of_session(session_id)
        .ok_or_else(|| "Session details are unavailable for this host.".to_owned())?;
    let id = stored_id(session_id).ok_or_else(|| "Invalid session ID.".to_owned())?;
    let meta = match &location {
        Location::Local => read_local(&id)?,
        Location::Wsl(_) => read_wsl(&location, &id)?,
    };
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if cache.len() > 512 {
        cache.clear();
    }
    cache.insert(key, meta.clone());
    Ok(meta)
}

/// The runtime's own ID, restricted to the characters jcode generates so it
/// can never name a path outside the sessions directory.
fn stored_id(session_id: &str) -> Option<String> {
    let id = session_id.rsplit('/').next()?;
    (!id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    .then(|| id.to_owned())
}

fn read_local(id: &str) -> Result<SessionMeta, String> {
    use std::io::{Read, Seek, SeekFrom};
    let home = crate::harness::jcode_home().ok_or("Session storage unavailable.")?;
    let path = home.join("sessions").join(format!("{id}.json"));
    let mut file = std::fs::File::open(&path).map_err(|_| "Session record not found.".to_owned())?;
    let bytes = file.metadata().map(|m| m.len()).unwrap_or_default();
    if bytes <= (WINDOW * 2) as u64 {
        let mut all = Vec::new();
        file.read_to_end(&mut all).map_err(|e| e.to_string())?;
        let (user, assistant) = count_roles(&all);
        return Ok(parse(&all, &all, bytes, user, assistant));
    }
    let mut head = vec![0; WINDOW];
    file.read_exact(&mut head).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::End(-(WINDOW as i64))).map_err(|e| e.to_string())?;
    let mut tail = vec![0; WINDOW];
    file.read_exact(&mut tail).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let (user, assistant) = count_stream(&mut file, 4 << 20).map_err(|e| e.to_string())?;
    Ok(parse(&head, &tail, bytes, user, assistant))
}

/// Role counts over a reader in fixed chunks. One carry per needle, each kept
/// one byte shorter than its needle, so a match split across reads counts
/// once and none counts twice.
fn count_stream(reader: &mut impl std::io::Read, chunk_size: usize) -> std::io::Result<(usize, usize)> {
    let (mut user, mut assistant) = (0, 0);
    let mut chunk = vec![0; chunk_size];
    let mut carries = [Vec::new(), Vec::new()];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        for (carry, (needle, total)) in carries
            .iter_mut()
            .zip([(USER_NEEDLE, &mut user), (ASSISTANT_NEEDLE, &mut assistant)])
        {
            carry.extend_from_slice(&chunk[..read]);
            *total += count_needle(carry, needle);
            let keep = carry.len().min(needle.len() - 1);
            carry.drain(..carry.len() - keep);
        }
    }
    Ok((user, assistant))
}

/// Prints `size RS head RS tail RS role-initial counts` for `$HOME/.jcode/sessions/$1.json`.
const WSL_SCRIPT: &str = "f=\"$HOME/.jcode/sessions/$1.json\"; [ -f \"$f\" ] || exit 3; \
    wc -c < \"$f\"; printf '\\036'; head -c 65536 \"$f\"; printf '\\036'; tail -c 65536 \"$f\"; printf '\\036'; \
    grep -oE '\"role\":\"(user\",\"content\":\\[\\{\"type\":\"text|assistant)\"' \"$f\" | cut -c9 | sort | uniq -c";

fn read_wsl(location: &Location, id: &str) -> Result<SessionMeta, String> {
    let mut command = location.command("/", "sh");
    command.args(["-c", WSL_SCRIPT, "sh", id]);
    let output = location
        .output(&mut command)
        .map_err(|error| format!("Could not run wsl.exe: {error}"))?;
    if output.status.code() == Some(3) {
        return Err("Session record not found in WSL.".into());
    }
    parse_wsl_output(&output.stdout)
        .ok_or_else(|| "Could not read session details from WSL.".to_owned())
}

fn parse_wsl_output(stdout: &[u8]) -> Option<SessionMeta> {
    let mut parts = stdout.splitn(4, |byte| *byte == SEPARATOR);
    let bytes = std::str::from_utf8(parts.next()?).ok()?.trim().parse().ok()?;
    let head = parts.next()?;
    let tail = parts.next()?;
    let (mut user, mut assistant) = (0, 0);
    for line in String::from_utf8_lossy(parts.next().unwrap_or_default()).lines() {
        let mut fields = line.split_whitespace();
        let (Some(count), Some(role)) = (fields.next(), fields.next()) else {
            continue;
        };
        let count = count.parse().unwrap_or(0);
        match role {
            "u" => user = count,
            "a" => assistant = count,
            _ => {}
        }
    }
    // A small record arrives whole in both windows. Prefer complete parsing.
    Some(parse(head, tail, bytes, user, assistant))
}

const USER_NEEDLE: &[u8] = b"\"role\":\"user\",\"content\":[{\"type\":\"text\"";
const ASSISTANT_NEEDLE: &[u8] = b"\"role\":\"assistant\"";

fn count_needle(bytes: &[u8], needle: &[u8]) -> usize {
    bytes.windows(needle.len()).filter(|w| *w == needle).count()
}

fn count_roles(bytes: &[u8]) -> (usize, usize) {
    (count_needle(bytes, USER_NEEDLE), count_needle(bytes, ASSISTANT_NEEDLE))
}

fn parse(head: &[u8], tail: &[u8], bytes: u64, user: usize, assistant: usize) -> SessionMeta {
    let whole = (bytes as usize) <= head.len();
    let full: Option<serde_json::Value> = whole.then(|| serde_json::from_slice(head).ok()).flatten();
    let string = |name: &str, from_tail: bool| -> Option<String> {
        if let Some(full) = &full {
            return full.get(name)?.as_str().map(str::to_owned);
        }
        let window = if from_tail { tail } else { head };
        field(window, name, from_tail)?.as_str().map(str::to_owned)
    };
    let value = |name: &str| -> Option<serde_json::Value> {
        match &full {
            Some(full) => full.get(name).cloned(),
            None => field(tail, name, true),
        }
    };
    SessionMeta {
        title: string("custom_title", true)
            .or_else(|| string("title", false))
            .filter(|t| !t.trim().is_empty()),
        short_name: string("short_name", true),
        created_at: string("created_at", false),
        updated_at: string("updated_at", false),
        last_active_at: string("last_active_at", true),
        working_dir: string("working_dir", true),
        model: string("model", true).filter(|m| m != "unknown"),
        provider: string("provider_key", true),
        reasoning_effort: string("reasoning_effort", true),
        saved: value("saved").and_then(|v| v.as_bool()).unwrap_or(false),
        save_label: string("save_label", true),
        parent_id: string("parent_id", false),
        user_messages: user,
        assistant_messages: assistant,
        first_prompt: first_prompt(head),
        bytes,
    }
}

/// A top-level field by its first (head) or last (tail) occurrence.
fn field(bytes: &[u8], name: &str, last: bool) -> Option<serde_json::Value> {
    let needle = format!("\"{name}\":");
    let mut starts = bytes
        .windows(needle.len())
        .enumerate()
        .filter_map(|(index, window)| (window == needle.as_bytes()).then_some(index));
    let index = if last { starts.next_back()? } else { starts.next()? };
    let value = bytes[index + needle.len()..].trim_ascii_start();
    serde_json::Deserializer::from_slice(value)
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()
}

/// Walk the head's `messages` array one complete message at a time. The
/// window may cut a message short, which simply ends the walk.
fn first_prompt(head: &[u8]) -> Option<String> {
    let needle = b"\"messages\":[";
    let start = head.windows(needle.len()).position(|w| w == needle)? + needle.len();
    let mut rest = &head[start..];
    loop {
        rest = rest.trim_ascii_start();
        rest = rest.strip_prefix(b",").unwrap_or(rest).trim_ascii_start();
        if rest.first() == Some(&b']') || rest.is_empty() {
            return None;
        }
        let mut stream =
            serde_json::Deserializer::from_slice(rest).into_iter::<serde_json::Value>();
        let message = stream.next()?.ok()?;
        rest = &rest[stream.byte_offset()..];
        if message.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) != Some("text") {
                continue;
            }
            let Some(text) = block.get("text").and_then(|t| t.as_str()) else {
                continue;
            };
            let text = strip_reminders(text);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
}

fn strip_reminders(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find("<system-reminder>") {
        out.push_str(&rest[..open]);
        match rest[open..].find("</system-reminder>") {
            Some(close) => rest = &rest[open + close + "</system-reminder>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: &str = r#"{"id":"session_fox_1792983424920_ab","parent_id":"session_owl_1","title":"Fix PvP ladder","created_at":"2026-10-23T14:02:00Z","updated_at":"2026-10-26T09:00:00Z","messages":[{"id":"m1","role":"user","content":[{"type":"text","text":"<system-reminder>\nctx\n</system-reminder>"},{"type":"text","text":"Fix the ladder please"}]},{"id":"m2","role":"assistant","content":[{"type":"text","text":"ok"}]}],"provider_key":"claude","model":"claude-opus-5-5","reasoning_effort":"high","working_dir":"/home/v/go-chathub","short_name":"fox","status":"Closed","last_active_at":"2026-10-26T08:58:00Z","saved":true,"save_label":"ladder"}"#;

    #[test]
    fn parses_small_record_whole() {
        let bytes = RECORD.as_bytes();
        let (u, a) = count_roles(bytes);
        let meta = parse(bytes, bytes, bytes.len() as u64, u, a);
        assert_eq!(meta.title.as_deref(), Some("Fix PvP ladder"));
        assert_eq!(meta.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(meta.provider.as_deref(), Some("claude"));
        assert_eq!(meta.working_dir.as_deref(), Some("/home/v/go-chathub"));
        assert_eq!(meta.short_name.as_deref(), Some("fox"));
        assert_eq!(meta.created_at.as_deref(), Some("2026-10-23T14:02:00Z"));
        assert!(meta.saved);
        assert_eq!(meta.save_label.as_deref(), Some("ladder"));
        assert_eq!((meta.user_messages, meta.assistant_messages), (1, 1));
        assert_eq!(meta.first_prompt.as_deref(), Some("Fix the ladder please"));
    }

    #[test]
    fn parses_wsl_script_output_with_truncated_windows() {
        let padding = "x".repeat(WINDOW);
        let record = RECORD.replace("\"ok\"", &format!("\"{padding}\""));
        let head = &record.as_bytes()[..WINDOW];
        let tail = &record.as_bytes()[record.len() - WINDOW..];
        let mut stdout = format!("{}\n", record.len()).into_bytes();
        stdout.push(SEPARATOR);
        stdout.extend_from_slice(head);
        stdout.push(SEPARATOR);
        stdout.extend_from_slice(tail);
        stdout.push(SEPARATOR);
        stdout.extend_from_slice(b"     42 u\n     40 a\n");
        let meta = parse_wsl_output(&stdout).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Fix PvP ladder"));
        assert_eq!(meta.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(meta.last_active_at.as_deref(), Some("2026-10-26T08:58:00Z"));
        assert_eq!((meta.user_messages, meta.assistant_messages), (42, 40));
        assert_eq!(meta.first_prompt.as_deref(), Some("Fix the ladder please"));
        assert_eq!(meta.estimated_tokens(), record.len() as u64 / 4);
    }

    #[test]
    fn tool_results_are_not_counted_as_prompts() {
        let record = RECORD.replace(
            r#"{"id":"m2","#,
            r#"{"id":"t1","role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]},{"id":"m2","#,
        );
        assert_eq!(count_roles(record.as_bytes()), (1, 1));
    }

    #[test]
    fn streamed_counts_match_whole_counts_at_every_chunk_size() {
        let record = RECORD.repeat(7);
        let whole = count_roles(record.as_bytes());
        assert_eq!(whole, (7, 7));
        for chunk in 1..64 {
            let mut reader = std::io::Cursor::new(record.as_bytes());
            assert_eq!(count_stream(&mut reader, chunk).unwrap(), whole, "chunk {chunk}");
        }
    }

    #[test]
    fn stored_id_rejects_paths() {
        assert_eq!(stored_id("ssh://wsl/session_fox_1").as_deref(), Some("session_fox_1"));
        assert_eq!(stored_id("session_fox_1").as_deref(), Some("session_fox_1"));
        assert!(stored_id("..").is_none());
        assert!(stored_id("a b").is_none());
        assert!(stored_id("").is_none());
    }

    #[test]
    fn wsl_script_runs_against_a_real_record() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".jcode/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session_fox_1.json"), RECORD).unwrap();
        let Ok(output) = std::process::Command::new("sh")
            .env("HOME", home.path())
            .args(["-c", WSL_SCRIPT, "sh", "session_fox_1"])
            .output()
        else {
            return;
        };
        if !output.status.success() {
            return; // no POSIX shell (Windows host)
        }
        let meta = parse_wsl_output(&output.stdout).unwrap();
        assert_eq!((meta.user_messages, meta.assistant_messages), (1, 1));
        assert_eq!(meta.first_prompt.as_deref(), Some("Fix the ladder please"));
    }
}
