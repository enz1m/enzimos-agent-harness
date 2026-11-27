// SPDX-License-Identifier: MIT
use crate::{Result, ToolError, desktop, fail, provider::read_line};
use serde_json::{Value, json};
use std::{os::unix::fs::FileTypeExt, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::UnixStream,
};

const FRAME: usize = 4096;
pub(crate) const LOCKED: &str = "The desktop is locked.";
pub(crate) const UNAVAILABLE: &str = "This control or reading is unavailable on this desktop.";
pub(crate) const TIMER_LIMIT: &str = "The desktop timer limit has been reached.";
pub(crate) const APP_UNAVAILABLE: &str = "No unique installed app matched that name.";
pub(crate) const APP_LAUNCH_FAILED: &str = "The app is installed but could not be started.";
pub(crate) const APP_RESTRICTED: &str = "This account cannot launch the requested app.";
pub(crate) const FOLDER_UNAVAILABLE: &str = "That folder is unavailable.";
pub(crate) const NO_BACKLIGHT: &str = "No controllable screen backlight was found.";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    Setting,
    Preference(&'static str),
    ReadSetting(&'static str),
    Open,
    Timer,
    Status,
    Downloads,
}

impl Tool {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Setting | Self::Preference(_) => "desktop_setting",
            Self::ReadSetting(_) => "desktop_read_setting",
            Self::Open => "desktop_open",
            Self::Timer => "desktop_timer",
            Self::Status => "device_status",
            Self::Downloads => "find_downloads",
        }
    }

    pub(crate) fn changes_state(self) -> bool {
        matches!(
            self,
            Self::Setting | Self::Preference(_) | Self::Open | Self::Timer
        )
    }

    pub(crate) fn accepts_setting(self, setting: &Value) -> bool {
        match self {
            Self::Setting => matches!(
                setting.as_str(),
                Some("dark_mode" | "dnd" | "brightness" | "volume")
            ),
            Self::Preference(target) | Self::ReadSetting(target) => setting == target,
            _ => true,
        }
    }

    pub(crate) fn schema(self) -> Value {
        let (description, properties, required) = match self {
            Self::Setting => (
                "Change a desktop setting. Use on/off for dark mode or Do Not Disturb. Use a percent (e.g. 40) or up/down for brightness or volume. Volume also accepts mute/unmute.",
                json!({"setting":{"enum":["dark_mode","dnd","brightness","volume"]},"value":{"type":"string"}}),
                json!(["setting", "value"]),
            ),
            Self::Preference(setting) => (
                match setting {
                    "night_light" | "notification_previews" => "Use on or off.",
                    "shell_style" => "Use frame or islands.",
                    "wallpaper" => "Use default or a #RRGGBB color.",
                    "power_mode" => "Use power-saver, balanced, or performance.",
                    "keyboard_layout" => "Use comma-separated XKB layout codes such as us,es.",
                    "display_scale" => "Use output=scale, e.g. eDP-1=1.5 for 150%.",
                    _ => "Use the app name or its .desktop ID.",
                },
                json!({"setting":{"enum":[setting]},"value":{"type":"string"}}),
                json!(["setting", "value"]),
            ),
            Self::ReadSetting(setting) => (
                "Read the current desktop setting.",
                json!({"setting":{"enum":[setting]}}),
                json!(["setting"]),
            ),
            Self::Open => (
                "Open an installed app by name. To open a folder, set app to Files and folder to a user folder name or absolute path.",
                json!({"app":{"type":"string"},"folder":{"type":"string"}}),
                json!(["app"]),
            ),
            Self::Timer => (
                "Start a countdown and notify when it ends. Convert the duration to seconds.",
                json!({"seconds":{"type":"integer","minimum":1,"maximum":86400}}),
                json!(["seconds"]),
            ),
            Self::Status => (
                "Read current hardware status. CPU includes the busiest processes.",
                json!({"metric":{"enum":["battery","cpu","memory","network"]}}),
                json!(["metric"]),
            ),
            Self::Downloads => (
                "Find files in Downloads by local modified date and optional extension without the dot. Give the date as today, yesterday, or YYYY-MM-DD.",
                json!({"date":{"type":"string"},"extension":{"type":"string"}}),
                json!(["date"]),
            ),
        };
        json!({"type":"function","function":{"name":self.name(),"description":description,
            "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
    }

    pub(crate) fn classify(words: &[&str], first: Option<&str>) -> Option<Self> {
        let words = words
            .iter()
            .map(|word| word.trim_end_matches('.'))
            .collect::<Vec<_>>();
        let any = |choices: &[&str]| {
            words
                .iter()
                .any(|word| choices.contains(&word.trim_end_matches('.')))
        };
        if matches!(
            first,
            Some("explain" | "describe" | "translate" | "write" | "why" | "cómo" | "hogyan")
        ) {
            return None;
        }
        let change = matches!(
            first,
            Some(
                "turn"
                    | "switch"
                    | "set"
                    | "enable"
                    | "disable"
                    | "make"
                    | "dim"
                    | "brighten"
                    | "mute"
                    | "unmute"
                    | "lower"
                    | "raise"
                    | "reduce"
                    | "increase"
                    | "decrease"
                    | "adjust"
                    | "activa"
                    | "desactiva"
                    | "baja"
                    | "sube"
                    | "kapcsold"
                    | "állítsd"
                    | "csökkentsd"
                    | "növeld"
            )
        );
        if let Some(setting) = preference_target(&words) {
            if change {
                return Some(Self::Preference(setting));
            }
            if matches!(
                first,
                Some("what" | "which" | "check" | "show" | "tell" | "is" | "get")
            ) || words.windows(2).any(|pair| pair == ["how", "is"])
            {
                return Some(Self::ReadSetting(setting));
            }
        }
        if let Some((setting, _)) = setting_target(&words) {
            if change {
                return Some(Self::Setting);
            }
            if matches!(setting, "dark_mode" | "dnd")
                && matches!(first, Some("what" | "check" | "show" | "tell" | "is"))
            {
                return Some(Self::ReadSetting(setting));
            }
        }
        if (change || matches!(first, Some("start" | "begin" | "inicia" | "indíts")))
            && any(&["timer", "countdown", "temporizador", "időzítő", "időzítőt"])
        {
            return Some(Self::Timer);
        }
        let dated = any(&["today", "yesterday", "hoy", "ayer", "ma", "tegnap"])
            || words.windows(3).any(|date| {
                date.iter().zip([4, 2, 2]).all(|(part, len)| {
                    part.trim_end_matches('.').len() == len
                        && part
                            .trim_end_matches('.')
                            .bytes()
                            .all(|digit| digit.is_ascii_digit())
                })
            });
        if dated
            && any(&[
                "downloaded",
                "downloads",
                "descargué",
                "descargas",
                "letöltött",
                "letöltések",
            ])
            && !matches!(first, Some("open" | "launch" | "abre" | "nyisd"))
        {
            return Some(Self::Downloads);
        }
        if matches!(first, Some("open" | "launch" | "abre" | "nyisd"))
            && !words
                .iter()
                .any(|word| word.contains('.') && !word.starts_with('/'))
            && !any(&["attachment", "link", "website", "http", "https"])
        {
            return Some(Self::Open);
        }
        let asks_status = matches!(
            first,
            Some(
                "what"
                    | "how"
                    | "check"
                    | "show"
                    | "tell"
                    | "is"
                    | "am"
                    | "report"
                    | "cuánta"
                    | "cuánto"
                    | "milyen"
                    | "mennyi"
            )
        );
        let live = any(&[
            "my",
            "our",
            "this",
            "current",
            "currently",
            "now",
            "using",
            "usage",
            "left",
            "connected",
            "remaining",
            "me",
            "queda",
            "actual",
            "használat",
            "maradt",
            "állapota",
        ]) || matches!(first, Some("check" | "report"));
        let direct_question = first != Some("how")
            || words.windows(2).any(|pair| {
                matches!(
                    pair,
                    ["how", "much" | "many" | "long" | "is" | "full" | "busy"]
                )
            });
        if asks_status && live && direct_question && Self::metric(&words).is_some() {
            return Some(Self::Status);
        }
        None
    }

    pub(crate) fn grounded_arguments(self, prompt: &str) -> Option<Value> {
        let lower = prompt.to_lowercase();
        let words = lower
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        let negative_number = || {
            prompt
                .as_bytes()
                .windows(2)
                .any(|pair| pair[0] == b'-' && pair[1].is_ascii_digit())
        };
        match self {
            Self::Setting if !negative_number() => setting_arguments(&words),
            Self::ReadSetting(setting) => Some(json!({"setting":setting})),
            Self::Preference(setting) => preference_arguments(setting, &words),
            Self::Status => Some(json!({"metric": Self::metric(&words)?})),
            Self::Timer if !negative_number() => timer_arguments(&words),
            Self::Downloads => download_arguments(&words, &lower),
            _ => None,
        }
    }

    pub(crate) fn metric(words: &[&str]) -> Option<&'static str> {
        let choices: &[(&str, &[&str])] = &[
            (
                "battery",
                &["battery", "batería", "akkumulátor", "akku", "töltöttség"],
            ),
            ("cpu", &["cpu", "processor", "procesador", "processzor"]),
            ("memory", &["ram", "memory", "memoria", "memória"]),
            (
                "network",
                &["network", "wifi", "internet", "ethernet", "red", "hálózat"],
            ),
        ];
        let mut found = choices.iter().filter(|(_, variants)| {
            variants.iter().any(|variant| {
                words
                    .iter()
                    .any(|word| word.trim_end_matches('.') == *variant)
            })
        });
        let (metric, _) = found.next()?;
        found.next().is_none().then_some(*metric)
    }
}

fn preference_target(words: &[&str]) -> Option<&'static str> {
    let has = |word| words.contains(&word);
    let mut settings = [
        (has("night") && has("light"), "night_light"),
        (has("wallpaper"), "wallpaper"),
        (
            (has("shell") || has("desktop")) && has("style"),
            "shell_style",
        ),
        (
            has("power") && (has("mode") || has("profile")),
            "power_mode",
        ),
        (
            (has("display") || has("screen")) && (has("scale") || has("scaling")),
            "display_scale",
        ),
        (has("keyboard") && has("layout"), "keyboard_layout"),
        (
            has("notification") && (has("previews") || has("images")),
            "notification_previews",
        ),
        (has("default") && has("browser"), "default_browser"),
        (
            has("default") && (has("mail") || has("email")),
            "default_mail",
        ),
        (has("default") && has("files"), "default_files"),
    ]
    .into_iter()
    .filter_map(|(mentioned, setting)| mentioned.then_some(setting));
    let setting = settings.next()?;
    settings.next().is_none().then_some(setting)
}

