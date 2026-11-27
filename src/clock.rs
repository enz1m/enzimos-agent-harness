// SPDX-License-Identifier: MIT
use chrono::Local;
use serde_json::{Value, json};

pub(crate) fn now() -> Value {
    let local = Local::now();
    let timezone = std::env::var("TZ")
        .ok()
        .filter(|tz| !tz.is_empty())
        .or_else(|| iana_time_zone::get_timezone().ok())
        .unwrap_or_else(|| local.format("%:z").to_string());
    json!({
        "date": local.format("%Y-%m-%d").to_string(),
        "time": local.format("%H:%M:%S").to_string(),
        "weekday": local.format("%A").to_string(),
        "timezone": timezone,
        "utc_offset": local.format("%:z").to_string(),
    })
}
