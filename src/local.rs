// SPDX-License-Identifier: MIT
use crate::device;
use serde_json::{Value, json};
use std::sync::LazyLock;

static PROFILES: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("local_profiles.json"))
        .expect("built-in local profiles must be valid JSON")
});

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Task {
    Text,
    Files,
    Write,
    Rename,
    Screen,
    Device(device::Tool),
    Full,
}

fn rename_operand(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    if let Some(quote @ ('"' | '\'' | '`')) = input.chars().next() {
        return input.strip_prefix(quote)?.split_once(quote);
    }
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    Some(input.split_at(end))
}

fn rename_paths(prompt: &str) -> Option<(&str, &str)> {
    let (first, rest) = prompt.trim().split_once(char::is_whitespace)?;
    let (verb, arguments) = if first.eq_ignore_ascii_case("please") {
        rest.trim_start().split_once(char::is_whitespace)?
    } else {
        (first, rest)
    };
    if !verb.eq_ignore_ascii_case("rename") {
        return None;
    }
    let (path, rest) = rename_operand(arguments)?;
    let (connector, new) = rest.trim_start().split_once(char::is_whitespace)?;
    if path.is_empty() || !connector.eq_ignore_ascii_case("to") {
        return None;
    }
    let (name, tail) = rename_operand(new)?;
    let name = if new.trim_start().starts_with(['"', '\'', '`']) {
        name
    } else {
        name.trim_end_matches(['.', '!', '?'])
    };
    (!name.is_empty()
        && !name.contains('/')
        && tail.trim().chars().all(|ch| matches!(ch, '.' | '!' | '?')))
    .then_some((path, name))
}

fn open_arguments(prompt: &str) -> Option<Value> {
    let (verb, rest) = rename_operand(prompt)?;
    let (verb, rest) = if verb.eq_ignore_ascii_case("please") {
        rename_operand(rest)?
    } else {
        (verb, rest)
    };
    if !verb.eq_ignore_ascii_case("open") {
        return None;
    }
    let (app, tail) = rename_operand(rest)?;
    let (preposition, folder) = rename_operand(tail)?;
    if app.eq_ignore_ascii_case("files")
        && ["in", "at", "to"]
            .iter()
            .any(|word| preposition.eq_ignore_ascii_case(word))
    {
        let folder = folder.trim();
        let folder = if folder.starts_with(['"', '\'', '`']) {
            let (folder, tail) = rename_operand(folder)?;
            if !tail.trim().chars().all(|ch| matches!(ch, '.' | '!' | '?')) {
                return None;
            }
            folder
        } else {
            folder.trim_end_matches(['.', '!', '?'])
        };
        return (!folder.is_empty() && !folder.chars().any(char::is_control))
            .then(|| json!({"app":"Files","folder":folder}));
    }

    let app = rest
        .trim()
        .trim_end_matches(['.', '!', '?'])
        .trim_matches(['"', '\'', '`']);
    let lower = app.to_lowercase();
    if app.is_empty()
        || app.len() > 120
        || app.chars().any(char::is_control)
        || lower.split_whitespace().any(|word| {
            matches!(
                word,
                "and" | "in" | "at" | "to" | "with" | "folder" | "the" | "app"
            )
        })
        || matches!(
            lower.as_str(),
            "downloads" | "documents" | "home" | "pictures" | "music" | "videos"
        )
    {
        return None;
    }
    Some(json!({"app":app}))
}

impl Task {
    pub(crate) fn asks_for_screenshot(prompt: &str) -> bool {
        let instruction = prompt
            .split([':', '\n'])
            .next()
            .unwrap_or(prompt)
            .trim()
            .to_lowercase();
        let instruction = ["please ", "can you ", "could you "]
            .iter()
            .fold(instruction.as_str(), |text, prefix| {
                text.strip_prefix(prefix).unwrap_or(text)
            });
        [
            "look at my screen",
            "look at the screen",
            "what's on my screen",
        ]
        .iter()
        .any(|request| instruction.starts_with(request))
            || (["take ", "capture ", "grab ", "get "]
                .iter()
                .any(|verb| instruction.starts_with(verb))
                && (instruction.split_whitespace().any(|word| {
                    word.trim_matches(|ch: char| !ch.is_alphanumeric()) == "screenshot"
                }) || instruction.contains("screen shot")
                    || instruction.contains("screen capture")
                    || instruction.contains("capture the screen")
                    || instruction.contains("capture my screen")))
    }