fn preference_arguments(setting: &str, words: &[&str]) -> Option<Value> {
    let has = |word| words.contains(&word);
    let value = match setting {
        "night_light" | "notification_previews" => {
            match (has("on") || has("enable"), has("off") || has("disable")) {
                (true, false) => "on",
                (false, true) => "off",
                _ => return None,
            }
        }
        "shell_style" => match (has("islands"), has("frame")) {
            (true, false) => "islands",
            (false, true) => "frame",
            _ => return None,
        },
        _ => return None,
    };
    Some(json!({"setting":setting,"value":value}))
}

fn setting_target(words: &[&str]) -> Option<(&'static str, bool)> {
    let has = |choices: &[&str]| choices.iter().any(|word| words.contains(word));
    let dark = words.windows(2).any(|pair| {
        matches!(
            pair,
            ["dark", "mode"] | ["modo", "oscuro"] | ["sötét", "mód" | "módot"]
        )
    });
    let light = words.windows(2).any(|pair| {
        matches!(
            pair,
            ["light", "mode"] | ["modo", "claro"] | ["világos", "mód" | "módot"]
        )
    });
    let dnd = words
        .windows(3)
        .any(|part| part == ["do", "not", "disturb"])
        || words.windows(2).any(|part| part == ["no", "molestar"])
        || words.contains(&"dnd");
    let brightness = has(&[
        "brightness",
        "dim",
        "dimmer",
        "brighten",
        "brighter",
        "brillo",
        "fényerő",
    ]);
    let volume = has(&[
        "volume", "mute", "unmute", "louder", "quieter", "volumen", "hangerő",
    ]);
    let mut settings = [
        (dark || light, "dark_mode"),
        (dnd, "dnd"),
        (brightness, "brightness"),
        (volume, "volume"),
    ]
    .into_iter()
    .filter_map(|(mentioned, setting)| mentioned.then_some(setting));
    let setting = settings.next()?;
    (settings.next().is_none() && !(dark && light)).then_some((setting, light))
}

