//! Agent turns: the files each assistant turn edited, derived from the tool
//! calls already in a transcript (`edit`, `apply_patch`, `write`, `multiedit`,
//! batches of those). No filesystem or git access.
use crate::diff_model::{DiffFile, DiffPreview};
use crate::panel::Item;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentTurn {
    /// 1-based position among the transcript's user prompts.
    pub number: usize,
    /// First non-empty line of the prompt that started the turn.
    pub prompt: String,
    /// Files edited in the turn, in first-edit order, hunks of repeated
    /// edits to one file appended.
    pub preview: DiffPreview,
    /// Edit tool calls in the turn.
    pub edits: usize,
}

impl AgentTurn {
    pub(crate) fn counts(&self) -> (usize, usize) {
        self.preview.counts()
    }
}

fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        out.push('…');
    }
    out
}

fn merge(files: &mut Vec<DiffFile>, file: DiffFile) {
    match files.iter_mut().find(|f| f.path == file.path) {
        Some(existing) => {
            existing.hunks.extend(file.hunks);
            if existing.note.is_none() {
                existing.note = file.note;
            }
        }
        None => files.push(file),
    }
}

/// Turns that edited at least one file, newest first.
pub(crate) fn agent_turns(items: &[Item]) -> Vec<AgentTurn> {
    let mut turns: Vec<AgentTurn> = Vec::new();
    let mut current: Option<AgentTurn> = None;
    let mut number = 0;
    for item in items {
        match item {
            Item::User(text) => {
                if let Some(turn) = current.take() {
                    turns.push(turn);
                }
                number += 1;
                current = Some(AgentTurn {
                    number,
                    prompt: first_line(text),
                    preview: DiffPreview { files: Vec::new() },
                    edits: 0,
                });
            }
            Item::Tool {
                name,
                input,
                output,
                done,
                error,
                ..
            } => {
                if error.is_some() || !*done {
                    continue;
                }
                let preview = crate::diff_model::from_tool_result(name, input, output)
                    .or_else(|| crate::diff_model::from_tool(name, input));
                let Some(preview) = preview.filter(|p| !p.files.is_empty()) else {
                    continue;
                };
                let turn = current.get_or_insert_with(|| AgentTurn {
                    number: 0,
                    prompt: "Before the first prompt".into(),
                    preview: DiffPreview { files: Vec::new() },
                    edits: 0,
                });
                turn.edits += 1;
                for file in preview.files {
                    merge(&mut turn.preview.files, file);
                }
            }
            _ => {}
        }
    }
    if let Some(turn) = current.take() {
        turns.push(turn);
    }
    turns.retain(|t| !t.preview.files.is_empty());
    turns.reverse();
    turns
}

/// Fenced diff for the composer.
pub(crate) fn fenced(file: &DiffFile) -> String {
    format!("```diff\n{}```\n", file.plain_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, input: serde_json::Value) -> Item {
        Item::Tool {
            call_id: format!("call-{name}-{}", input.to_string().len()),
            name: name.into(),
            input: input.to_string(),
            output: String::new(),
            done: true,
            error: None,
        }
    }

    pub(crate) fn fixture() -> Vec<Item> {
        vec![
            Item::User("\n  Add a greeting\nmore".into()),
            Item::Assistant("ok".into()),
            tool(
                "write",
                serde_json::json!({"file_path":"src/hello.rs","content":"fn hi() {}\nfn bye() {}\n"}),
            ),
            tool(
                "edit",
                serde_json::json!({"file_path":"src/hello.rs","old_string":"fn bye() {}","new_string":"fn bye() { done() }"}),
            ),
            Item::User("Just a question".into()),
            Item::Assistant("answer".into()),
            tool("bash", serde_json::json!({"command":"ls"})),
            Item::User("Rename label".into()),
            tool(
                "apply_patch",
                serde_json::json!({"patch_text":"*** Begin Patch\n*** Update File: src/nav.rs\n@@\n-    \"Go\"\n+    \"Continue\"\n*** End Patch"}),
            ),
            Item::Tool {
                call_id: "f".into(),
                name: "edit".into(),
                input: serde_json::json!({"file_path":"src/x.rs","old_string":"a","new_string":"b"}).to_string(),
                output: String::new(),
                done: true,
                error: Some("no match".into()),
            },
        ]
    }

    #[test]
    fn groups_edits_per_turn_newest_first() {
        let turns = agent_turns(&fixture());
        assert_eq!(turns.len(), 2, "the question turn edited nothing");
        assert_eq!(turns[0].number, 3);
        assert_eq!(turns[0].prompt, "Rename label");
        assert_eq!(turns[0].preview.files.len(), 1);
        assert_eq!(turns[0].preview.files[0].path, "src/nav.rs");
        assert_eq!(turns[0].counts(), (1, 1));
        assert_eq!(turns[1].number, 1);
        assert_eq!(turns[1].prompt, "Add a greeting");
        assert_eq!(turns[1].edits, 2);
        assert_eq!(turns[1].preview.files.len(), 1, "edits to one file merge");
        assert!(turns[1].preview.files[0].hunks.len() >= 2);
        assert!(fenced(&turns[0].preview.files[0]).starts_with("```diff\n"));
    }
}

#[cfg(test)]
pub(crate) use tests::fixture;
