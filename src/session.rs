use crate::provider::{Content, Message};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub created: u64,
    pub summary: String,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub model: String,
    pub messages: Vec<Message>,
    pub transcript: Vec<(String, String)>,
    /// Files pinned with `/add`; restored into the agent when the session
    /// resumes (older exports simply default to an empty list).
    #[serde(default)]
    pub pinned: Vec<String>,
    /// The session this one was forked from — the edge `/tree` draws.
    #[serde(default)]
    pub parent: Option<String>,
}

impl Session {
    pub fn fresh() -> Self {
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            name: format!(
                "{created}-{}-{}",
                std::process::id(),
                NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
            ),
            created,
            summary: "New session".into(),
            provider_name: String::new(),
            model: String::new(),
            messages: Vec::new(),
            transcript: Vec::new(),
            pinned: Vec::new(),
            parent: None,
        }
    }
    pub fn dir() -> Result<PathBuf> {
        Ok(
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join(".wrosecode/sessions"),
        )
    }
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.json", self.name))
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = self.path(dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }
    pub fn list(dir: &Path) -> Result<Vec<Self>> {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(session) = Self::load(&path) {
                    sessions.push(session);
                }
            }
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.created));
        Ok(sessions)
    }
    /// The session forest for `/tree`: roots in creation order, forks
    /// indented under their parent, the active session marked. A `parent`
    /// that no longer exists is drawn as a root, so deleting a session never
    /// hides its orphans.
    pub fn tree(sessions: &[Session], current: &str) -> Vec<String> {
        let known = |parent: &str| sessions.iter().any(|other| other.name == parent);
        let mut children: BTreeMap<&str, Vec<&Session>> = BTreeMap::new();
        let mut roots: Vec<&Session> = Vec::new();
        for session in sessions {
            match session.parent.as_deref() {
                Some(parent) if parent != session.name && known(parent) => {
                    children.entry(parent).or_default().push(session);
                }
                _ => roots.push(session),
            }
        }
        roots.sort_by_key(|session| session.created);
        for branch in children.values_mut() {
            branch.sort_by_key(|session| session.created);
        }

        fn walk(
            session: &Session,
            line_prefix: &str,
            subtree_prefix: &str,
            children: &BTreeMap<&str, Vec<&Session>>,
            current: &str,
            visited: &mut Vec<String>,
            lines: &mut Vec<String>,
        ) {
            visited.push(session.name.clone());
            let marker = if session.name == current {
                "  (current)"
            } else {
                ""
            };
            lines.push(format!(
                "{line_prefix}{}  {}{marker}",
                session.name, session.summary
            ));
            let branch = children
                .get(session.name.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            for (index, child) in branch.iter().enumerate() {
                if visited.contains(&child.name) {
                    continue;
                }
                let last = index + 1 == branch.len();
                walk(
                    child,
                    &format!("{subtree_prefix}{}", if last { "└── " } else { "├── " }),
                    &format!("{subtree_prefix}{}", if last { "    " } else { "│   " }),
                    children,
                    current,
                    visited,
                    lines,
                );
            }
        }

        let mut lines = Vec::new();
        let mut visited = Vec::new();
        for root in roots {
            walk(root, "", "", &children, current, &mut visited, &mut lines);
        }
        // A parent cycle would leave sessions unreachable from any root; keep
        // them visible rather than silently dropping them.
        let mut strays: Vec<&Session> = sessions
            .iter()
            .filter(|session| !visited.contains(&session.name))
            .collect();
        strays.sort_by_key(|session| session.created);
        for stray in strays {
            if visited.contains(&stray.name) {
                continue;
            }
            walk(stray, "", "", &children, current, &mut visited, &mut lines);
        }
        lines
    }

    pub fn rename(&mut self, dir: &Path, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            anyhow::bail!("session name must use letters, numbers, - or _");
        }
        let old = self.path(dir);
        let new = dir.join(format!("{name}.json"));
        if new.exists() && new != old {
            anyhow::bail!("session {name} already exists");
        }
        self.name = name.into();
        self.save(dir)?;
        if old != new && old.exists() {
            std::fs::remove_file(old)?;
        }
        Ok(())
    }

    /// A copy of this session to branch from. `keep = None` keeps the whole
    /// conversation (a fork at the current end); `Some(n)` keeps only the
    /// first `n` messages so the branch starts from an earlier point
    /// (pi-mono: fork at any message — `/fork <n>` and `/history` use the
    /// same 1-based numbering). A partial fork rebuilds the transcript from
    /// the messages it keeps, because the live one mirrors rendered UI
    /// entries (tool cells, system lines) that do not line up with raw
    /// messages; restoring the session then shows exactly the context that
    /// survived.
    pub fn branch(&self, keep: Option<usize>) -> Result<Session> {
        let total = self.messages.len();
        let mut keep = keep.unwrap_or(total);
        if keep > total || (keep == 0 && total > 0) {
            anyhow::bail!("message {keep} is out of range (1..={total})");
        }
        let mut forked = Session::fresh();
        forked.parent = Some(self.name.clone());
        forked.summary = format!("Fork of {}", self.name);
        forked.provider_name = self.provider_name.clone();
        forked.model = self.model.clone();
        forked.pinned = self.pinned.clone();
        forked.messages = self.messages[..keep].to_vec();
        // A turn does not end with an assistant's tool calls: providers
        // reject orphaned `tool_use` blocks, so a cut that lands on pending
        // calls is rounded forward to include the results that answer them
        // (only reachable for a partial fork, where `keep < total`).
        while keep < total {
            let pending = forked.messages.last().is_some_and(|last| {
                last.role == "assistant"
                    && last
                        .content
                        .iter()
                        .any(|content| matches!(content, Content::Call(_)))
            });
            if !pending {
                break;
            }
            forked.messages.push(self.messages[keep].clone());
            keep += 1;
        }
        forked.transcript = if forked.messages.len() == total {
            self.transcript.clone()
        } else {
            transcript_for(&forked.messages)
        };
        Ok(forked)
    }
}

