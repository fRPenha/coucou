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

#[derive(Default, Serialize, Clone)]
pub struct Totals {
    pub calls: u64,
    /// Everything that went in: fresh input + cache written + cache read.
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// Context size of the latest call and the largest one.
    pub last: u64,
    pub peak: u64,
    pub compactions: u64,
    pub model: String,
}

#[derive(Default)]
struct Session {
    offset: u64,
    seen: HashSet<String>,
    totals: Totals,
}

impl Session {
    /// Adds one transcript line. Lines that are not an assistant message with
    /// usage, or repeat a message already counted, change nothing.
    fn feed(&mut self, line: &str) {
        let Ok(o) = serde_json::from_str::<Value>(line) else { return };
        if o.get("type").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let m = &o["message"];
        let (Some(u), Some(id)) = (m.get("usage"), m.get("id").and_then(Value::as_str)) else { return };
        if !self.seen.insert(id.to_string()) {
            return;
        }
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let (fresh, write, read) = (n("input_tokens"), n("cache_creation_input_tokens"), n("cache_read_input_tokens"));
        let context = fresh + write + read;
        let t = &mut self.totals;
        if t.calls > 0 && (context as f64) < COMPACTION_RATIO * t.last as f64 {
            t.compactions += 1;
        }
        t.calls += 1;
        t.input += context;
        t.cache_read += read;
        t.cache_write += write;
        t.output += n("output_tokens");
        t.last = context;
        t.peak = t.peak.max(context);
        if let Some(model) = m.get("model").and_then(Value::as_str) {
            t.model = model.to_string();
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

#[derive(Default)]
pub struct Store(Mutex<HashMap<String, (PathBuf, Session)>>);

#[derive(Serialize, Clone)]
struct Update<'a> {
    session_id: &'a str,
    totals: Totals,
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
        map.insert(sid.to_string(), (path, Session::default()));
    }
    let (path, session) = map.get_mut(sid).unwrap();
    let before = session.totals.calls;
    session.read_new(path);
    if session.totals.calls != before {
        let _ = app.emit_to(WINDOW_LABEL, "usage", Update { session_id: sid, totals: session.totals.clone() });
    }
}

/// Totals of every session seen since Coucou started, for the usage card.
#[tauri::command]
pub fn usage_sessions(store: tauri::State<'_, Store>) -> HashMap<String, Totals> {
    let map = store.0.lock().unwrap_or_else(PoisonError::into_inner);
    map.iter().map(|(k, (_, s))| (k.clone(), s.totals.clone())).collect()
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
    let mut s = Session::default();
    s.read_new(&PathBuf::from(std::env::var("USAGE_JSONL").unwrap()));
    println!("{}", serde_json::to_string(&s.totals).unwrap());
}
