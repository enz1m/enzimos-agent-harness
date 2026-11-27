// SPDX-License-Identifier: MIT
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
    Screen,
    Full,
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
            Self::Screen => {
                let window = prompt
                    .split(|ch: char| !ch.is_alphanumeric())
                    .any(|word| word.eq_ignore_ascii_case("window"));
                let target = if window { "focused_window" } else { "screen" };
                Some(("screenshot", json!({"target": target})))
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
        let first_action = words.iter().find(|word| **word != "please");
        if first_action.is_some_and(|word| command(*word))
            || (words.iter().copied().any(command)
                && ["command", "script", "terminal", "shell", "tool"]
                    .iter()
                    .any(|word| words.contains(word)))
        {
            return Self::Full;
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
        let workspace = words
            .iter()
            .any(|word| matches!(*word, "workspace" | "folder" | "directory"));
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
            let writing = words.iter().any(|word| {
                matches!(
                    *word,
                    "write" | "create" | "replace" | "edit" | "update" | "save"
                )
            });
            if writing { Self::Write } else { Self::Files }
        } else if prompt
            .chars()
            .any(|ch| ch.is_alphabetic() && !ch.is_ascii())
        {
            // The keyword rules above are English; elsewhere keep the file tools
            // rather than risk answering a file request without them.
            Self::Files
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
    ) -> Option<Self> {
        if provider != "local" {
            return None;
        }
        let model_prompt = PROFILES["models"].get(model)?.as_str()?;
        if task == Task::Write && model == "qwen3.5-0.8b" {
            return Some(Self {
                instructions: full_instructions.to_owned(),
                tools: full_tools.to_vec(),
                optional_digest: false,
                tune_sampling: false,
            });
        }
        let mut instructions = format!("{}{model_prompt}", PROFILES["prompt"].as_str()?);
        let tools = match task {
            Task::Text => Vec::new(),
            Task::Screen => full_tools
                .iter()
                .filter(|tool| tool["function"]["name"] == "screenshot")
                .cloned()
                .collect(),
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
    let workspace = if matches!(task, Task::Text | Task::Screen) {
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
    fn requests_without_english_keywords_keep_the_file_tools() {
        assert!(Task::classify("What is the capital of France?", false) == Task::Text);
        assert!(Task::classify("Read todo.txt", false) == Task::Files);
        assert!(Task::classify("Mi van ebben a mappában?", false) == Task::Files);
        assert!(Task::classify("olvasd el a todo.txt fájlt", false) == Task::Files);
    }
}
