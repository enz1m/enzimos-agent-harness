// SPDX-License-Identifier: MIT
use serde_json::{Value, json};

const SETTINGS: &str = "dev.enzui.Settings";

fn has_any(text: &str, cues: &[&str]) -> bool {
    cues.iter().any(|cue| text.contains(cue))
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
}

fn mentions(text: &str, name: &str) -> bool {
    let prompt: Vec<_> = words(text).collect();
    let name: Vec<_> = words(name).collect();
    !name.is_empty()
        && prompt.windows(name.len()).any(|group| {
            group
                .iter()
                .zip(&name)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
}

pub(super) fn named_app(prompt: &str, name: &str) -> bool {
    if name.eq_ignore_ascii_case("Files") {
        return words(prompt).any(|word| word == "Files");
    }
    if name.eq_ignore_ascii_case("Terminal") {
        let lower = prompt.to_lowercase();
        if lower.contains("terminal command")
            && !has_any(&lower, &["in terminal", "open terminal", "terminal app"])
        {
            return false;
        }
    }
    mentions(prompt, name)
}

pub(super) fn app_label<'a>(id: &str, registered_name: &'a str) -> &'a str {
    if id == "org.enzim.Browser" {
        "Browser"
    } else {
        registered_name
    }
}

pub(super) fn expected_apps(prompt: &str) -> impl Iterator<Item = &'static str> + '_ {
    [
        "Mail",
        "Calendar",
        "Files",
        "Terminal",
        "Music",
        "Store",
        "Task Manager",
        "Browser",
        "Settings",
    ]
    .into_iter()
    .filter(|name| named_app(prompt, name))
}

pub(super) fn requested_capability(app: &str, prompt: &str) -> &'static str {
    let prompt = prompt.to_lowercase();
    match app {
        "Mail" if has_any(&prompt, &["draft", "compose", "send"]) => "Prepare an email for review",
        "Calendar" if has_any(&prompt, &["create", "add", "schedule"]) => {
            "Create and verify a Calendar event"
        }
        "Calendar" => "Read Calendar events",
        "Files" if has_any(&prompt, &["which files", "list", "show"]) => "List the folder",
        "Files" => "Open the folder",
        "Terminal" => "Use a local Terminal session",
        "Music" => "Control Music playback",
        "Store" => "Find the app in Store",
        "Task Manager" => "List processes and CPU usage",
        "Browser" => "Open and read a Browser page",
        "Settings" => "Read or change the requested setting",
        _ => "Use the requested app function",
    }
}

pub(super) fn desktop_id(app: &str) -> Option<&'static str> {
    Some(match app {
        "Mail" => "io.github.enzui.Mail",
        "Calendar" => "io.github.enzui.Calendar",
        "Files" => "io.github.enzui.Files",
        "Terminal" => "io.github.enzui.Terminal",
        "Music" => "io.github.enzui.Music",
        "Store" => "io.github.enzui.Store",
        "Task Manager" => "io.github.enzui.TaskManager",
        "Browser" => "org.enzim.Browser",
        "Settings" => SETTINGS,
        _ => return None,
    })
}

fn launch_intent(prompt: &str) -> bool {
    prompt.split(['.', ',', ';', '!', '?', '\n']).any(|clause| {
        let words = words(clause).collect::<Vec<_>>();
        words.iter().enumerate().any(|(index, word)| {
            matches!(*word, "open" | "launch" | "start")
                && (index == 0
                    || matches!(words[index - 1], "and" | "then" | "please" | "you" | "to"))
        })
    })
}

fn setting_intent(prompt: &str) -> bool {
    has_any(
        prompt,
        &[
            "setting",
            "night light",
            "theme",
            "light mode",
            "dark mode",
            "brightness",
            "volume",
            "scale",
            "24-hour",
            "12-hour",
            "clock format",
            "time format",
            "keyboard layout",
            "wallpaper",
            "do not disturb",
            "notification",
        ],
    )
}

