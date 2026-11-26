// SPDX-License-Identifier: MIT
use crate::{Result, ToolError, fail, runtime};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use image::{DynamicImage, ImageFormat, ImageReader, codecs::jpeg::JpegEncoder};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{BufReader, Cursor, Read},
    os::unix::fs::OpenOptionsExt,
    time::Duration,
};
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedObjectPath, OwnedValue, Value as Variant},
};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
static CAPTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PIXELS: u64 = 20_000_000;
const MAX_IMAGE_BYTES: usize = 600 * 1024;
pub(crate) const UNAVAILABLE: &str = "screenshot portal unavailable or capture declined";
pub(crate) const UNSUPPORTED: &str =
    "focused-window capture is unavailable; request a screen screenshot";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Access {
    #[default]
    Off,
    Text,
    Vision,
}

impl Access {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Text => "text",
            Self::Vision => "vision",
        }
    }

    pub(crate) fn parse(value: Option<&Value>) -> Result<Self> {
        match value {
            None => Ok(Self::Off),
            Some(value) => match value.as_str() {
                Some("off") => Ok(Self::Off),
                Some("text") => Ok(Self::Text),
                Some("vision") => Ok(Self::Vision),
                _ => fail("invalid screenshot access"),
            },
        }
    }
}

pub(crate) struct Capture {
    pub description: String,
    pub image_url: Option<String>,
}

fn unavailable() -> ToolError {
    ToolError(UNAVAILABLE.into())
}

pub(crate) async fn capture(access: Access, target: &str) -> Result<Capture> {
    if access == Access::Off {
        return fail(UNAVAILABLE);
    }
    let _capture = CAPTURE.lock().await;
    let uri = tokio::time::timeout(Duration::from_secs(60), screenshot_uri(target))
        .await
        .map_err(|_| unavailable())??;
    let path = url::Url::parse(&uri)
        .ok()
        .filter(|url| url.host_str().is_none() && url.query().is_none() && url.fragment().is_none())
        .and_then(|url| url.to_file_path().ok())
        .ok_or_else(unavailable)?;
    let target = target.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| unavailable())?;
        let metadata = file.metadata().map_err(|_| unavailable())?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return fail(UNAVAILABLE);
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| unavailable())?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return fail(UNAVAILABLE);
        }
        prepare(&bytes, access, &target)
    })
    .await
    .map_err(|_| unavailable())?
}

async fn screenshot_uri(target: &str) -> Result<String> {
    let connection = Connection::session().await.map_err(|_| unavailable())?;
    let screenshot = Proxy::new(
        &connection,
        PORTAL,
        PATH,
        "org.freedesktop.portal.Screenshot",
    )
    .await
    .map_err(|_| unavailable())?;
    let token = runtime::token();
    if token.is_empty() {
        return fail(UNAVAILABLE);
    }
    let sender = connection
        .unique_name()
        .ok_or_else(unavailable)?
        .trim_start_matches(':')
        .replace('.', "_");
    let request_path = format!("{PATH}/request/{sender}/{token}");
    let request = Proxy::new(
        &connection,
        PORTAL,
        request_path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await
    .map_err(|_| unavailable())?;
    let mut response = request
        .receive_signal("Response")
        .await
        .map_err(|_| unavailable())?;
    let mut options = HashMap::from([
        ("handle_token", Variant::from(token.as_str())),
        ("interactive", Variant::from(false)),
    ]);
    match target {
        "screen" => {}
        "focused_window" => {
            let targets = screenshot
                .get_property::<u32>("AvailableTargets")
                .await
                .unwrap_or(0);
            if targets & 8 == 0 {
                return fail(UNSUPPORTED);
            }
            options.insert("target", Variant::from(8_u32));
        }
        _ => return fail("invalid screenshot target"),
    }
    let returned: OwnedObjectPath = screenshot
        .call("Screenshot", &("", options))
        .await
        .map_err(|_| unavailable())?;
    if returned.as_str() != request_path {
        return fail(UNAVAILABLE);
    }
    let response = response.next().await.ok_or_else(unavailable)?;
    let (code, mut values): (u32, HashMap<String, OwnedValue>) =
        response.body().deserialize().map_err(|_| unavailable())?;
    if code != 0 {
        return fail(UNAVAILABLE);
    }
    values
        .remove("uri")
        .and_then(|value| String::try_from(value).ok())
        .ok_or_else(unavailable)
}

fn prepare(bytes: &[u8], access: Access, target: &str) -> Result<Capture> {
    let reader = || ImageReader::with_format(BufReader::new(Cursor::new(bytes)), ImageFormat::Png);
    let (width, height) = reader().into_dimensions().map_err(|_| unavailable())?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return fail(UNAVAILABLE);
    }
    let mut reader = reader();
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(MAX_PIXELS * 8);
    reader.limits(limits);
    let pixels = reader.decode().map_err(|_| unavailable())?;
    let sample = pixels
        .resize_exact(2, 2, image::imageops::FilterType::Triangle)
        .to_rgb8();
    let colors = sample
        .pixels()
        .map(|color| format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2]))
        .collect::<Vec<_>>()
        .join(", ");
    let description = format!(
        "Captured {target} at {width} × {height} pixels. Approximate colors of the four quadrants, in reading order: {colors}."
    );
    let image_url = if access == Access::Vision {
        let preview = if width.max(height) > 1024 {
            pixels.thumbnail(1024, 1024)
        } else {
            pixels
        };
        let preview = DynamicImage::ImageRgb8(preview.to_rgb8());
        let mut data = Vec::new();
        JpegEncoder::new_with_quality(&mut data, 78)
            .encode_image(&preview)
            .map_err(|_| unavailable())?;
        if data.len() > MAX_IMAGE_BYTES {
            return fail(UNAVAILABLE);
        }
        Some(format!("data:image/jpeg;base64,{}", STANDARD.encode(data)))
    } else {
        None
    };
    Ok(Capture {
        description,
        image_url,
    })
}

pub(crate) fn message(call_id: &str, url: String) -> Value {
    json!({"role": "user", "content": [
        {"type": "text", "text": format!("Visual result of screenshot tool call {call_id}.")},
        {"type": "image_url", "image_url": {"url": url}}
    ]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_pixels_are_only_attached_for_vision() {
        let png =
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 3, image::Rgb([200, 40, 20])));
        let mut bytes = Cursor::new(Vec::new());
        png.write_to(&mut bytes, ImageFormat::Png).unwrap();

        let text = prepare(bytes.get_ref(), Access::Text, "screen").unwrap();
        assert!(text.description.contains("4 × 3"));
        assert!(text.description.contains("#c82814"));
        assert!(text.image_url.is_none());

        let capture = prepare(bytes.get_ref(), Access::Vision, "screen").unwrap();
        let url = capture.image_url.unwrap();
        let jpeg = STANDARD
            .decode(url.strip_prefix("data:image/jpeg;base64,").unwrap())
            .unwrap();
        assert_eq!(image::load_from_memory(&jpeg).unwrap().width(), 4);
        let message = message("shot-42", url);
        assert!(
            message["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("shot-42")
        );
        assert!(prepare(&jpeg, Access::Vision, "screen").is_err());
    }
}