    pub(crate) fn direct_tool(self, prompt: &str) -> Option<(&'static str, Value)> {
        match self {
            Self::Device(tool) => {
                let arguments = if tool == device::Tool::Open {
                    open_arguments(prompt)?
                } else {
                    tool.grounded_arguments(prompt)?
                };
                Some((tool.name(), arguments))
            }
            Self::Screen => {
                let window = prompt
                    .split(|ch: char| !ch.is_alphanumeric())
                    .any(|word| word.eq_ignore_ascii_case("window"));
                let target = if window { "focused_window" } else { "screen" };
                Some(("screenshot", json!({"target": target})))
            }
            Self::Rename => {
                let (path, new_name) = rename_paths(prompt)?;
                Some(("file_rename", json!({"path": path, "new_name": new_name})))
            }
            Self::Files => {
                let mut words = prompt.split_whitespace();
                let verb = words.next()?;
                let verb = if verb.eq_ignore_ascii_case("please") {
                    words.next()?
                } else {
                    verb
                };
                if !verb.eq_ignore_ascii_case("read") {
                    return None;
                }
                let path = words
                    .next()?
                    .trim_end_matches([',', '.', '?', '!', ':'])
                    .trim_matches(['`', '"', '\'']);
                if !path.contains(['/', '.'])
                    || path.starts_with(['/', '~'])
                    || path.contains("://")
                {
                    return None;
                }
                Some(("file_read", json!({"path": path})))
            }
            _ => None,
        }
    }

    pub(crate) fn classify(prompt: &str, follows_tools: bool) -> Self {
        if Self::asks_for_screenshot(prompt) {
            return Self::Screen;
        }
        let instruction = prompt.split([':', '\n']).next().unwrap_or(prompt);
        let lower = instruction.trim().to_lowercase();
        let words = lower
            .split(|ch: char| !ch.is_alphanumeric() && !matches!(ch, '.' | '/' | '_'))
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        let command = |word| matches!(word, "run" | "execute" | "invoke");
        let first_action = match words.as_slice() {
            ["can" | "could" | "would", "you", rest @ ..] => rest,
            words => words,
        }
        .iter()
        .find(|word| **word != "please");
        if first_action.is_some_and(|word| command(*word))
            || (words.iter().copied().any(command)
                && ["command", "script", "terminal", "shell", "tool"]
                    .iter()
                    .any(|word| words.contains(word)))
        {
            return Self::Full;
        }
        if let Some(tool) = device::Tool::classify(&words, first_action.copied()) {
            return Self::Device(tool);
        }
        let file = words.iter().any(|word| {
            let word = word.trim_end_matches('.');
            (word.contains('/') && word.chars().any(char::is_alphabetic))
                || word.rsplit_once('.').is_some_and(|(stem, extension)| {
                    (stem.is_empty() || stem.chars().any(char::is_alphabetic))
                        && (1..=12).contains(&extension.len())
                        && extension.chars().all(|ch| ch.is_ascii_alphabetic())
                })
        });
        let workspace = words.iter().any(|word| {
            matches!(
                *word,
                "workspace"
                    | "folder"
                    | "directory"
                    | "carpeta"
                    | "directorio"
                    | "mappa"
                    | "mappában"
                    | "mappát"
                    | "mappámban"
            )
        });
        let action = |word| {
            matches!(
                word,
                "list"
                    | "read"
                    | "write"
                    | "create"
                    | "look"
                    | "check"
                    | "find"
                    | "open"
                    | "show"
                    | "replace"
                    | "edit"
                    | "update"
                    | "save"
                    | "summarize"
                    | "summarise"
                    | "rename"
            )
        };
        let named_file = first_action.is_some_and(|word| action(*word))
            && ["file", "files", "readme", "license", "makefile"]
                .iter()
                .any(|word| words.contains(word));
        let reference = words
            .iter()
            .any(|word| matches!(*word, "it" | "them" | "there"));
        if file
            || workspace
            || named_file
            || (follows_tools && reference && words.iter().copied().any(action))
        {
            if first_action == Some(&"rename") {
                return Self::Rename;
            }
            let writing = words.iter().any(|word| {
                matches!(
                    *word,
                    "write" | "create" | "replace" | "edit" | "update" | "save"
                )
            });
            if writing { Self::Write } else { Self::Files }
        } else {
            Self::Text
        }
    }

    pub(crate) fn from_turn(prompt: &str, history: &[Value]) -> Self {
        let follows_tools = history
            .iter()
            .rev()
            .take_while(|message| message["role"] != "user")
            .any(|message| message["role"] == "tool");
        Self::classify(prompt, follows_tools)
    }
}

