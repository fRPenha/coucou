//! Token use per Claude Code session, read from the session's own transcript.
//!
//! The relay drops `transcript_path`, so the file is found again from the
//! `session_id`: `~/.claude/projects/<project>/<session_id>.jsonl`. Each hook
//! event reads only the lines written since the last one (a byte offset per
//! session) and adds their `usage`, once per message id: Claude Code writes one
//! line per content block, all carrying the same usage. Nothing is sent
//! anywhere; the totals go to the island as a `usage` event.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use crate::island::WINDOW_LABEL;
use crate::platform;

/// A context smaller than this share of the previous call is a compaction
/// (same rule as the Meu Claude panel's gerar-dados.py).
const COMPACTION_RATIO: f64 = 0.6;

/// US$ per million tokens: input, output, cache write 5 min, cache write 1 h,
/// cache read. Anthropic API list prices as of 2026-10-06 (claude-api skill);
/// cache writes are 1.25x / 2x input, reads ~0.1x except where listed lower.
// ponytail: hand-kept table; an unknown model counts tokens but no US$ until added here.
fn price(model: &str) -> Option<[f64; 5]> {
    Some(match model {
        "claude-fable-5-1" | "claude-mythos-5-1" => [10.0, 50.0, 12.5, 20.0, 0.25],
        "claude-opus-5-5" => [4.0, 20.0, 5.0, 8.0, 0.20],
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" => [5.0, 25.0, 6.25, 10.0, 0.50],
        "claude-sonnet-5-5" | "claude-sonnet-5" => [2.0, 10.0, 2.5, 4.0, 0.20],
        "claude-sonnet-4-6" => [3.0, 15.0, 3.75, 6.0, 0.30],
        "claude-haiku-4-5" => [1.0, 5.0, 1.25, 2.0, 0.10],
        _ => return None,
    })
}

#[derive(Default, Serialize, Clone)]
pub struct Tool {
    pub calls: u64,
    /// Size of what the tool returned, in tokens (characters / 4, as the panel).
    pub tokens: u64,
}

#[derive(Default, Serialize, Clone)]
pub struct Totals {
    pub calls: u64,
    /// Everything that went in: fresh input + cache written + cache read.
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The part of cache_write kept for 1 hour (billed 2x instead of 1.25x).
    pub cache_write_1h: u64,
    pub output: u64,
    /// Context size of the latest call and the largest one.
    pub last: u64,
    pub peak: u64,
    pub compactions: u64,
    pub model: String,
    /// What these tokens would cost on the API; plan users pay nothing extra.
    pub usd: f64,
    /// Calls with a model missing from `price`, left out of `usd`.
    pub unpriced: u64,
    pub tools: HashMap<String, Tool>,
    /// The session's custom title, else its first prompt.
    pub title: String,
    /// Timestamps of the first and the latest transcript line (ISO 8601).
    pub started: String,
    pub updated: String,
}

#[derive(Default)]
struct Session {
    offset: u64,
    seen: HashSet<String>,
    tool_names: HashMap<String, String>,
    custom_title: bool,
    totals: Totals,
}

/// claude.ai connectors show up as `mcp__<uuid>__…`; the first 8 hex digits
/// name the ones in use here (same table as the Meu Claude panel).
// ponytail: personal table, a new connector shows as its id prefix until added.
const CONNECTORS: &[(&str, &str)] = &[
    ("3601a52f", "Google Drive"), ("62798bb0", "Jira"), ("f1ce1523", "Jira"), ("62f13c8d", "Jira"),
    ("ecca1f33", "Gmail"), ("44188114", "Google Calendar"), ("4022ebd6", "Context7"),
    ("1a59c906", "Claude Docs"), ("0414693c", "Mermaid"), ("58de92b6", "Visualize"), ("6f616b42", "Visualize"),
];

/// `mcp__<server>__<tool>` is shown per server, everything else by its name.
fn tool_label(name: &str) -> String {
    let Some(server) = name.strip_prefix("mcp__").and_then(|r| r.split("__").next()) else { return name.to_string() };
    let short: String = server.chars().take(8).collect();
    match CONNECTORS.iter().find(|(id, _)| *id == short) {
        Some((_, label)) => format!("MCP {label}"),
        None => format!("MCP {}", server.chars().take(24).collect::<String>()),
    }
}

