// Claude Code hook installation.
//
// The rule from CLAUDE.md is strict and is followed to the letter:
// read %USERPROFILE%\.claude\settings.json, take a dated backup, merge without
// touching anybody else's hooks, show the diff, and write only after an explicit
// click. Uninstall removes Coucou's entries and nothing else.
//
// The command is only the quoted exe path in forward slashes plus the event name:
// on Windows Claude Code runs hook commands through Git Bash, and anything with
// PowerShell or cmd in it breaks.
//
// Reading, the diff, the backup and the write itself live in config_file.rs,
// shared with every other agent's installer (agents.rs).
//
// Fork (Meu Claude): the hooks are a local Claude Code plugin in
// ~/.claude/skills/coucou-hooks, not entries in settings.json. settings.json
// can be shared between machines (here a symlink into a dotfiles repo) while
// Coucou is installed per machine. Claude Code loads that folder as
// `coucou-hooks@skills-dir` in the CLI and in the desktop app's Code tab
// (checked 2026-10-10, Claude Code 2.1.282). The status line, when wanted,
// still goes to settings.json: a plugin cannot carry one.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Map, Value};
use tauri::{AppHandle, Manager};
use crate::agents::{self, Shell};
use crate::config_file::{self, FileEdit};
use crate::{platform, settings};

/// Every event the island reacts to, with the hook timeout written to settings.json.
/// PermissionRequest waits for a human, so it gets the decision timeout + 10 s.
pub const HOOK_EVENTS: &[(&str, u64)] = &[
    ("SessionStart", 10),
    ("SessionEnd", 10),
    ("UserPromptSubmit", 10),
    ("PreToolUse", 10),
    ("PostToolUse", 10),
    ("PostToolUseFailure", 10),
    ("PermissionRequest", 120),
    ("Notification", 10),
    ("Stop", 10),
    ("StopFailure", 10),
    ("SubagentStart", 10),
    ("SubagentStop", 10),
];