pub(super) fn alias(app: &Value, id: &str) -> String {
    let legacy = match (app["id"].as_str().unwrap_or_default(), id) {
        (SETTINGS, "settings.search") => "search_settings",
        (SETTINGS, "settings.read") => "get_setting",
        (SETTINGS, "settings.change") => "set_setting",
        (SETTINGS, "desktop.open") => "open_app",
        (SETTINGS, "desktop.timer") => "start_timer",
        (SETTINGS, "desktop.status") => "device_status",
        (SETTINGS, "desktop.downloads") => "find_downloads",
        ("org.enzim.Browser", "new_page") => "browser_open",
        ("org.enzim.Browser", "take_snapshot") => "browser_read",
        (_, "list_processes") if app["name"] == "Task Manager" => "task_manager_list",
        (_, "create_event") if app["name"] == "Calendar" => "calendar_create",
        (_, "list_events") if app["name"] == "Calendar" => "calendar_list",
        (_, "create_draft" | "draft_email" | "prepare_send") if app["name"] == "Mail" => {
            "mail_draft"
        }
        _ => "",
    };
    if !legacy.is_empty() {
        return legacy.to_owned();
    }
    let name = app["name"].as_str().unwrap_or_default();
    let alias = format!("{name}_{id}")
        .to_ascii_lowercase()
        .replace(|ch: char| !ch.is_ascii_alphanumeric(), "_");
    if alias.len() > 64 {
        let prefix = alias.chars().take(51).collect::<String>();
        let digest = crate::sha256_hex(alias.as_bytes());
        format!("{prefix}_{}", digest.get(..12).unwrap_or_default())
    } else {
        alias
    }
}

pub(super) fn rank(app: &Value, function: &Value, prompt: &str) -> u32 {
    let id = function["id"].as_str().unwrap_or_default();
    let lower = prompt.to_lowercase();
    let name = app_label(
        app["id"].as_str().unwrap_or_default(),
        app["name"].as_str().unwrap_or_default(),
    );
    if app["id"] == SETTINGS {
        return match id {
            "settings.search" | "settings.read" if setting_intent(&lower) => 40,
            "settings.change"
                if setting_intent(&lower)
                    && has_any(
                        &lower,
                        &[
                            "set ", "switch", "turn ", "enable", "disable", "change", "make ",
                            "adjust", "lower", "raise",
                        ],
                    ) =>
            {
                40
            }
            "desktop.downloads"
                if lower.contains("download")
                    || (lower.contains("pdf")
                        && has_any(&lower, &["today", "yesterday", "new"])
                        && has_any(&lower, &["find ", "which ", "list ", "look "])) =>
            {
                40
            }
            "desktop.open"
                if launch_intent(&lower)
                    && (!has_any(&lower, &["https://", "http://"])
                        || named_app(prompt, "Files")) =>
            {
                40
            }
            "desktop.timer" if has_any(&lower, &["timer", "countdown"]) => 40,
            "desktop.status"
                if has_any(
                    &lower,
                    &["cpu", "battery", "memory", "ram usage", "network status"],
                ) =>
            {
                40
            }
            _ => 0,
        };
    }
    if !named_app(prompt, name) {
        return 0;
    }
    if name == "Mail"
        && id == "prepare_send"
        && has_any(&lower, &["draft", "compose", "email", "send"])
    {
        return 25;
    }
    if name == "Files"
        && id == "open_folder"
        && has_any(
            &lower,
            &["downloads", "documents", "desktop", "pictures", "videos"],
        )
    {
        return 0;
    }
    let mut score = words(id)
        .filter(|word| word.len() > 2 && mentions(&lower, word))
        .count() as u32;
    let matching_action = [
        (
            &[
                "create", "add", "schedule", "draft", "new", "save", "compose",
            ][..],
            &["create", "add", "draft", "new"][..],
        ),
        (
            &[
                "read", "list", "find", "verify", "show", "which", "check", "tell", "heading",
            ][..],
            &["get", "list", "read", "snapshot", "search", "current"][..],
        ),
        (
            &["open", "navigate", "launch"][..],
            &["open", "page", "navigate", "go"][..],
        ),
        (
            &["play", "pause", "stop"][..],
            &["play", "pause", "stop"][..],
        ),
        (&["run", "execute"][..], &["run", "execute"][..]),
    ]
    .iter()
    .any(|(intent, functions)| {
        intent.iter().any(|word| mentions(&lower, word))
            && words(id).any(|word| functions.contains(&word))
    });
    if matching_action {
        score += 5;
    }
    if app["id"] == "org.enzim.Browser" && has_any(&lower, &["http://", "https://"]) {
        return match id {
            "new_page" | "take_snapshot" => 20,
            _ => score,
        };
    }
    score
}

pub(crate) fn clock_format_query(prompt: &str) -> Option<Value> {
    let lower = prompt.to_lowercase();
    (has_any(
        &lower,
        &["24-hour", "12-hour", "clock format", "time format"],
    ) && has_any(&lower, &["clock", "system", "setting"])
        && !has_any(
            &lower,
            &["switch ", "change ", "set ", "enable ", "disable "],
        ))
    .then(|| json!({"query":"24-hour"}))
}