fn setting_arguments(words: &[&str]) -> Option<Value> {
    let (setting, light) = setting_target(words)?;
    let has = |choices: &[&str]| choices.iter().any(|word| words.contains(word));
    let value = if matches!(setting, "dark_mode" | "dnd") {
        let on = has(&["on", "enable", "activa", "be"]);
        let off = has(&["off", "disable", "desactiva", "ki"]);
        let on = match (on, off) {
            (true, false) => !light,
            (false, true) => light,
            _ => return None,
        };
        if on { "on" } else { "off" }
    } else if setting == "volume" && has(&["mute", "unmute"]) {
        match (words.contains(&"mute"), words.contains(&"unmute")) {
            (true, false) => "mute",
            (false, true) => "unmute",
            _ => return None,
        }
    } else {
        let numbers = words
            .iter()
            .filter(|word| word.bytes().all(|byte| byte.is_ascii_digit()));
        let mut numbers = numbers.copied();
        if let Some(number) = numbers.next() {
            if numbers.next().is_some() || words.contains(&"by") || number.parse::<u8>().ok()? > 100
            {
                return None;
            }
            return Some(json!({"setting":setting,"value":number}));
        }
        let down = has(&[
            "dim",
            "dimmer",
            "lower",
            "reduce",
            "decrease",
            "down",
            "quieter",
            "baja",
            "csökkentsd",
        ]);
        let up = has(&[
            "brighten", "brighter", "raise", "increase", "up", "louder", "sube", "növeld",
        ]);
        match (down, up) {
            (true, false) => "down",
            (false, true) => "up",
            _ => return None,
        }
    };
    Some(json!({"setting":setting,"value":value}))
}