impl Session {
    /// Adds one transcript line. Usage is counted once per message id; tool
    /// calls are matched to their results by id.
    fn feed(&mut self, line: &str) {
        let Ok(o) = serde_json::from_str::<Value>(line) else { return };
        if let Some(ts) = o["timestamp"].as_str() {
            if self.totals.started.is_empty() {
                self.totals.started = ts.to_string();
            }
            self.totals.updated = ts.to_string();
        }
        let m = &o["message"];
        match o.get("type").and_then(Value::as_str) {
            Some("assistant") => {}
            Some("user") => return self.user(m),
            Some("custom-title") => {
                if let Some(t) = o["customTitle"].as_str() {
                    self.custom_title = true;
                    self.totals.title = t.to_string();
                }
                return;
            }
            _ => return,
        }
        for c in m["content"].as_array().into_iter().flatten() {
            if let (Some("tool_use"), Some(id), Some(name)) =
                (c["type"].as_str(), c["id"].as_str(), c["name"].as_str())
            {
                self.tool_names.insert(id.to_string(), tool_label(name));
            }
        }
        let (Some(u), Some(id)) = (m.get("usage"), m.get("id").and_then(Value::as_str)) else { return };
        if !self.seen.insert(id.to_string()) {
            return;
        }
        let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        let (fresh, write, read, out) =
            (n(u, "input_tokens"), n(u, "cache_creation_input_tokens"), n(u, "cache_read_input_tokens"), n(u, "output_tokens"));
        let write_1h = n(&u["cache_creation"], "ephemeral_1h_input_tokens").min(write);
        let context = fresh + write + read;
        let model = m.get("model").and_then(Value::as_str).unwrap_or_default();
        let t = &mut self.totals;
        if t.calls > 0 && (context as f64) < COMPACTION_RATIO * t.last as f64 {
            t.compactions += 1;
        }
        t.calls += 1;
        t.input += context;
        t.cache_read += read;
        t.cache_write += write;
        t.cache_write_1h += write_1h;
        t.output += out;
        t.last = context;
        t.peak = t.peak.max(context);
        match price(model) {
            Some([i, o, w5, w1, r]) => {
                let tokens = [fresh, out, write - write_1h, write_1h, read];
                t.usd += [i, o, w5, w1, r].iter().zip(tokens).map(|(p, n)| p * n as f64).sum::<f64>() / 1e6;
            }
            None if model != "<synthetic>" => t.unpriced += 1,
            None => {}
        }
        if !model.is_empty() && model != "<synthetic>" {
            t.model = model.to_string();
        }
    }

    /// A prompt (the first one titles the session) or the results of tool calls.
    fn user(&mut self, m: &Value) {
        if self.totals.title.is_empty() && !self.custom_title {
            let text = match &m["content"] {
                Value::String(s) => Some(s.as_str()),
                Value::Array(parts) => parts.iter().find(|p| p["type"] == "text").and_then(|p| p["text"].as_str()),
                _ => None,
            };
            if let Some(t) = text.filter(|t| !t.starts_with('<')) {
                self.totals.title = t.lines().next().unwrap_or_default().chars().take(70).collect();
            }
        }
        for c in m["content"].as_array().into_iter().flatten() {
            if c["type"].as_str() != Some("tool_result") {
                continue;
            }
            let chars = match &c["content"] {
                Value::String(s) => s.chars().count(),
                Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).map(|t| t.chars().count()).sum(),
                _ => 0,
            };
            let name = c["tool_use_id"].as_str().and_then(|id| self.tool_names.get(id)).cloned();
            let tool = self.totals.tools.entry(name.unwrap_or_else(|| "?".into())).or_default();
            tool.calls += 1;
            tool.tokens += (chars as f64 / 4.0).round() as u64;
        }
    }

    /// Reads what was appended since the last call; a half-written last line
    /// waits for the next event.
    fn read_new(&mut self, path: &PathBuf) {
        let Ok(mut f) = File::open(path) else { return };
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return;
        }
        let Some(end) = buf.rfind('\n') else { return };
        buf[..end].lines().for_each(|l| self.feed(l));
        self.offset += end as u64 + 1;
    }
}