pub(crate) fn resolve_files_label(prompt: &str, arguments: &mut serde_json::Map<String, Value>) {
    if args_text(arguments, "app").is_none_or(|app| !app.eq_ignore_ascii_case("Files")) {
        return;
    }
    const PLACES: &[&str] = &[
        "Downloads",
        "Documents",
        "Desktop",
        "Pictures",
        "Videos",
        "Music",
        "Home",
    ];
    if let Some(folder) = args_text(arguments, "folder").filter(|folder| !folder.trim().is_empty())
    {
        if let Some(label) = PLACES
            .iter()
            .find(|label| label.eq_ignore_ascii_case(folder.trim()))
        {
            arguments.insert("folder".into(), json!(label));
        }
        return;
    }
    let mut places = PLACES.iter().filter(|place| mentions(prompt, place));
    if let (Some(place), None) = (places.next(), places.next()) {
        arguments.insert("folder".into(), json!(place));
    }
}

fn args_text<'a>(arguments: &'a serde_json::Map<String, Value>, field: &str) -> Option<&'a str> {
    arguments.get(field).and_then(Value::as_str)
}

pub(crate) fn builtin_relevant(name: &str, prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    let explicit_web = has_any(&lower, &["http://", "https://"]);
    let file_name = lower.split_whitespace().any(|word| {
        !word.contains("://")
            && word
                .trim_end_matches(['.', '?', '!', ',', '"', '\''])
                .rsplit_once('.')
                .is_some_and(|(stem, extension)| {
                    !stem.is_empty()
                        && (1..=12).contains(&extension.len())
                        && extension.chars().all(|ch| ch.is_ascii_alphabetic())
                        && !word.contains('@')
                })
    });
    let workspace = file_name
        || has_any(
            &lower,
            &["workspace", "my agenda", "booking code in", "readme"],
        );
    let scratch = has_any(&lower, &["scratch", "temporary note", "temporary draft"]);
    let writing = has_any(
        &lower,
        &[
            "save ", "write ", "replace ", "create ", "make ", "edit ", "update ",
        ],
    );
    match name {
        "local_time" => has_any(
            &lower,
            &[
                "local time",
                "current time",
                "time is it",
                "time locally",
                "today's date",
                "date here",
                "tomorrow",
                "clock",
                "weekday",
            ],
        ),
        "web_fetch" => explicit_web && !named_app(prompt, "Browser"),
        "file_read" => workspace,
        "files_list" => {
            workspace
                && (!file_name
                    || has_any(
                        &lower,
                        &["which files", "which folders", "list ", "delete "],
                    ))
        }
        "file_write" => {
            workspace
                && writing
                && (!scratch || has_any(&lower, &["workspace note", "workspace file"]))
        }
        "file_rename" => workspace && lower.contains("rename "),
        "scratch_list" => scratch && (!file_name || has_any(&lower, &["list ", "which files"])),
        "scratch_read" => scratch,
        "scratch_write" => scratch && writing,
        "command" => {
            has_any(&lower, &["command", "script", "delete ", "remove "])
                && !named_app(prompt, "Terminal")
        }
        "screenshot" => crate::local::Task::asks_for_screenshot(prompt),
        "add_extension" => has_any(&lower, &["skill", "mcp server"]),
        _ => true,
    }
}

pub(super) fn parameters(app: &str, id: &str, schema: &Value) -> Value {
    let mut schema = schema.clone();
    if app == "io.github.enzui.Files"
        && id == "current_folder"
        && let Some(properties) = schema["properties"].as_object_mut()
    {
        properties.remove("workspace");
    }
    if id != "desktop.open" {
        return schema;
    }
    schema["properties"]["folder"]["minLength"] = json!(1);
    schema["properties"]["folder"]["description"] = json!(
        "For Files, provide the folder requested by the user, e.g. Downloads, Documents, Home, or an absolute path."
    );
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_short_name_ranks_native_open_and_read() {
        let browser = json!({"id": "org.enzim.Browser", "name": "enzim Browser"});
        let prompt = "Open https://example.com/ in Browser and read its heading";
        for function in ["new_page", "take_snapshot"] {
            assert!(rank(&browser, &json!({"id": function}), prompt) > 0);
        }
    }

    #[test]
    fn a_page_already_open_in_browser_does_not_request_an_app_launch() {
        assert!(!launch_intent("read the page open in browser"));
        assert!(launch_intent(
            "find today's pdf and open my downloads folder in files"
        ));
        assert!(launch_intent("check cpu use. open task manager"));
    }
}