pub(crate) struct Profile {
    pub(crate) instructions: String,
    pub(crate) tools: Vec<Value>,
    pub(crate) optional_digest: bool,
    pub(crate) tune_sampling: bool,
}

impl Profile {
    // Leave room for template framing and output in the runtime’s 4,608-token default context.
    pub(crate) const MESSAGE_BYTES: usize = 10 * 1024;
    pub(crate) const RESULT_BYTES: usize = 3 * 1024;
    pub(crate) const VISION_MODEL: &str = "lfm3-vl-2b";

    pub(crate) fn is_builtin(provider: &str, model: &str) -> bool {
        provider == "local" && PROFILES["models"].get(model).is_some()
    }

    pub(crate) fn for_model(
        provider: &str,
        model: &str,
        task: Task,
        full_instructions: &str,
        full_tools: &[Value],
        desktop_available: bool,
    ) -> Option<Self> {
        if provider != "local" {
            return None;
        }
        let model_prompt = PROFILES["models"].get(model)?.as_str()?;
        if task == Task::Write && model == "qwen3.5-0.8b" {
            return Some(Self {
                instructions: full_instructions.to_owned(),
                tools: full_tools
                    .iter()
                    .filter(|tool| tool["function"]["name"] != "file_rename")
                    .cloned()
                    .collect(),
                optional_digest: false,
                tune_sampling: false,
            });
        }
        let mut instructions = format!("{}{model_prompt}", PROFILES["prompt"].as_str()?);
        let tools = match task {
            Task::Text => {
                if !desktop_available {
                    instructions.push_str(PROFILES["chat_only"].as_str()?);
                }
                Vec::new()
            }
            Task::Device(_) => {
                instructions.push_str("\nUse the desktop tool for the request. Answer from its result in the user's language.");
                Vec::new()
            }
            Task::Screen => full_tools
                .iter()
                .filter(|tool| tool["function"]["name"] == "screenshot")
                .cloned()
                .collect(),
            Task::Rename => {
                instructions.push_str(PROFILES["workspace"].as_str()?);
                full_tools
                    .iter()
                    .filter(|tool| tool["function"]["name"] == "file_rename")
                    .cloned()
                    .collect()
            }
            Task::Files | Task::Write => {
                instructions.push_str(PROFILES["workspace"].as_str()?);
                if task == Task::Write {
                    instructions.push_str(PROFILES["write"].as_str()?);
                }
                PROFILES["tools"]
                    .as_array()?
                    .iter()
                    .filter(|tool| task == Task::Write || tool["name"] != "file_write")
                    .map(|tool| json!({"type": "function", "function": tool}))
                    .collect()
            }
            Task::Full => {
                instructions.push_str("\n\n");
                instructions.push_str(full_instructions);
                full_tools.to_vec()
            }
        };
        Some(Self {
            instructions,
            tools,
            optional_digest: matches!(task, Task::Files | Task::Write),
            tune_sampling: true,
        })
    }

    pub(crate) fn sample(body: &mut Value) {
        if let (Some(request), Some(sampling)) =
            (body.as_object_mut(), PROFILES["sampling"].as_object())
        {
            request.extend(sampling.clone());
        }
    }
}