// ponytail: scans the projects folder for every new session id; fine for a
// few dozen projects, keep the path from SessionStart if it ever shows up.
fn transcript(session_id: &str) -> Option<PathBuf> {
    let name = format!("{session_id}.jsonl");
    std::fs::read_dir(platform::home_dir().join(".claude").join("projects"))
        .ok()?
        .flatten()
        .map(|d| d.path().join(&name))
        .find(|p| p.is_file())
}

/// One session: its transcript and the transcripts of the subagents it ran
/// (`<session>/subagents/agent-*.jsonl`), each read on its own offset.
struct Tracked {
    path: PathBuf,
    cwd: String,
    main: Session,
    subagents: HashMap<PathBuf, Session>,
}

impl Tracked {
    fn read(&mut self) {
        self.main.read_new(&self.path);
        let dir = self.path.with_extension("").join("subagents");
        for f in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().is_some_and(|e| e == "jsonl") {
                self.subagents.entry(p.clone()).or_default().read_new(&p);
            }
        }
    }

    fn entry(&self) -> Entry {
        let mut sub = Totals::default();
        for s in self.subagents.values().map(|s| &s.totals) {
            sub.calls += s.calls;
            sub.input += s.input;
            sub.cache_read += s.cache_read;
            sub.output += s.output;
            sub.usd += s.usd;
            sub.unpriced += s.unpriced;
        }
        Entry { cwd: self.cwd.clone(), totals: self.main.totals.clone(), subagent_count: self.subagents.len(), subagents: sub }
    }
}

#[derive(Default)]
pub struct Store(Mutex<HashMap<String, Tracked>>);

#[derive(Serialize, Clone)]
pub struct Entry {
    pub cwd: String,
    pub totals: Totals,
    pub subagent_count: usize,
    /// The subagents' calls, tokens and US$ summed (title, tools and context
    /// fields stay empty).
    pub subagents: Totals,
}

#[derive(Serialize, Clone)]
struct Update<'a> {
    session_id: &'a str,
    #[serde(flatten)]
    entry: Entry,
}

/// Called by the relay server for every hook event.
pub fn observe(app: &AppHandle, payload: &Value) {
    let Some(sid) = payload.get("session_id").and_then(Value::as_str).filter(|s| !s.is_empty()) else { return };
    if payload.get("hook_event_name").and_then(Value::as_str) == Some("StatusLine") {
        return;
    }
    let Some(store) = app.try_state::<Store>() else { return };
    let mut map = store.0.lock().unwrap_or_else(PoisonError::into_inner);
    if !map.contains_key(sid) {
        let Some(path) = transcript(sid) else { return };
        let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or_default().to_string();
        map.insert(sid.to_string(), Tracked { path, cwd, main: Session::default(), subagents: HashMap::new() });
    }
    let tracked = map.get_mut(sid).unwrap();
    let before = (tracked.main.offset, tracked.subagents.values().map(|s| s.offset).sum::<u64>());
    tracked.read();
    let after = (tracked.main.offset, tracked.subagents.values().map(|s| s.offset).sum::<u64>());
    if after != before {
        let _ = app.emit_to(WINDOW_LABEL, "usage", Update { session_id: sid, entry: tracked.entry() });
    }
}