/// The `/history` listing: one numbered line per message. Numbering is
/// 1-based over the raw message list — the same numbering `/fork <n>`
/// keeps a prefix of.
pub fn history(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            format!(
                "{:>4}  {:<6} {}",
                index + 1,
                role_label(message),
                snippet(message)
            )
        })
        .collect()
}

/// Messages holding tool results are tool traffic even though providers see
/// them under the `user` role.
fn role_label(message: &Message) -> &str {
    if message
        .content
        .iter()
        .any(|content| matches!(content, Content::Result { .. }))
    {
        "tool"
    } else {
        match message.role.as_str() {
            "assistant" => "agent",
            "user" => "user",
            other => other,
        }
    }
}

/// First displayable line of a message, capped so a wall-of-text prompt
/// cannot flood the `/history` listing.
fn snippet(message: &Message) -> String {
    let raw = match message.content.first() {
        Some(Content::Text(text)) => text.as_str(),
        Some(Content::Result { output, .. }) => output.as_str(),
        Some(Content::Call(call)) => call.name.as_str(),
        None => "",
    };
    let line = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(empty)");
    let capped: String = line.chars().take(72).collect();
    if line.chars().count() > 72 {
        format!("{capped}…")
    } else {
        capped
    }
}

/// Rebuild a transcript for a truncated message list: roles map to the
/// speaker names the session file stores (`user`/`agent`/`tool`), and
/// results or pending tool calls appear as plain tool lines. See
/// [`Session::branch`].
fn transcript_for(messages: &[Message]) -> Vec<(String, String)> {
    let mut lines = Vec::new();
    for message in messages {
        match message.role.as_str() {
            "assistant" => {
                for content in &message.content {
                    match content {
                        Content::Text(text) if !text.trim().is_empty() => {
                            lines.push(("agent".into(), text.clone()));
                        }
                        Content::Call(call) => {
                            lines.push(("tool".into(), format!("· {}", call.name)));
                        }
                        _ => {}
                    }
                }
            }
            _ if message
                .content
                .iter()
                .any(|content| matches!(content, Content::Result { .. })) =>
            {
                for content in &message.content {
                    match content {
                        Content::Result {
                            output, is_error, ..
                        } => {
                            let mark = if *is_error { "✗" } else { "·" };
                            let first = output
                                .lines()
                                .map(str::trim)
                                .find(|line| !line.is_empty())
                                .unwrap_or("");
                            let first: String = first.chars().take(160).collect();
                            lines.push(("tool".into(), format!("{mark} {first}")));
                        }
                        Content::Text(text) if !text.trim().is_empty() => {
                            lines.push(("tool".into(), text.clone()));
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                // The user's prompt, plus any context injected into it.
                for content in &message.content {
                    if let Content::Text(text) = content {
                        if !text.trim().is_empty() {
                            lines.push(("user".into(), text.clone()));
                        }
                    }
                }
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    fn named(name: &str, created: u64, parent: Option<&str>) -> Session {
        let mut session = Session::fresh();
        session.name = name.into();
        session.created = created;
        session.summary = format!("summary of {name}");
        session.parent = parent.map(str::to_string);
        session
    }

    fn text_message(role: &str, text: &str) -> Message {
        Message {
            role: role.into(),
            content: vec![Content::Text(text.into())],
        }
    }

    fn result_message(output: &str) -> Message {
        Message {
            role: "user".into(),
            content: vec![Content::Result {
                id: "call-1".into(),
                output: output.into(),
                is_error: false,
            }],
        }
    }

    fn called_message(text: &str) -> Message {
        Message {
            role: "assistant".into(),
            content: vec![
                Content::Text(text.into()),
                Content::Call(ToolCall {
                    id: "call-1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "ls"}),
                }),
            ],
        }
    }

    #[test]
    fn the_tree_indents_forks_under_their_parent_and_marks_the_current_one() {
        let sessions = vec![
            named("root", 1, None),
            named("child-b", 3, Some("root")),
            named("child-a", 2, Some("root")),
            named("lonely", 4, None),
            named("orphan", 5, Some("deleted-session")),
        ];
        assert_eq!(
            Session::tree(&sessions, "child-a"),
            vec![
                "root  summary of root".to_string(),
                "├── child-a  summary of child-a  (current)".to_string(),
                "└── child-b  summary of child-b".to_string(),
                "lonely  summary of lonely".to_string(),
                "orphan  summary of orphan".to_string(),
            ]
        );
    }

    #[test]
    fn a_parent_cycle_keeps_every_session_visible() {
        let sessions = vec![named("a", 1, Some("b")), named("b", 2, Some("a"))];
        let lines = Session::tree(&sessions, "a");
        assert_eq!(
            lines,
            vec![
                "a  summary of a  (current)".to_string(),
                "└── b  summary of b".to_string(),
            ]
        );
    }

    #[test]
    fn fork_at_keeps_a_prefix_and_rebuilds_the_transcript() {
        let mut parent = Session::fresh();
        parent.name = "parent".into();
        parent.summary = "seeded".into();
        parent.messages = vec![
            text_message("user", "first question"),
            text_message("assistant", "first answer"),
            text_message("user", "second question"),
            text_message("assistant", "second answer"),
        ];
        parent.transcript = vec![
            ("user".into(), "first question".into()),
            ("agent".into(), "first answer".into()),
            ("system".into(), "live-UI-only line".into()),
        ];
        let forked = parent.branch(Some(2)).expect("fork at message 2");
        assert_eq!(forked.messages.len(), 2);
        assert_eq!(forked.parent.as_deref(), Some("parent"));
        assert_eq!(forked.summary, "Fork of parent");
        assert_eq!(forked.provider_name, parent.provider_name);
        // Rebuilt from the kept messages, not copied: nothing past the cut
        // survives, including transcript-only lines the UI rendered.
        assert_eq!(
            forked.transcript,
            vec![
                ("user".into(), "first question".into()),
                ("agent".into(), "first answer".into()),
            ]
        );
    }

    #[test]
    fn forking_at_the_current_end_keeps_the_original_transcript() {
        let mut parent = Session::fresh();
        parent.messages = vec![text_message("user", "one")];
        parent.transcript = vec![("system".into(), "live UI line".into())];
        for keep in [None, Some(1)] {
            let forked = parent.branch(keep).expect("end fork");
            assert_eq!(forked.messages.len(), 1);
            assert_eq!(forked.transcript, parent.transcript);
        }
        let empty = Session::fresh();
        assert!(empty.branch(None).is_ok(), "forking an empty session");
    }

    #[test]
    fn fork_at_rejects_out_of_range_message_numbers() {
        let mut parent = Session::fresh();
        parent.messages = vec![
            text_message("user", "one"),
            text_message("assistant", "two"),
        ];
        for keep in [0, 3] {
            let error = parent.branch(Some(keep)).expect_err("out of range");
            assert!(
                error.to_string().contains("1..=2"),
                "message {keep}: {error}"
            );
        }
    }

    #[test]
    fn a_cut_on_pending_tool_calls_rounds_forward_to_the_results() {
        let mut parent = Session::fresh();
        parent.messages = vec![
            text_message("user", "run something"),
            called_message("on it"),
            result_message("exit=0"),
            text_message("assistant", "done"),
        ];
        // Cutting at the assistant's call would strand the tool_use blocks;
        // the results that answer them come along.
        let forked = parent.branch(Some(2)).expect("fork at the call");
        assert_eq!(forked.messages.len(), 3);
        assert_eq!(
            forked.messages[2].content.len(),
            1,
            "the last kept message should be the results"
        );
        assert!(
            forked
                .transcript
                .iter()
                .any(|(speaker, text)| speaker == "tool" && text.contains("exit=0")),
            "the rebuilt transcript shows the tool result: {:?}",
            forked.transcript
        );
    }

    #[test]
    fn history_numbers_messages_the_way_fork_does() {
        let messages = vec![
            text_message("user", "find the flag"),
            called_message("searching"),
            result_message("exit=0\nflag{...}"),
        ];
        let lines = history(&messages);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "   1  user   find the flag");
        assert_eq!(lines[1], "   2  agent  searching");
        assert_eq!(lines[2], "   3  tool   exit=0");
        // Long prompts collapse to one capped line.
        let mut with_long = messages;
        with_long.push(text_message("user", &"y".repeat(200)));
        let lines = history(&with_long);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[3].chars().filter(|ch| *ch == 'y').count(), 72);
        assert!(lines[3].ends_with('…'));
    }
}
