//! Paja Desk: a desktop window over Paja's core. It is a fourth door to the same core as the
//! CLI, the MCP server and the plugin: its commands call the library functions the CLI calls
//! and return the JSON `paja --json` prints. It keeps no state of its own; everything comes from
//! replaying `$PAJA_HOME`'s event log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use paja::home::Home;
use paja::jono;
use serde_json::{json, Value};
use tauri::Emitter;

/// The event the window hears when Paja's event log changed, so it reads the queue again.
pub const CHANGED: &str = "paja://events-changed";

/// Every repository's work items, as `paja --json work list --all` prints them. An error is
/// `{"error": "..."}` with the text the CLI prints.
#[tauri::command]
fn work_list() -> Value {
    match Home::from_env() {
        Ok(home) => work_list_in(&home, &checkout()),
        Err(err) => error(err),
    }
}

/// [`work_list`] for `home`. `checkout` is only asked for its repository when the log still
/// holds events from before items carried one.
pub fn work_list_in(home: &Home, checkout: &Path) -> Value {
    match jono::list_all_from(home, checkout).and_then(|items| Ok(serde_json::to_value(items)?)) {
        Ok(items) => items,
        Err(err) => error(err),
    }
}

/// The directory Desk was started in: the checkout, when started from one.
fn checkout() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn error(err: anyhow::Error) -> Value {
    json!({ "error": format!("{err:#}") })
}

/// A fingerprint of the event log that changes whenever an event is appended: its length and
/// modification time. None while the file doesn't exist.
fn fingerprint(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// Tell the window each time the event log changes. The log is append-only, so a poll of its
/// size and time is enough, and works the same on every platform.
fn watch(app: tauri::AppHandle) {
    let Ok(home) = Home::from_env() else { return };
    let path = home.root().join("events.jsonl");
    std::thread::spawn(move || {
        let mut seen = fingerprint(&path);
        loop {
            std::thread::sleep(Duration::from_millis(750));
            let now = fingerprint(&path);
            if now != seen {
                seen = now;
                let _ = app.emit(CHANGED, ());
            }
        }
    });
}

/// Start Desk's window.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![work_list])
        .setup(|app| {
            watch(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Paja Desk could not start its window");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import(home: &Home, repo: &str, number: u64, title: &str) {
        let lock = home.lock().unwrap();
        home.events()
            .append(
                &lock,
                jono::EV_IMPORTED,
                json!({
                    "repo": repo,
                    "number": number,
                    "title": title,
                    "body": "",
                    "url": format!("https://github.com/{repo}/issues/{number}"),
                }),
            )
            .unwrap();
    }

    #[test]
    fn the_list_command_returns_what_the_cli_prints() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        import(&home, "acme/app", 3, "Add a login page");
        import(&home, "acme/lib", 1, "Fix the parser");
        {
            let lock = home.lock().unwrap();
            home.events()
                .append(
                    &lock,
                    jono::EV_STATUS,
                    json!({"repo": "acme/app", "number": 3, "status": "escalated", "note": "A or B?"}),
                )
                .unwrap();
        }
        let listed = work_list_in(&home, dir.path());
        // The CLI's `--json work list --all` prints exactly this value.
        let cli = serde_json::to_value(jono::list_all_from(&home, dir.path()).unwrap()).unwrap();
        assert_eq!(listed, cli);
        let items = listed.as_array().unwrap();
        assert_eq!(items.len(), 2, "{listed}");
        let app = items.iter().find(|i| i["repo"] == "acme/app").unwrap();
        assert_eq!(app["status"], "escalated");
        assert_eq!(app["note"], "A or B?");
    }

    #[test]
    fn an_error_is_the_text_the_cli_prints() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        std::fs::write(home.root().join("events.jsonl"), "not json\n").unwrap();
        let listed = work_list_in(&home, dir.path());
        let cli = jono::list_all_from(&home, dir.path()).unwrap_err();
        assert_eq!(listed, json!({ "error": format!("{cli:#}") }));
    }

    #[test]
    fn the_fingerprint_changes_when_an_event_is_appended() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let path = home.root().join("events.jsonl");
        assert_eq!(fingerprint(&path), None);
        import(&home, "acme/app", 1, "One");
        let first = fingerprint(&path);
        assert!(first.is_some());
        import(&home, "acme/app", 2, "Two");
        assert_ne!(fingerprint(&path), first);
    }
}
