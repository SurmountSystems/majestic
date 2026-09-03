//! Stream a JSON array of story-card objects into conversation records.
//!
//! Each object is one card: `title`, `type`, `keys` (comma string or array),
//! `value` (body), plus optional leftover keys. Explicit ingest only. Home scan
//! does not treat a random `*.json` array as story cards. Do not parse the
//! whole array as one `serde_json::Value`.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Deserializer;
use serde::de::{self, DeserializeSeed, SeqAccess, Visitor};

use crate::Error;
use crate::schema::{Conversation, ConversationItem, ExtraMap, JsonAtom, Response, ResponseItem};

/// `*.json` whose first non-space byte is `[`, and not a named export file.
pub(crate) fn looks_like_story_card_file(path: &Path) -> Result<bool, Error> {
    if !path.is_file() || !is_json_file(path) || is_reserved_json_name(path) {
        return Ok(false);
    }
    json_file_starts_with_array(path)
}

fn is_json_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

fn is_reserved_json_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name.eq_ignore_ascii_case("prod-grok-backend.json")
        || name.eq_ignore_ascii_case("prod-mc-auth-mgmt-api.json")
        || name.eq_ignore_ascii_case("prod-mc-billing.json")
        || name.eq_ignore_ascii_case("result.json")
        || name.eq_ignore_ascii_case("user.json")
        || crate::ingest_chatgpt::is_conversations_json_name(name)
}

fn json_file_starts_with_array(path: &Path) -> Result<bool, Error> {
    let mut file = File::open(path)?;
    let mut buf = [0u8; 64];
    let n = file.read(&mut buf)?;
    let mut bytes = &buf[..n];
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        bytes = &bytes[3..];
    }
    let trimmed = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map(|index| &bytes[index..])
        .unwrap_or(&[]);
    Ok(trimmed.first() == Some(&b'['))
}

/// Stream each story-card object as a conversation.
pub(crate) fn for_each_story_card(
    path: &Path,
    each: impl FnMut(ConversationItem) -> Result<(), Error>,
) -> Result<(), Error> {
    let file = File::open(path)?;
    for_each_story_card_reader(file, path, each)
}

fn for_each_story_card_reader<R: Read>(
    reader: R,
    label: &Path,
    mut each: impl FnMut(ConversationItem) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut de = serde_json::Deserializer::from_reader(std::io::BufReader::with_capacity(
        256 * 1024,
        reader,
    ));
    let count = CardArrayVisitor { each: &mut each }
        .deserialize(&mut de)
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    de.end()
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    if count == 0 {
        return Err(Error::ingest(format!(
            "{} is a JSON array but has no story cards (need title plus keys or value)",
            label.display()
        )));
    }
    Ok(())
}

struct CardArrayVisitor<'a> {
    each: &'a mut dyn FnMut(ConversationItem) -> Result<(), Error>,
}

impl<'de> DeserializeSeed<'de> for CardArrayVisitor<'_> {
    type Value = usize;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for CardArrayVisitor<'_> {
    type Value = usize;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON array of story-card objects")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut count = 0usize;
        while let Some(atom) = seq.next_element::<JsonAtom>()? {
            if let Some(item) = story_card_item(atom) {
                (self.each)(item).map_err(de::Error::custom)?;
                count += 1;
            }
        }
        Ok(count)
    }
}

fn story_card_item(atom: JsonAtom) -> Option<ConversationItem> {
    let JsonAtom::Object(mut extra) = atom else {
        return None;
    };
    let title = extra
        .remove("title")
        .as_ref()
        .and_then(atom_plain_string)
        .filter(|title| !title.is_empty())?;
    let keys = extra.get("keys").cloned();
    let keys_text = keys.as_ref().and_then(keys_as_text);
    let value = extra.remove("value");
    let has_body = keys_text.as_ref().is_some_and(|text| !text.is_empty())
        || value.as_ref().is_some_and(atom_is_body);
    if !has_body {
        return None;
    }
    let id = title.clone();
    let mut responses = Vec::new();
    if let Some(message) = value.filter(atom_is_body) {
        responses.push(ResponseItem {
            response: Response {
                conversation_id: Some(id.clone()),
                message: Some(message),
                extra: ExtraMap::new(),
                ..Default::default()
            },
            share_link: None,
            extra: ExtraMap::new(),
        });
    }
    Some(ConversationItem {
        conversation: Conversation {
            id: Some(id),
            title: Some(title),
            summary: keys_text,
            extra,
            ..Default::default()
        },
        responses,
        extra: ExtraMap::new(),
    })
}

fn atom_is_body(atom: &JsonAtom) -> bool {
    match atom {
        JsonAtom::Null => false,
        JsonAtom::String(text) => !text.is_empty(),
        JsonAtom::Array(items) => !items.is_empty(),
        JsonAtom::Object(map) => !map.is_empty(),
        JsonAtom::Bool(_) | JsonAtom::I64(_) | JsonAtom::U64(_) | JsonAtom::F64(_) => true,
    }
}

fn keys_as_text(atom: &JsonAtom) -> Option<String> {
    match atom {
        JsonAtom::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        JsonAtom::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(atom_plain_string).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(", "))
            }
        }
        _ => atom_plain_string(atom),
    }
}

fn atom_plain_string(atom: &JsonAtom) -> Option<String> {
    match atom {
        JsonAtom::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        JsonAtom::I64(value) => Some(value.to_string()),
        JsonAtom::U64(value) => Some(value.to_string()),
        JsonAtom::F64(value) => Some(value.to_string()),
        JsonAtom::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}