/// Marker that identifies a Coucou entry inside settings.json.
const MARKER: &str = "coucou-hook";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookStatus {
    pub installed: bool,
    /// Coucou's status line relay (plan usage) is the one in settings.json.
    pub plan_relay_installed: bool,
    pub settings_path: String,
    pub hook_path: String,
    pub hook_ready: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookPreview {
    pub diff: String,
    pub backup: String,
    pub settings_path: String,
    /// Identifies the bytes this diff was computed from; handed back to `write`
    /// so we only ever apply what the user actually looked at.
    pub fingerprint: String,
}

pub fn settings_path() -> PathBuf {
    platform::home_dir().join(".claude").join("settings.json")
}

/// The settings as they are, or an empty object when we cannot tell. Only for
/// read-only paths like `status()`, which must never fail loudly; anything that
/// writes goes through `config_file`, which surfaces the error instead.
fn read_settings_lossy() -> Value {
    config_file::read(&settings_path())
        .ok()
        .and_then(|bytes| config_file::parse_json(bytes.as_deref(), "settings.json").ok())
        .unwrap_or_else(|| json!({}))
}

/// On Windows Claude Code runs hook commands through Git Bash; on Linux through
/// `sh`. Either way the relay path is one quoted shell word.
fn hook_command(event: &str) -> String {
    agents::relay_command(Shell::Sh, event)
}

/// The status line in settings.json is Coucou's relay (old installs wrote
/// `coucou-hook StatusLine`, new ones `coucou-hook --statusline`; both match).
fn status_line_is_ours(v: &Value) -> bool {
    v.get("command").and_then(Value::as_str).is_some_and(|c| c.contains(MARKER))
}

fn unexpected(what: &str) -> String {
    crate::i18n::tf("settings.json: {what} has an unexpected type — Coucou has not touched it.", &[("what", what)])
}

/// The plugin folder Claude Code loads by itself, without a settings entry.
pub fn plugin_dir() -> PathBuf {
    platform::home_dir().join(".claude").join("skills").join("coucou-hooks")
}

/// The plugin's hooks: every event the island reacts to except approvals.
// ponytail: PermissionRequest left out on purpose — approvals stay in Claude
// Code's own prompt instead of waiting up to 108 s for the island. Add it back
// to HOOK_EVENTS' filter here if approving from the island is wanted.
fn plugin_hooks() -> Value {
    let hooks: Map<String, Value> = HOOK_EVENTS
        .iter()
        .filter(|(event, _)| *event != "PermissionRequest")
        .map(|(event, timeout)| {
            let entry = json!([{ "hooks": [{ "type": "command", "command": hook_command(event), "timeout": timeout }] }]);
            ((*event).to_string(), entry)
        })
        .collect();
    json!({ "hooks": hooks })
}

/// Install writes the plugin's two files, uninstall removes them.
fn edits(install: bool) -> Vec<FileEdit<'static>> {
    let dir = plugin_dir();
    let manifest = json!({
        "name": "coucou-hooks",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Coucou: Claude Code sessions in the island",
    });
    vec![
        FileEdit {
            path: dir.join(".claude-plugin").join("plugin.json"),
            edit: config_file::json_edit("plugin.json".into(), move |_| Ok(install.then(|| manifest.clone()))),
        },
        FileEdit {
            path: dir.join("hooks").join("hooks.json"),
            edit: config_file::json_edit("hooks.json".into(), move |_| Ok(install.then(plugin_hooks))),
        },
    ]
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn status() -> HookStatus {
    let current = read_settings_lossy();
    let installed = plugin_dir().join("hooks").join("hooks.json").is_file();
    let hook_path = settings::hook_exe_path();
    HookStatus {
        installed,
        plan_relay_installed: plan_relay_installed(&current),
        settings_path: settings_path().to_string_lossy().to_string(),
        hook_ready: hook_path.exists(),
        hook_path: hook_path.to_string_lossy().to_string(),
    }
}

pub fn preview(install: bool) -> Result<HookPreview, String> {
    Ok(plan_to_preview(config_file::preview(&edits(install))?))
}

fn plan_to_preview(plan: config_file::Plan) -> HookPreview {
    HookPreview {
        diff: plan.diff,
        backup: plan.backup,
        settings_path: plan.path,
        fingerprint: plan.fingerprint,
    }
}

/// Writes the merged (or cleaned) settings after taking a dated backup, and
/// returns where the backup went ("" when there was no file to back up).
///
/// `fingerprint` is the one the preview was computed from: a settings.json that
/// changed in between is refused rather than overwritten (see config_file).
pub fn write(install: bool, fingerprint: &str) -> Result<String, String> {
    let backups = config_file::apply(&edits(install), fingerprint)?;
    Ok(backups.first().map(|p| p.to_string_lossy().to_string()).unwrap_or_default())
}

// ── Status line (plan usage) ──────────────────────────────────────────────────
//
// Claude Code runs one `statusLine` command and hands it the plan limits. Coucou
// puts its relay there; a status line the user already had is kept in
// statusline-previous.json beside the relay, and the relay still runs it, so it
// keeps working. Installing and removing it is separate from the hooks, and
// goes through the same hardened writer (config_file.rs): strict read, diff,
// fingerprint, dated backup, then the write.

/// Where the user's own status line waits while the relay stands in for it.
pub fn status_line_previous_path() -> PathBuf {
    settings::hook_exe_path().with_file_name("statusline-previous.json")
}

fn read_status_line_previous() -> Option<Value> {
    let bytes = std::fs::read(status_line_previous_path()).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok().filter(Value::is_object)
}

/// True when the `statusLine` in settings.json is Coucou's relay.
pub fn plan_relay_installed(settings: &Value) -> bool {
    settings.get("statusLine").is_some_and(status_line_is_ours)
}

/// `statusLine` as it reads after installing or removing the relay. `None`: the
/// key goes. Installing swaps only `command`, so `padding`, `refreshInterval`
/// and the rest of the user's status line stay as they were.
fn status_line_after(existing: Option<&Value>, install: bool, previous: Option<&Value>) -> Option<Value> {
    if install {
        let mut sl = existing.filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
        let obj = sl.as_object_mut().expect("an object");
        obj.entry("type").or_insert_with(|| json!("command"));
        obj.insert("command".into(), json!(hook_command("--statusline")));
        Some(sl)
    } else if existing.is_some_and(status_line_is_ours) {
        previous.cloned()
    } else {
        existing.cloned() // not ours any more (the user changed it): leave it alone
    }
}

fn status_line_settings(current: &Value, install: bool, previous: Option<&Value>) -> Result<Value, String> {
    let mut root = current.as_object().cloned().unwrap_or_default();
    // A statusLine that is not an object is something we do not understand:
    // refuse rather than replace it.
    if install && root.get("statusLine").is_some_and(|v| !v.is_object()) {
        return Err(unexpected("\"statusLine\""));
    }
    match status_line_after(root.get("statusLine"), install, previous) {
        Some(sl) => root.insert("statusLine".into(), sl),
        None => root.remove("statusLine"),
    };
    Ok(Value::Object(root))
}

fn status_line_edits(install: bool, previous: Option<Value>) -> Vec<FileEdit<'static>> {
    vec![FileEdit {
        path: settings_path(),
        edit: config_file::json_edit("settings.json".into(), move |current| {
            Ok(Some(status_line_settings(current, install, previous.as_ref())?))
        }),
    }]
}

