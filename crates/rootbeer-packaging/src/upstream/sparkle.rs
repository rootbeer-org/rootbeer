use std::collections::BTreeMap;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// One appcast release: where it downloads from and the EdDSA signature over those bytes.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Item {
    pub url: String,
    pub signature: Option<String>,
}

#[derive(Default)]
struct Fields {
    url: Option<String>,
    signature: Option<String>,
    os: Option<String>,
    version: Option<String>,
    short_version: Option<String>,
    channel: Option<String>,
}

impl Fields {
    /// Reads the item's full download; attributes lose to the equivalent elements.
    fn enclosure(&mut self, element: &BytesStart) -> Result<(), String> {
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|e| format!("invalid appcast: {e}"))?;
            let value = attribute
                .unescape_value()
                .map_err(|e| format!("invalid appcast: {e}"))?
                .trim()
                .to_string();
            match attribute.key.local_name().as_ref() {
                b"url" => self.url = Some(value),
                b"edSignature" => self.signature = Some(value),
                b"os" => self.os = Some(value),
                b"version" => self.version = self.version.take().or(Some(value)),
                b"shortVersionString" => {
                    self.short_version = self.short_version.take().or(Some(value));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn field(&mut self, name: &[u8]) -> Option<&mut Option<String>> {
        match name {
            b"version" => Some(&mut self.version),
            b"shortVersionString" => Some(&mut self.short_version),
            b"channel" => Some(&mut self.channel),
            _ => None,
        }
    }
}

/// Maps each version an appcast offers to its download, following the default channel
/// and `channel`. Delta updates and other operating systems are skipped.
pub(super) fn parse(xml: &str, channel: Option<&str>) -> Result<BTreeMap<String, Item>, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut items = BTreeMap::new();
    let mut item: Option<Fields> = None;
    let mut field: Option<Vec<u8>> = None;
    let mut deltas = 0usize;
    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("invalid appcast: {e}"))?;
        match event {
            Event::Start(element) => match element.local_name().as_ref() {
                b"item" => item = Some(Fields::default()),
                b"deltas" => deltas += 1,
                b"enclosure" if deltas == 0 => {
                    if let Some(item) = item.as_mut() {
                        item.enclosure(&element)?;
                    }
                }
                name if deltas == 0 => field = Some(name.to_vec()),
                _ => {}
            },
            Event::Empty(element) if deltas == 0 => {
                if let (Some(item), b"enclosure") = (item.as_mut(), element.local_name().as_ref()) {
                    item.enclosure(&element)?;
                }
            }
            Event::Text(text) => {
                let slot = item
                    .as_mut()
                    .zip(field.as_deref())
                    .and_then(|(item, name)| item.field(name));
                if let Some(slot) = slot {
                    let value = text.xml_content().map_err(|e| e.to_string())?;
                    *slot = Some(value.trim().to_string());
                }
            }
            Event::End(element) => match element.local_name().as_ref() {
                b"item" => {
                    if let Some(fields) = item.take() {
                        insert(&mut items, fields, channel)?;
                    }
                }
                b"deltas" => deltas = deltas.saturating_sub(1),
                _ => field = None,
            },
            Event::Eof => return Ok(items),
            _ => {}
        }
    }
}

fn insert(
    items: &mut BTreeMap<String, Item>,
    fields: Fields,
    channel: Option<&str>,
) -> Result<(), String> {
    let Some(url) = fields.url.filter(|url| url.starts_with("https://")) else {
        return Ok(());
    };
    if fields
        .channel
        .as_deref()
        .is_some_and(|listed| Some(listed) != channel)
    {
        return Ok(());
    }
    if fields.os.as_deref().is_some_and(|os| os != "macos") {
        return Ok(());
    }
    let Some(version) = fields.short_version.or(fields.version) else {
        return Ok(());
    };

    let item = Item {
        url,
        signature: fields.signature,
    };
    match items.get(&version) {
        Some(known) if known.url != item.url => Err(format!(
            "appcast offers version {version} as both {} and {}",
            known.url, item.url
        )),
        Some(_) => Ok(()),
        None => {
            items.insert(version, item);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>App</title>
    <item>
      <title>2.1</title>
      <description><![CDATA[<enclosure url="https://evil.example/app.dmg"/>]]></description>
      <sparkle:version>210</sparkle:version>
      <sparkle:shortVersionString>2.1</sparkle:shortVersionString>
      <enclosure url="https://example.com/App_2.1_210.dmg" length="10"
                 sparkle:edSignature="c2ln" type="application/octet-stream"/>
      <sparkle:deltas>
        <enclosure url="https://example.com/App_2.1-2.0.delta" sparkle:deltaFrom="200"
                   sparkle:version="210" sparkle:shortVersionString="2.1"/>
      </sparkle:deltas>
    </item>
    <item>
      <title>2.2 beta</title>
      <sparkle:channel>beta</sparkle:channel>
      <enclosure url="https://example.com/App_2.2.dmg" sparkle:version="220"
                 sparkle:shortVersionString="2.2"/>
    </item>
    <item>
      <title>2.0</title>
      <enclosure url="https://example.com/App_2.0.dmg?a=1&amp;b=2" sparkle:version="200"
                 sparkle:shortVersionString="2.0" sparkle:edSignature="c2ln"/>
    </item>
    <item>
      <title>Windows</title>
      <enclosure url="https://example.com/App_3.0.exe" sparkle:os="windows"
                 sparkle:shortVersionString="3.0"/>
    </item>
    <item>
      <title>Insecure</title>
      <enclosure url="http://example.com/App_1.0.dmg" sparkle:shortVersionString="1.0"/>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn reads_full_downloads_on_the_followed_channels() {
        let items = parse(FEED, None).unwrap();
        assert_eq!(items.keys().collect::<Vec<_>>(), ["2.0", "2.1"]);
        assert_eq!(items["2.1"].url, "https://example.com/App_2.1_210.dmg");
        assert_eq!(items["2.1"].signature.as_deref(), Some("c2ln"));
        assert_eq!(items["2.0"].url, "https://example.com/App_2.0.dmg?a=1&b=2");

        let beta = parse(FEED, Some("beta")).unwrap();
        assert_eq!(beta["2.2"].url, "https://example.com/App_2.2.dmg");
    }

    #[test]
    fn one_version_with_two_downloads_is_rejected() {
        let feed = r#"<rss xmlns:sparkle="s"><channel>
            <item><enclosure url="https://example.com/a.dmg" sparkle:shortVersionString="1"/></item>
            <item><enclosure url="https://example.com/b.dmg" sparkle:shortVersionString="1"/></item>
        </channel></rss>"#;
        assert!(parse(feed, None).unwrap_err().contains("both"));
    }
}
