// SPDX-License-Identifier: MIT
use crate::{Result, ToolError, fail};
use html5ever::tokenizer::{
    BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, states::RawKind,
};
use serde_json::{Value, json};
use std::{cell::RefCell, net::IpAddr, time::Duration};
use url::{Host, Url};

const MAX_BODY: usize = 512 * 1024;
const PAGE_BYTES: usize = 2400;
pub(crate) const REFUSED: &str = "Web fetch requires a public HTTP or HTTPS address.";
pub(crate) const UNAVAILABLE: &str = "Web fetch failed or exceeded its time limit.";
pub(crate) const UNSUPPORTED: &str = "Web fetch supports HTML, plain text, and JSON up to 512 KiB.";

fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !matches!(
                (a, b, c),
                (0 | 10 | 127 | 224..=255, _, _)
                    | (100, 64..=127, _)
                    | (169, 254, _)
                    | (172, 16..=31, _)
                    | (192, 168, _)
                    | (192, 0, 0 | 2)
                    | (192, 88, 99)
                    | (198, 18 | 19, _)
                    | (198, 51, 100)
                    | (203, 0, 113)
            )
        }
        IpAddr::V6(ip) => {
            let [a, b, ..] = ip.segments();
            (0x2000..0x4000).contains(&a)
                && !matches!((a, b), (0x2001, 0..=0x1ff | 0xdb8) | (0x2002 | 0x3fff, _))
        }
    }
}

fn validate(url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.as_str().len() > 4096
    {
        return fail(REFUSED);
    }
    match url.host() {
        Some(Host::Ipv4(ip)) if public(ip.into()) => Ok(()),
        Some(Host::Ipv6(ip)) if public(ip.into()) => Ok(()),
        Some(Host::Domain(domain))
            if domain.trim_end_matches('.').contains('.')
                && ![".localhost", ".local", ".internal", ".home.arpa"]
                    .iter()
                    .any(|suffix| domain.trim_end_matches('.').ends_with(suffix)) =>
        {
            Ok(())
        }
        _ => fail(REFUSED),
    }
}

async fn get(mut url: Url, offset: usize) -> Result<Value> {
    let unavailable = || ToolError(UNAVAILABLE.into());
    for _ in 0..=4 {
        validate(&url)?;
        url.set_fragment(None);
        let host = url.host_str().ok_or_else(unavailable)?;
        let port = url.port_or_known_default().ok_or_else(unavailable)?;
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("enzimOS Assistant");
        if matches!(url.host(), Some(Host::Domain(_))) {
            let addresses = tokio::net::lookup_host((host, port))
                .await
                .map_err(|_| unavailable())?
                .collect::<Vec<_>>();
            if addresses.is_empty() || addresses.iter().any(|address| !public(address.ip())) {
                return fail(REFUSED);
            }
            // Connect only to the addresses this check approved, even if DNS changes.
            builder = builder.resolve_to_addrs(host, &addresses);
        }
        let mut response = builder
            .build()
            .map_err(|_| unavailable())?
            .get(url.clone())
            .header("Accept", "text/html,text/plain,application/json")
            .send()
            .await
            .map_err(|_| unavailable())?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(unavailable)?;
            url = url.join(location).map_err(|_| unavailable())?;
            continue;
        }
        if !status.is_success() {
            return Ok(json!({"url": url.as_str(), "http_status": status.as_u16()}));
        }
        let media = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !matches!(
            media.as_str(),
            "text/html" | "text/plain" | "application/json"
        ) || response
            .content_length()
            .is_some_and(|bytes| bytes > MAX_BODY as u64)
        {
            return fail(UNSUPPORTED);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if bytes.len() + chunk.len() > MAX_BODY {
                return fail(UNSUPPORTED);
            }
            bytes.extend_from_slice(&chunk);
        }
        let raw = String::from_utf8_lossy(&bytes);
        let text = if media == "text/html" {
            html_text(&raw)
        } else {
            raw.into_owned()
        };
        let start = text.ceil_char_boundary(offset.min(text.len()));
        let end = text.floor_char_boundary((start + PAGE_BYTES).min(text.len()));
        return Ok(json!({"url": url.as_str(), "http_status": status.as_u16(),
            "text": text.get(start..end).unwrap_or_default(), "next_offset": (end < text.len()).then_some(end)}));
    }
    fail(UNAVAILABLE)
}

pub(crate) async fn fetch(url: &str, offset: usize) -> Result<Value> {
    let url = Url::parse(url).map_err(|_| ToolError(REFUSED.into()))?;
    tokio::time::timeout(Duration::from_secs(12), get(url, offset))
        .await
        .unwrap_or_else(|_| fail(UNAVAILABLE))
}

#[derive(Default)]
struct Text {
    output: String,
    hidden: Vec<html5ever::LocalName>,
}

#[derive(Default)]
struct Extractor(RefCell<Text>);

impl TokenSink for Extractor {
    type Handle = ();

    fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<()> {
        let mut text = self.0.borrow_mut();
        match token {
            Token::TagToken(tag) => {
                let name = tag.name.as_ref();
                let raw = match name {
                    "script" => Some(RawKind::ScriptData),
                    "style" => Some(RawKind::Rawtext),
                    _ => None,
                };
                if tag.kind == TagKind::StartTag && (raw.is_some() || matches!(name, "template")) {
                    text.hidden.push(tag.name);
                    if let Some(raw) = raw {
                        return TokenSinkResult::RawData(raw);
                    }
                } else if tag.kind == TagKind::EndTag && text.hidden.last() == Some(&tag.name) {
                    text.hidden.pop();
                } else if text.hidden.is_empty()
                    && matches!(
                        name,
                        "p" | "br"
                            | "div"
                            | "li"
                            | "tr"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "section"
                            | "article"
                            | "title"
                    )
                {
                    text.output.push('\n');
                }
            }
            Token::CharacterTokens(chars) if text.hidden.is_empty() => text.output.push_str(&chars),
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

fn html_text(html: &str) -> String {
    let input = BufferQueue::default();
    input.push_back(html.into());
    let tokenizer = Tokenizer::new(Extractor::default(), Default::default());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer
        .sink
        .0
        .into_inner()
        .output
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses_and_non_web_urls_are_refused() {
        for address in [
            "file:///etc/passwd",
            "http://localhost/",
            "http://127.1/",
            "http://169.254.169.254/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fc00::1]/",
            "http://[64:ff9b::7f00:1]/",
            "http://100.64.0.1/",
            "https://user@example.com/",
        ] {
            assert!(
                validate(&Url::parse(address).unwrap()).is_err(),
                "{address}"
            );
        }
        assert!(validate(&Url::parse("https://example.com/").unwrap()).is_ok());
        assert!(validate(&Url::parse("https://[2606:4700::1111]").unwrap()).is_ok());
    }

    #[test]
    fn html_text_skips_script_style_and_template_contents() {
        assert_eq!(
            html_text(
                "<title>Example</title><script>'<style>'</script><p>Hi &amp; bye.</p><template>inert</template><style>p{}</style>"
            ),
            "Example\nHi & bye."
        );
    }
}