/// The diff the user has to look at before the relay goes in or out.
pub fn status_line_preview(install: bool) -> Result<HookPreview, String> {
    Ok(plan_to_preview(config_file::preview(&status_line_edits(install, read_status_line_previous()))?))
}

/// Installs or removes the relay, after the same backup and fingerprint checks as
/// the hooks. Installing first saves a status line of the user's own, removing
/// puts it back (or removes the key if there was none).
pub fn status_line_write(install: bool, fingerprint: &str) -> Result<String, String> {
    let before = config_file::read(&settings_path())
        .and_then(|bytes| config_file::parse_json(bytes.as_deref(), "settings.json"))?;
    let previous = read_status_line_previous();
    let own = before.get("statusLine").filter(|v| !status_line_is_ours(v)).cloned();
    let saved = match (install, own.as_ref()) {
        (true, Some(own)) => {
            save_status_line_previous(own).map_err(|_| {
                crate::i18n::t("Could not save your current status line next to the relay; nothing was changed.")
            })?;
            true
        }
        _ => false,
    };
    let result = config_file::apply(&status_line_edits(install, previous), fingerprint);
    match &result {
        // Back in settings.json: the saved copy has done its job.
        Ok(_) if !install => {
            let _ = std::fs::remove_file(status_line_previous_path());
        }
        // Nothing was written: do not leave a stale copy behind.
        Err(_) if saved => {
            let _ = std::fs::remove_file(status_line_previous_path());
        }
        _ => {}
    }
    let backups = result?;
    Ok(backups.first().map(|p| p.to_string_lossy().to_string()).unwrap_or_default())
}

fn save_status_line_previous(status_line: &Value) -> std::io::Result<()> {
    let path = status_line_previous_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    config_file::write_like(&path, &path, config_file::pretty(status_line).as_bytes())
}

