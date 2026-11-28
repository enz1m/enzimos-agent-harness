// SPDX-License-Identifier: MIT
use serde_json::Value;
use url::{Host, Url};

#[derive(Clone, Copy)]
pub(super) enum Interest {
    Reservations,
    Vegan,
    Vegetarian,
    Cuisine,
}

impl Interest {
    pub(super) fn from_prompt(prompt: &str) -> Option<Self> {
        let mut interest = None;
        for word in prompt.split(|c: char| !c.is_ascii_alphabetic()) {
            match word.to_ascii_lowercase().as_str() {
                "vegan" => return Some(Self::Vegan),
                "vegetarian" => interest = Some(Self::Vegetarian),
                "reservation" | "reservations" | "booking" | "bookings" | "reserve"
                    if !matches!(interest, Some(Self::Vegetarian)) =>
                {
                    interest = Some(Self::Reservations);
                }
                "restaurant" | "restaurants" | "cuisine" if interest.is_none() => {
                    interest = Some(Self::Cuisine);
                }
                _ => {}
            }
        }
        interest
    }
}

pub(crate) struct Finding {
    source: Url,
    sentence: String,
}

impl Finding {
    pub(crate) fn sentence(&self) -> &str {
        &self.sentence
    }

    pub(crate) fn during_followup(&self, next: &Url) -> Option<&str> {
        (public_https(next)
            && super::validate(next).is_ok()
            && next.host_str().map(site_host) != self.source.host_str().map(site_host))
        .then_some(self.sentence())
    }
}

fn site_host(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

pub(super) fn public_https(url: &Url) -> bool {
    url.scheme() == "https"
        && matches!(url.host(), Some(Host::Domain(_)))
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

const MAX_SCRIPT: usize = 24 * 1024;

#[derive(Default)]
pub(super) struct Metadata {
    script: Option<String>,
    record: Option<Value>,
    ambiguous: bool,
}

impl Metadata {
    pub(super) fn start_script(&mut self) {
        self.script = Some(String::new());
    }

    pub(super) fn append_script(&mut self, text: &str) {
        if let Some(script) = &mut self.script {
            if script.len() + text.len() > MAX_SCRIPT {
                self.script = None;
                self.ambiguous = true;
            } else {
                script.push_str(text);
            }
        }
    }

    pub(super) fn finish_script(&mut self) {
        if let Some(script) = self.script.take()
            && let Ok(value) = serde_json::from_str::<Value>(&script)
        {
            self.add(&value);
        }
    }

    fn add(&mut self, value: &Value) {
        let entries = value
            .as_array()
            .map_or_else(|| std::slice::from_ref(value), Vec::as_slice);
        for entry in entries {
            if !schema_org(&entry["@context"]) {
                continue;
            }
            let graph = entry["@graph"].as_array().map_or(&[][..], Vec::as_slice);
            for record in std::iter::once(entry).chain(graph) {
                if !record.is_object()
                    || (record.get("@context").is_some() && !schema_org(&record["@context"]))
                    || !is_restaurant(&record["@type"])
                {
                    continue;
                }
                if self.record.is_some() {
                    self.ambiguous = true;
                    return;
                }
                self.record = Some(record.clone());
            }
        }
    }

    pub(super) fn finding(self, source: Url, visible: &str, interest: Interest) -> Option<Finding> {
        if self.ambiguous {
            return None;
        }
        let record = self.record?;
        let declared = Url::parse(record["url"].as_str()?).ok()?;
        if declared != source {
            return None;
        }
        // Bind the spoken name to both the fetched origin and visible page text.
        let name = record["name"].as_str()?.replace('’', "'");
        let host_label = site_host(source.host_str()?).split('.').next()?;
        let visible = visible.replace('’', "'");
        if !self_host_name(&name, host_label)
            || !visible.match_indices(&name).any(|(start, _)| {
                visible
                    .get(..start)
                    .and_then(|before| before.chars().next_back())
                    .is_none_or(|ch| !ch.is_alphanumeric())
                    && visible
                        .get(start + name.len()..)
                        .and_then(|after| after.chars().next())
                        .is_none_or(|ch| !ch.is_alphanumeric())
            })
        {
            return None;
        }
        let cuisine = &record["servesCuisine"];
        let claim = match interest {
            Interest::Reservations => match &record["acceptsReservations"] {
                Value::Bool(true) => "takes reservations".into(),
                Value::Bool(false) => "doesn't take reservations".into(),
                Value::String(value) if value.eq_ignore_ascii_case("yes") => {
                    "takes reservations".into()
                }
                Value::String(value) if value.eq_ignore_ascii_case("no") => {
                    "doesn't take reservations".into()
                }
                _ => return None,
            },
            Interest::Vegan if lists_cuisine(cuisine, "vegan") => "serves vegan food".into(),
            Interest::Vegetarian if lists_cuisine(cuisine, "vegetarian") => {
                "serves vegetarian food".into()
            }
            Interest::Vegetarian if lists_cuisine(cuisine, "vegan") => "serves vegan food".into(),
            Interest::Cuisine => cuisine_claim(cuisine)?,
            _ => return None,
        };
        Some(Finding {
            source,
            sentence: format!("The website for {name} says it {claim}."),
        })
    }
}

fn schema_org(context: &Value) -> bool {
    matches!(
        context.as_str(),
        Some(
            "https://schema.org"
                | "https://schema.org/"
                | "http://schema.org"
                | "http://schema.org/"
        )
    )
}

fn is_restaurant(kind: &Value) -> bool {
    let matches = |value: &Value| {
        matches!(
            value.as_str(),
            Some("Restaurant" | "FoodEstablishment" | "CafeOrCoffeeShop")
        )
    };
    matches(kind)
        || kind
            .as_array()
            .is_some_and(|types| types.iter().any(matches))
}

fn self_host_name(name: &str, host_label: &str) -> bool {
    name.len() <= 40
        && (1..=4).contains(&name.split(' ').count())
        && name.split(' ').all(|word| {
            (2..=16).contains(&word.len())
                && word
                    .split(['\'', '-'])
                    .all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_alphabetic()))
        })
        && name
            .bytes()
            .filter(u8::is_ascii_alphabetic)
            .map(|c| c.to_ascii_lowercase())
            .eq(host_label.bytes().filter(|c| *c != b'-'))
}

fn lists_cuisine(value: &Value, cuisine: &str) -> bool {
    let values = value
        .as_array()
        .map_or_else(|| std::slice::from_ref(value), Vec::as_slice);
    values.iter().any(|value| {
        value
            .as_str()
            .is_some_and(|text| text.eq_ignore_ascii_case(cuisine))
    })
}

fn cuisine_claim(value: &Value) -> Option<String> {
    if lists_cuisine(value, "vegan") {
        return Some("serves vegan food".into());
    }
    if lists_cuisine(value, "vegetarian") {
        return Some("serves vegetarian food".into());
    }
    if lists_cuisine(value, "pizza") {
        return Some("serves pizza".into());
    }
    let mut cuisines = [
        "African",
        "Indian",
        "Italian",
        "Japanese",
        "Mexican",
        "Portuguese",
        "Thai",
    ]
    .into_iter()
    .filter(|cuisine| lists_cuisine(value, cuisine));
    let first = cuisines.next()?;
    match cuisines.next() {
        Some(second) => Some(format!("lists {first} and {second} cuisine")),
        None => Some(format!("lists {first} cuisine")),
    }
}