/// Sessions whose transcript changed since `since` (ms since the epoch, the
/// island's local midnight), picked up even without hooks, plus every session
/// a hook already brought in. Async: the first read of a big transcript must
/// not hold the main thread.
#[tauri::command]
pub async fn usage_sessions(app: AppHandle, since: u64) -> HashMap<String, Entry> {
    let Some(store) = app.try_state::<Store>() else { return HashMap::new() };
    let since = std::time::UNIX_EPOCH + std::time::Duration::from_millis(since);
    let mut map = store.0.lock().unwrap_or_else(PoisonError::into_inner);
    let projects = std::fs::read_dir(platform::home_dir().join(".claude").join("projects"));
    for dir in projects.into_iter().flatten().flatten() {
        for f in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let p = f.path();
            let fresh = f.metadata().and_then(|m| m.modified()).is_ok_and(|t| t >= since);
            let Some(sid) = p.file_stem().and_then(|s| s.to_str()).filter(|_| fresh && p.extension().is_some_and(|e| e == "jsonl")) else { continue };
            map.entry(sid.to_string())
                .or_insert_with(|| Tracked { path: p.clone(), cwd: String::new(), main: Session::default(), subagents: HashMap::new() });
        }
    }
    map.values_mut().for_each(Tracked::read);
    map.iter().filter(|(_, t)| t.main.totals.calls > 0).map(|(k, t)| (k.clone(), t.entry())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, fresh: u64, write: u64, read: u64, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"id":"{id}","model":"m","usage":{{"input_tokens":{fresh},"cache_creation_input_tokens":{write},"cache_read_input_tokens":{read},"output_tokens":{out}}}}}}}"#
        )
    }

    #[test]
    fn sums_once_per_message_and_counts_compactions() {
        let mut s = Session::default();
        s.feed(&line("a", 10, 90, 0, 5));
        s.feed(&line("a", 10, 90, 0, 5)); // second block of the same message
        s.feed(&line("b", 5, 10, 1000, 7));
        s.feed(&line("c", 5, 100, 0, 1)); // 105 < 0.6 * 1015: compacted
        s.feed(r#"{"type":"user","message":{"content":"hi"}}"#);
        s.feed("not json");
        let t = &s.totals;
        assert_eq!((t.calls, t.input, t.cache_read, t.cache_write, t.output), (3, 1220, 1000, 200, 13));
        assert_eq!((t.last, t.peak, t.compactions), (105, 1015, 1));
    }

    #[test]
    fn prices_known_models_and_counts_tools() {
        let mut s = Session::default();
        s.feed(r#"{"type":"assistant","message":{"id":"a","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"t1","name":"mcp__hydra__query"}],"usage":{"input_tokens":1000000,"output_tokens":1000000,"cache_creation_input_tokens":2000000,"cache_read_input_tokens":1000000,"cache_creation":{"ephemeral_1h_input_tokens":1000000}}}}"#);
        s.feed(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"12345678"}]}}"#);
        s.feed(&line("b", 10, 0, 0, 0)); // model "m": no price
        let t = &s.totals;
        // 4 in + 20 out + 5 (5 min write) + 8 (1 h write) + 0.20 read
        assert!((t.usd - 37.2).abs() < 1e-9, "{}", t.usd);
        assert_eq!((t.unpriced, t.cache_write_1h), (1, 1_000_000));
        let mcp = &t.tools["MCP hydra"];
        assert_eq!((mcp.calls, mcp.tokens), (1, 2));
    }

    #[test]
    fn reads_only_complete_new_lines() {
        let path = std::env::temp_dir().join(format!("coucou-usage-{}.jsonl", std::process::id()));
        std::fs::write(&path, format!("{}\n{}", line("a", 1, 0, 0, 1), &line("b", 2, 0, 0, 1)[..20])).unwrap();
        let mut s = Session::default();
        s.read_new(&path);
        assert_eq!(s.totals.calls, 1);
        std::fs::write(&path, format!("{}\n{}\n", line("a", 1, 0, 0, 1), line("b", 2, 0, 0, 1))).unwrap();
        s.read_new(&path);
        assert_eq!((s.totals.calls, s.totals.input), (2, 3));
        let _ = std::fs::remove_file(path);
    }
}

/// Same numbers as the Meu Claude panel for a real transcript:
/// `USAGE_JSONL=<file> cargo test usage::real -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn real() {
    let path = PathBuf::from(std::env::var("USAGE_JSONL").unwrap());
    let mut t = Tracked { path, cwd: String::new(), main: Session::default(), subagents: HashMap::new() };
    t.read();
    println!("{}", serde_json::to_string(&t.entry()).unwrap());
}