/// Copies the relay (coucou-hook.exe / coucou-hook) into the local data dir's
/// bin/ on launch. In a bundled install it comes from the app resources; in
/// `tauri dev` it sits next to the app binary in the workspace target directory.
///
/// Every candidate is tried rather than just the first, because getting this
/// wrong is silent and fatal: `resources` used to be a glob, which made NSIS
/// mirror the source path into `_up_\target\release\`, no candidate matched, and
/// the relay was simply never installed. It only looked healthy on a developer
/// machine, where a leftover copy from `tauri dev` was already sitting in bin/.
pub fn ensure_hook_exe(app: &AppHandle) {
    let dest = settings::hook_exe_path();
    let Some(dir) = dest.parent() else { return };
    // Nobody else may swap the relay Claude Code runs: its folder is ours only.
    if platform::ensure_private_dir(&settings::local_dir()).is_err()
        || std::fs::create_dir_all(dir).is_err()
    {
        return;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = app.path().resolve(platform::HOOK_EXE, tauri::path::BaseDirectory::Resource) {
        candidates.push(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            // Installed build, then `tauri dev` (target/debug) next to the
            // release hook the pre-build step produces.
            candidates.push(parent.join(platform::HOOK_EXE));
            candidates.push(parent.join("../release").join(platform::HOOK_EXE));
            // Belt and braces: where the old glob form used to land it.
            candidates.push(parent.join("_up_/target/release").join(platform::HOOK_EXE));
        }
    }

    let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    let Some(src) = candidates.into_iter().find(|p| p.exists()) else {
        crate::log::line(format!(
            "{} not found — Claude Code hooks cannot work. Looked in: {}",
            platform::HOOK_EXE,
            tried.join(", ")
        ));
        return;
    };
    install_relay(&src, &dest);
}

#[cfg(windows)]
fn install_relay(src: &Path, dest: &Path) {
    let same = match (std::fs::metadata(src), std::fs::metadata(dest)) {
        (Ok(a), Ok(b)) => a.len() == b.len() && a.modified().ok() == b.modified().ok(),
        _ => false,
    };
    if same {
        return;
    }
    // A hook may be running right now and hold the file open; keeping the old
    // copy is fine, it is the same relay.
    if let Err(err) = std::fs::copy(src, dest) {
        if !dest.exists() {
            crate::log::line(format!("could not install {}: {err}", platform::HOOK_EXE));
        }
    }
}

/// Linux does not keep the modification time on copy, so the contents decide.
/// The new relay is written beside the old one and renamed over it: a hook
/// starting at that moment runs either the old relay or the new one, never half
/// of one, and a relay that is running right now does not block the update.
#[cfg(unix)]
fn install_relay(src: &Path, dest: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if matches!((std::fs::read(src), std::fs::read(dest)), (Ok(a), Ok(b)) if a == b) {
        return;
    }
    let temp = dest.with_extension(format!("new-{}", std::process::id()));
    let result = std::fs::copy(src, &temp)
        .and_then(|_| std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755)))
        .and_then(|_| std::fs::rename(&temp, dest));
    if let Err(err) = result {
        let _ = std::fs::remove_file(&temp);
        crate::log::line(format!("could not install {}: {err}", platform::HOOK_EXE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_relay_takes_the_status_line_and_keeps_the_users_other_fields() {
        // None yet: ours is added.
        let fresh = status_line_settings(&json!({ "model": "opus" }), true, None).unwrap();
        assert!(status_line_is_ours(&fresh["statusLine"]));
        assert_eq!(fresh["statusLine"]["type"], "command");
        assert_eq!(fresh["model"], "opus");

        // Their own: only the command is swapped; padding and refresh stay.
        let own = json!({ "statusLine": { "type": "command", "command": "~/bin/my-line", "padding": 2, "refreshInterval": 5 } });
        let taken = status_line_settings(&own, true, None).unwrap();
        assert!(status_line_is_ours(&taken["statusLine"]));
        assert_eq!(taken["statusLine"]["padding"], 2);
        assert_eq!(taken["statusLine"]["refreshInterval"], 5);

        // Installing again keeps ours and its extra fields.
        let again = status_line_settings(&taken, true, None).unwrap();
        assert_eq!(again["statusLine"]["padding"], 2);
        assert!(status_line_is_ours(&again["statusLine"]));
    }

    #[test]
    fn removing_the_relay_restores_what_was_there_and_never_touches_anything_else() {
        let previous = json!({ "type": "command", "command": "~/bin/my-line", "padding": 2 });
        let ours = status_line_settings(&json!({}), true, None).unwrap();

        // There was one before: it comes back exactly.
        assert_eq!(status_line_settings(&ours, false, Some(&previous)).unwrap()["statusLine"], previous);
        // There was none: the key goes.
        assert!(status_line_settings(&ours, false, None).unwrap().get("statusLine").is_none());
        // The user changed it since: not ours, so untouched.
        let theirs = json!({ "statusLine": { "type": "command", "command": "~/bin/other" } });
        assert_eq!(status_line_settings(&theirs, false, Some(&previous)).unwrap()["statusLine"], theirs["statusLine"]);
        // A statusLine we do not understand is refused, never replaced.
        assert!(status_line_settings(&json!({ "statusLine": "echo hi" }), true, None).is_err());
    }

    /// Everything filesystem-shaped lives in one test on purpose: it points
    /// the home directory at a temp directory, and that is process-wide.
    #[test]
    fn the_hooks_are_a_plugin_and_settings_json_is_never_touched() {
        let tmp = std::env::temp_dir().join(format!("coucou-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".claude")).unwrap();
        std::env::set_var(platform::HOME_VAR, &tmp);
        assert!(plugin_dir().starts_with(&tmp), "the test must not touch the real home");

        let settings = br#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"rtk hook"}]}]}}"#;
        std::fs::write(settings_path(), settings).unwrap();

        // Install: the two plugin files, every event but PermissionRequest.
        let plan = preview(true).unwrap();
        assert!(plan.diff.contains("coucou-hook"), "the diff must show what changes");
        write(true, &plan.fingerprint).expect("install should succeed");
        let hooks: Value = serde_json::from_slice(&std::fs::read(plugin_dir().join("hooks/hooks.json")).unwrap()).unwrap();
        assert!(hooks["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap().contains("coucou-hook"));
        assert!(hooks["hooks"].get("PermissionRequest").is_none());
        let manifest: Value = serde_json::from_slice(&std::fs::read(plugin_dir().join(".claude-plugin/plugin.json")).unwrap()).unwrap();
        assert_eq!(manifest["name"], "coucou-hooks");
        assert!(status().installed);
        assert_eq!(std::fs::read(settings_path()).unwrap(), settings);

        // A plugin file that moved since the preview is refused.
        let stale = preview(false).unwrap();
        std::fs::write(plugin_dir().join("hooks/hooks.json"), b"{}").unwrap();
        assert!(write(false, &stale.fingerprint).unwrap_err().contains("changed since the preview"));

        // Uninstall removes both files and still leaves settings.json alone.
        write(false, &preview(false).unwrap().fingerprint).expect("uninstall should succeed");
        assert!(!status().installed);
        assert!(!plugin_dir().join(".claude-plugin/plugin.json").exists());
        assert_eq!(std::fs::read(settings_path()).unwrap(), settings);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