fn timer_arguments(words: &[&str]) -> Option<Value> {
    if words
        .iter()
        .filter(|word| word.parse::<u32>().is_ok())
        .count()
        != 1
    {
        return None;
    }
    let seconds = words.windows(2).find_map(|pair| {
        let count = pair[0].parse::<u32>().ok()?;
        let factor = match pair[1] {
            "second" | "seconds" | "sec" | "segundo" | "segundos" | "másodperc" => 1,
            "minute" | "minutes" | "min" | "minuto" | "minutos" | "perc" | "perces" => 60,
            "hour" | "hours" | "hr" | "hora" | "horas" | "óra" | "órás" => 3600,
            _ => return None,
        };
        count.checked_mul(factor)
    })?;
    (1..=86_400)
        .contains(&seconds)
        .then(|| json!({"seconds":seconds}))
}

fn download_arguments(words: &[&str], prompt: &str) -> Option<Value> {
    let relative = ["today", "hoy", "ma", "yesterday", "ayer", "tegnap"];
    let mut dates = words
        .iter()
        .filter(|word| relative.contains(word))
        .map(|word| {
            if matches!(*word, "today" | "hoy" | "ma") {
                "today"
            } else {
                "yesterday"
            }
        })
        .chain(
            prompt
                .split(|ch: char| !ch.is_ascii_digit() && ch != '-')
                .filter(|part| {
                    let lengths = part.split('-').map(str::len).collect::<Vec<_>>();
                    lengths == [4, 2, 2]
                }),
        );
    let date = dates.next()?;
    if dates.any(|other| other != date) {
        return None;
    }
    let formats = [
        "pdf", "zip", "doc", "docx", "csv", "txt", "png", "jpg", "jpeg", "svg", "webp", "gif",
        "mp4", "mp3", "wav", "xlsx", "pptx", "epub", "odt",
    ];
    let mut extensions = formats.into_iter().filter(|format| words.contains(format));
    if let Some(extension) = extensions.next() {
        return extensions
            .next()
            .is_none()
            .then(|| json!({"date":date,"extension":extension}));
    }
    Some(json!({"date":date}))
}