pub(crate) fn pick_model(catalog: &Value, task: Task, prefer_vision: bool) -> Option<String> {
    let models = catalog["data"].as_array()?;
    let text_models = models.iter().filter(|model| model["kind"] == "llm");
    if prefer_vision
        && text_models
            .clone()
            .any(|model| model["id"] == Profile::VISION_MODEL)
    {
        return Some(Profile::VISION_MODEL.to_owned());
    }
    let id = |entry: &Value| entry["id"].as_str().map(str::to_owned);
    let workspace = if matches!(task, Task::Text | Task::Screen | Task::Device(_)) {
        None
    } else {
        [Profile::VISION_MODEL, "qwen3.5-0.8b", "minicpm5-1b"]
            .into_iter()
            .find(|name| text_models.clone().any(|model| model["id"] == *name))
            .map(str::to_owned)
    };
    workspace
        .or_else(|| {
            text_models
                .clone()
                .find(|model| model["default"] == true)
                .and_then(id)
        })
        .or_else(|| text_models.filter_map(id).next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_selects_only_installed_text_models() {
        let catalog = json!({"data": [
            {"id": "qwen3.5-0.8b", "kind": "llm"},
            {"id": "minicpm5-1b", "kind": "llm", "default": true},
            {"id": "dictation", "kind": "speech", "default": true}
        ]});
        assert_eq!(
            pick_model(&catalog, Task::Text, true).as_deref(),
            Some("minicpm5-1b")
        );
        assert_eq!(
            pick_model(&catalog, Task::Files, false).as_deref(),
            Some("qwen3.5-0.8b")
        );
        let mut catalog = catalog;
        catalog["data"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": Profile::VISION_MODEL, "kind": "llm"}));
        assert_eq!(
            pick_model(&catalog, Task::Text, true).as_deref(),
            Some(Profile::VISION_MODEL)
        );
        assert_eq!(
            pick_model(&catalog, Task::Text, false).as_deref(),
            Some("minicpm5-1b")
        );
    }

    #[test]
    fn explicit_rename_binds_the_original_path_and_destination_name() {
        assert_eq!(
            Task::Rename
                .direct_tool("Please rename \"reports/April notes.txt\" to \"May notes.txt\"."),
            Some((
                "file_rename",
                json!({"path":"reports/April notes.txt", "new_name":"May notes.txt"})
            ))
        );
        assert_eq!(
            Task::Rename.direct_tool("Rename notes/plan.md to outline.md."),
            Some((
                "file_rename",
                json!({"path":"notes/plan.md", "new_name":"outline.md"})
            ))
        );
        assert_eq!(
            Task::Rename.direct_tool("Rename plan.md to outline.md and share it"),
            None
        );
    }

    #[test]
    fn desktop_routing_preserves_explicit_arguments_and_leaves_ambiguous_requests_to_the_model() {
        let cases = [
            (
                "Can you make the monitor dimmer?",
                Some((
                    "desktop_setting",
                    json!({"setting":"brightness","value":"down"}),
                )),
            ),
            (
                "Please set a 12-minute countdown.",
                Some(("desktop_timer", json!({"seconds":720}))),
            ),
            ("Set a 1.5 minute timer.", None),
            ("Set a -10 second timer.", None),
            (
                "Locate the zip downloaded yesterday.",
                Some((
                    "find_downloads",
                    json!({"date":"yesterday","extension":"zip"}),
                )),
            ),
            (
                "List the PDF downloads from 2026-02-05.",
                Some((
                    "find_downloads",
                    json!({"date":"2026-02-05","extension":"pdf"}),
                )),
            ),
        ];
        for (prompt, expected) in cases {
            assert_eq!(
                Task::classify(prompt, false).direct_tool(prompt),
                expected,
                "{prompt}"
            );
        }
    }

    #[test]
    fn workspace_evidence_selects_file_tools_independently_of_language() {
        assert!(Task::classify("What is the capital of France?", false) == Task::Text);
        assert!(Task::classify("How does CPU scheduling work?", false) == Task::Text);
        assert_eq!(
            Task::Device(device::Tool::Open)
                .direct_tool("Please open Files at \"/tmp/Project files.\""),
            Some((
                "desktop_open",
                json!({"app":"Files","folder":"/tmp/Project files."})
            ))
        );
        assert_eq!(
            Task::Device(device::Tool::Open).direct_tool("Open Calculator."),
            Some(("desktop_open", json!({"app":"Calculator"})))
        );
        assert!(Task::classify("What is a battery?", false) == Task::Text);
        assert!(
            Task::classify("How much RAM is this computer using?", false)
                == Task::Device(device::Tool::Status)
        );
        for request in [
            "Please lower the volume",
            "Enable dark mode.",
            "Activa el modo oscuro.",
        ] {
            assert!(Task::classify(request, false) == Task::Device(device::Tool::Setting));
        }
        assert!(
            Task::classify("Locate the ZIP I downloaded yesterday", false)
                == Task::Device(device::Tool::Downloads)
        );
        assert!(Task::classify("Read todo.txt", false) == Task::Files);
        assert!(Task::classify("Mi van ebben a mappában?", false) == Task::Files);
        assert!(Task::classify("olvasd el a todo.txt fájlt", false) == Task::Files);
        assert!(Task::classify("Fordítsd le angolra: Hamarosan találkozunk.", false) == Task::Text);
        assert!(
            Task::classify("Could you rename photos/old.jpg to new.jpg", false) == Task::Rename
        );
        assert!(Task::classify("¿Qué hay en mi carpeta?", false) == Task::Files);
        assert!(Task::classify("¿Puedes explicar la fotosíntesis?", false) == Task::Text);
        assert!(Task::classify("Fogalmazz meg egy születésnapi üdvözletet.", false) == Task::Text);
    }
}