pub(crate) fn cpu_answer(result: &Value) -> Option<String> {
    let usage = result["cpu_percent"].as_f64()?;
    let mut answer = format!("CPU usage is {usage:.0}% overall.");
    let busiest = result["busiest"].as_array()?;
    if busiest.is_empty() {
        answer.push_str(" No busy processes were detected in the sample.");
    } else {
        use std::fmt::Write as _;
        answer.push_str("\n\nBusiest processes");
        for process in busiest {
            let name = process["process"].as_str()?.replace('`', "'");
            let usage = process["cpu_percent_of_one_core"].as_f64()?;
            let _ = write!(answer, "\n- `{name}` at {usage:.0}% of one core");
        }
    }
    Some(answer)
}

pub(crate) fn download_answer(result: &Value) -> Option<String> {
    use std::fmt::Write as _;
    let date = result["date"].as_str()?;
    let files = result["files"].as_array()?;
    let mut answer = if files.is_empty() {
        format!("No matching files were found in Downloads for {date}.")
    } else {
        format!("Matching files in Downloads for {date}.")
    };
    for file in files {
        let name = file.as_str()?;
        let fence = "`".repeat(1 + name.bytes().filter(|byte| *byte == b'`').count());
        let _ = write!(answer, "\n- {fence} {name} {fence}");
    }
    if result["truncated"] == true {
        answer.push_str("\n\nMore files may match.");
    }
    Some(answer)
}

fn socket_path() -> Option<PathBuf> {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    runtime
        .is_absolute()
        .then(|| runtime.join("enzui-assistant-control.sock"))
}

pub(crate) fn legacy_enabled() -> bool {
    std::env::var_os("ENZIMOS_ASSISTANT_LEGACY_DEVICE").is_some_and(|value| value == "1")
}

pub(crate) fn available() -> bool {
    socket_path()
        .and_then(|path| std::fs::symlink_metadata(path).ok())
        .is_some_and(|meta| meta.file_type().is_socket())
}

pub(crate) async fn invoke(tool: Tool, arguments: Value) -> Result<Value> {
    let path = socket_path().ok_or_else(|| ToolError(desktop::UNAVAILABLE.into()))?;
    let mut frame =
        json!({"method":"device_tool","tool":tool.name(),"arguments":arguments}).to_string();
    if frame.len() >= FRAME {
        return fail("invalid tool arguments");
    }
    frame.push('\n');
    let exchange = async {
        let socket = UnixStream::connect(path).await.ok()?;
        let peer = desktop::ShellPeer::new(&socket)?;
        let mut socket = BufReader::new(socket);
        socket.get_mut().write_all(frame.as_bytes()).await.ok()?;
        let mut line = Vec::new();
        let len = read_line(&mut socket, &mut line, FRAME).await.ok()?;
        if len > FRAME || !line.ends_with(b"\n") || !peer.alive() {
            return None;
        }
        serde_json::from_slice::<Value>(&line).ok()
    };
    let result = tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| ToolError(desktop::UNAVAILABLE.into()))?;
    if result["ok"] == true {
        return result
            .get("result")
            .cloned()
            .ok_or_else(|| ToolError(desktop::INVALID.into()));
    }
    fail(match result["error"].as_str() {
        Some("locked") => LOCKED,
        Some("unavailable") => UNAVAILABLE,
        Some("limit") => TIMER_LIMIT,
        Some("app") => APP_UNAVAILABLE,
        Some("folder") => FOLDER_UNAVAILABLE,
        Some("launch") => APP_LAUNCH_FAILED,
        Some("restricted") => APP_RESTRICTED,
        Some("no_backlight") => NO_BACKLIGHT,
        _ => desktop::INVALID,
    })
}
