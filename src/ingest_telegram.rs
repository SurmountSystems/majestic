//! Stream Telegram Desktop `result.json` into conversation records.
//!
//! Single-chat exports have top-level `id`, `messages`, `name`, and `type`.
//! Full-account exports have `personal_information` and `chats.list`.
//! Messages are streamed one object at a time. Unknown keys stay leftover.
//! Do not parse the file as a `serde_json::Value` document.

use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::Error;
use crate::schema::{
    Conversation, ConversationItem, ExtraMap, JsonAtom, Response, ResponseItem, Timestamp,
};

pub(crate) const RESULT_FILE_NAME: &str = "result.json";

/// Account stem from `personal_information.username` when present and non-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TelegramMeta {
    pub username: Option<String>,
}

pub(crate) fn is_result_json_name(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some(RESULT_FILE_NAME)
}

/// Telegram Desktop writes HTML/JSON dumps in folders named `ChatExport_*`.
pub(crate) fn is_chatexport_dir_name(name: &str) -> bool {
    name.starts_with("ChatExport_")
}

/// `result.json` whose parent directory name starts with `ChatExport_`.
pub(crate) fn is_chatexport_result_json(path: &Path) -> bool {
    is_result_json_name(path)
        && path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            .is_some_and(is_chatexport_dir_name)
}

/// Peek top-level keys. `None` when the file is missing or not a Telegram export.
pub(crate) fn telegram_meta(path: &Path) -> Result<Option<TelegramMeta>, Error> {
    if !is_result_json_name(path) {
        return Ok(None);
    }
    match peek_telegram_shape(path) {
        Ok(meta) => Ok(meta),
        Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn peek_telegram_shape(path: &Path) -> Result<Option<TelegramMeta>, Error> {
    let file = File::open(path)?;
    telegram_meta_from_reader(file, path)
}

/// Peek a `result.json` reader (zip entry or file). Does not dump the document.
pub(crate) fn telegram_meta_from_reader<R: std::io::Read>(
    reader: R,
    label: &Path,
) -> Result<Option<TelegramMeta>, Error> {
    let mut de =
        serde_json::Deserializer::from_reader(BufReader::with_capacity(256 * 1024, reader));
    let peek = PeekVisitor
        .deserialize(&mut de)
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    de.end()
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    Ok(peek)
}

struct PeekVisitor;

#[derive(Default)]
struct PeekKeys {
    has_id: bool,
    has_messages: bool,
    has_name: bool,
    has_type: bool,
    has_personal_information: bool,
    has_chats: bool,
    has_left_chats: bool,
    username: Option<String>,
}

impl PeekKeys {
    fn is_telegram(&self) -> bool {
        (self.has_id && self.has_messages && self.has_name && self.has_type)
            || (self.has_personal_information && self.has_chats)
            || (self.has_personal_information && self.has_left_chats)
    }
}

impl<'de> DeserializeSeed<'de> for PeekVisitor {
    type Value = Option<TelegramMeta>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for PeekVisitor {
    type Value = Option<TelegramMeta>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a Telegram Desktop result.json object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut keys = PeekKeys::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "id" => {
                    keys.has_id = true;
                    let _ = map.next_value::<IgnoredAny>()?;
                }
                "messages" => {
                    keys.has_messages = true;
                    let _ = map.next_value::<IgnoredAny>()?;
                }
                "name" => {
                    keys.has_name = true;
                    let _ = map.next_value::<IgnoredAny>()?;
                }
                "type" => {
                    keys.has_type = true;
                    let _ = map.next_value::<IgnoredAny>()?;
                }
                "personal_information" => {
                    keys.has_personal_information = true;
                    let info = map.next_value::<PersonalInformationPeek>()?;
                    keys.username = nonempty_username(info.username);
                }
                "chats" => {
                    keys.has_chats = true;
                    let _ = map.next_value::<IgnoredAny>()?;
                }
                "left_chats" => {
                    keys.has_left_chats = true;
                    let _ = map.next_value::<IgnoredAny>()?;
                }
                _ => {
                    let _ = map.next_value::<IgnoredAny>()?;
                }
            }
        }
        if keys.is_telegram() {
            Ok(Some(TelegramMeta {
                username: keys.username,
            }))
        } else {
            Ok(None)
        }
    }
}

#[derive(Deserialize)]
struct PersonalInformationPeek {
    #[serde(default)]
    username: Option<String>,
}

fn nonempty_username(username: Option<String>) -> Option<String> {
    username.filter(|name| !name.is_empty())
}

/// Stream each chat as a conversation. Account-level leftover is the return map.
pub(crate) fn for_each_telegram_conversation(
    path: &Path,
    each: impl FnMut(ConversationItem) -> Result<(), Error>,
) -> Result<ExtraMap, Error> {
    let file = File::open(path)?;
    for_each_telegram_conversation_reader(file, path, each)
}

/// Stream Telegram JSON from any `Read` (file or zip entry).
pub(crate) fn for_each_telegram_conversation_reader<R: std::io::Read>(
    reader: R,
    label: &Path,
    mut each: impl FnMut(ConversationItem) -> Result<(), Error>,
) -> Result<ExtraMap, Error> {
    let mut de =
        serde_json::Deserializer::from_reader(BufReader::with_capacity(256 * 1024, reader));
    let extra = RootVisitor { each: &mut each }
        .deserialize(&mut de)
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    de.end()
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    Ok(extra)
}

struct RootVisitor<'a> {
    each: &'a mut dyn FnMut(ConversationItem) -> Result<(), Error>,
}

struct RootState {
    id: Option<JsonAtom>,
    name: Option<JsonAtom>,
    kind: Option<JsonAtom>,
    extra: ExtraMap,
    responses: Vec<ResponseItem>,
    saw_messages: bool,
    saw_account: bool,
    export_extra: ExtraMap,
}

impl<'de> DeserializeSeed<'de> for RootVisitor<'_> {
    type Value = ExtraMap;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for RootVisitor<'_> {
    type Value = ExtraMap;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a Telegram Desktop result.json object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut state = RootState {
            id: None,
            name: None,
            kind: None,
            extra: ExtraMap::new(),
            responses: Vec::new(),
            saw_messages: false,
            saw_account: false,
            export_extra: ExtraMap::new(),
        };
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "id" => state.id = Some(map.next_value()?),
                "name" => state.name = Some(map.next_value()?),
                "type" => state.kind = Some(map.next_value()?),
                "messages" => {
                    state.saw_messages = true;
                    map.next_value_seed(MessageSeq {
                        responses: &mut state.responses,
                    })?;
                }
                "personal_information" => {
                    state.saw_account = true;
                    let value = map.next_value::<JsonAtom>()?;
                    state
                        .export_extra
                        .insert("personal_information".to_owned(), value);
                }
                "chats" => {
                    state.saw_account = true;
                    map.next_value_seed(ChatsSeed { each: self.each })?;
                }
                "left_chats" => {
                    state.saw_account = true;
                    map.next_value_seed(ChatsSeed { each: self.each })?;
                }
                _ => {
                    let value = map.next_value::<JsonAtom>()?;
                    state.extra.insert(key, value);
                }
            }
        }
        if state.saw_messages
            || (!state.saw_account && (state.id.is_some() || state.name.is_some()))
        {
            let item = chat_item(
                state.id,
                state.name,
                state.kind,
                state.extra,
                state.responses,
            );
            (self.each)(item).map_err(de::Error::custom)?;
        } else if !state.extra.is_empty() {
            for (key, value) in state.extra {
                state.export_extra.entry(key).or_insert(value);
            }
        }
        Ok(state.export_extra)
    }
}

struct ChatsSeed<'a> {
    each: &'a mut dyn FnMut(ConversationItem) -> Result<(), Error>,
}

impl<'de> DeserializeSeed<'de> for ChatsSeed<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ChatsSeed<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a Telegram chats object with a list array")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if key == "list" {
                map.next_value_seed(ChatSeq { each: self.each })?;
            } else {
                let _ = map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

struct ChatSeq<'a> {
    each: &'a mut dyn FnMut(ConversationItem) -> Result<(), Error>,
}

impl<'de> DeserializeSeed<'de> for ChatSeq<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for ChatSeq<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an array of Telegram chats")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(item) = seq.next_element::<TelegramChatItem>()? {
            (self.each)(item.0).map_err(de::Error::custom)?;
        }
        Ok(())
    }
}

struct TelegramChatItem(ConversationItem);

impl<'de> Deserialize<'de> for TelegramChatItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(TelegramChatVisitor).map(Self)
    }
}

struct TelegramChatVisitor;

impl<'de> Visitor<'de> for TelegramChatVisitor {
    type Value = ConversationItem;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a Telegram chat object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut id = None;
        let mut name = None;
        let mut kind = None;
        let mut extra = ExtraMap::new();
        let mut responses = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "id" => id = Some(map.next_value()?),
                "name" => name = Some(map.next_value()?),
                "type" => kind = Some(map.next_value()?),
                "messages" => {
                    map.next_value_seed(MessageSeq {
                        responses: &mut responses,
                    })?;
                }
                _ => {
                    extra.insert(key, map.next_value()?);
                }
            }
        }
        Ok(chat_item(id, name, kind, extra, responses))
    }
}

struct MessageSeq<'a> {
    responses: &'a mut Vec<ResponseItem>,
}

impl<'de> DeserializeSeed<'de> for MessageSeq<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for MessageSeq<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an array of Telegram messages")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(message) = seq.next_element::<TelegramMessage>()? {
            self.responses.push(message.into_response());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct TelegramMessage {
    #[serde(default)]
    id: Option<JsonAtom>,
    #[serde(default)]
    from: Option<JsonAtom>,
    #[serde(default)]
    from_id: Option<JsonAtom>,
    #[serde(default)]
    date: Option<JsonAtom>,
    #[serde(default)]
    text: Option<JsonAtom>,
    #[serde(flatten)]
    extra: ExtraMap,
}

impl TelegramMessage {
    fn into_response(self) -> ResponseItem {
        let body = self.text.as_ref().and_then(concat_telegram_text);
        let mut extra = self.extra;
        if let Some(from) = self.from.clone() {
            extra.insert("from".to_owned(), from);
        }
        if let Some(from_id) = self.from_id.clone() {
            extra.insert("from_id".to_owned(), from_id);
        }
        if let Some(date) = self.date.clone() {
            extra.insert("date".to_owned(), date);
        }
        if let Some(text) = self.text.clone() {
            extra.insert("text".to_owned(), text);
        }
        let sender = match &self.from {
            Some(JsonAtom::String(name)) if !name.is_empty() => Some(name.clone()),
            _ => None,
        };
        let create_time = self.date.as_ref().map(telegram_timestamp);
        ResponseItem {
            response: Response {
                _id: self.id.as_ref().and_then(stringify_id),
                conversation_id: None,
                message: body.map(JsonAtom::String),
                sender,
                create_time,
                extra,
                ..Default::default()
            },
            share_link: None,
            extra: ExtraMap::new(),
        }
    }
}

fn chat_item(
    id: Option<JsonAtom>,
    name: Option<JsonAtom>,
    kind: Option<JsonAtom>,
    mut extra: ExtraMap,
    mut responses: Vec<ResponseItem>,
) -> ConversationItem {
    let conversation_id = id.as_ref().and_then(stringify_id);
    if let Some(name) = name.clone() {
        extra.entry("name".to_owned()).or_insert(name);
    }
    if let Some(kind) = kind {
        extra.entry("type".to_owned()).or_insert(kind);
    }
    if let Some(id) = id.clone() {
        extra.entry("id".to_owned()).or_insert(id);
    }
    let title = name.as_ref().and_then(json_atom_text);
    for response in &mut responses {
        if response.response.conversation_id.is_none() {
            response.response.conversation_id = conversation_id.clone();
        }
    }
    ConversationItem {
        conversation: Conversation {
            id: conversation_id,
            title,
            extra,
            ..Default::default()
        },
        responses,
        extra: ExtraMap::new(),
    }
}

fn json_atom_text(atom: &JsonAtom) -> Option<String> {
    match atom {
        JsonAtom::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

fn stringify_id(atom: &JsonAtom) -> Option<String> {
    match atom {
        JsonAtom::String(text) if !text.is_empty() => Some(text.clone()),
        JsonAtom::I64(value) => Some(value.to_string()),
        JsonAtom::U64(value) => Some(value.to_string()),
        JsonAtom::F64(value) if value.is_finite() && *value == value.trunc() => {
            Some(format!("{value:.0}"))
        }
        _ => None,
    }
}

fn telegram_timestamp(atom: &JsonAtom) -> Timestamp {
    match atom {
        JsonAtom::String(text) => Timestamp::Iso(text.clone()),
        other => Timestamp::Other(other.clone()),
    }
}

fn concat_telegram_text(atom: &JsonAtom) -> Option<String> {
    let mut out = String::new();
    push_telegram_text(&mut out, atom);
    if out.is_empty() { None } else { Some(out) }
}

fn push_telegram_text(out: &mut String, atom: &JsonAtom) {
    match atom {
        JsonAtom::String(text) => out.push_str(text),
        JsonAtom::Array(parts) => {
            for part in parts {
                push_telegram_text(out, part);
            }
        }
        JsonAtom::Object(fields) => {
            if let Some(text) = fields.get("text") {
                push_telegram_text(out, text);
            }
        }
        JsonAtom::Null
        | JsonAtom::Bool(_)
        | JsonAtom::I64(_)
        | JsonAtom::U64(_)
        | JsonAtom::F64(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("majestic-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn concat_telegram_text_joins_entity_array() {
        let mut entity = ExtraMap::new();
        entity.insert("type".to_owned(), JsonAtom::String("plain".to_owned()));
        entity.insert("text".to_owned(), JsonAtom::String("telegram".to_owned()));
        let atom = JsonAtom::Array(vec![
            JsonAtom::String("Catfooding ".to_owned()),
            JsonAtom::Object(entity),
            JsonAtom::String(" fixture".to_owned()),
        ]);
        assert_eq!(
            concat_telegram_text(&atom).as_deref(),
            Some("Catfooding telegram fixture")
        );
    }

    #[test]
    fn stringify_telegram_id_from_number() {
        assert_eq!(stringify_id(&JsonAtom::I64(1)).as_deref(), Some("1"));
    }

    #[test]
    fn full_account_chats_list_yields_conversation() {
        let dir = test_dir("telegram-full-account");
        let path = dir.join(RESULT_FILE_NAME);
        fs::write(
            &path,
            r#"{
  "personal_information": {"user_id": 1, "username": "synthtelegramuser"},
  "chats": {
    "list": [
      {
        "name": "synth chat",
        "type": "personal_chat",
        "id": 1,
        "messages": [
          {
            "id": 1,
            "date": "2026-01-01T00:00:00",
            "from": "synth-from",
            "from_id": "user1",
            "text": [{"type": "plain", "text": "Catfooding telegram fixture"}]
          }
        ]
      }
    ]
  }
}"#,
        )
        .expect("write synthetic full-account result.json");
        let meta = telegram_meta(&path)
            .expect("peek full-account")
            .expect("full-account is telegram");
        assert_eq!(meta.username.as_deref(), Some("synthtelegramuser"));
        let mut items = Vec::new();
        let extra = for_each_telegram_conversation(&path, |item| {
            items.push(item);
            Ok(())
        })
        .expect("stream full-account");
        assert!(
            extra.contains_key("personal_information"),
            "personal_information stays leftover on the export"
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].conversation.id.as_deref(), Some("1"));
        assert_eq!(items[0].conversation.title.as_deref(), Some("synth chat"));
        assert_eq!(
            items[0].responses[0].response.message,
            Some(JsonAtom::String("Catfooding telegram fixture".to_owned()))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_account_two_chats_are_two_conversations() {
        let dir = test_dir("telegram-two-chats");
        let path = dir.join(RESULT_FILE_NAME);
        fs::write(
            &path,
            r#"{
  "personal_information": {"user_id": 1, "username": "synthtelegramuser"},
  "chats": {
    "list": [
      {
        "name": "first chat",
        "type": "personal_chat",
        "id": 1,
        "messages": [{"id": 1, "date": "2026-01-01T00:00:00", "text": "alpha token"}]
      },
      {
        "name": "second chat",
        "type": "personal_chat",
        "id": 2,
        "messages": [{"id": 1, "date": "2026-01-01T00:00:01", "text": "beta token"}]
      }
    ]
  }
}"#,
        )
        .expect("write two-chat result.json");
        let mut items = Vec::new();
        for_each_telegram_conversation(&path, |item| {
            items.push(item);
            Ok(())
        })
        .expect("stream two chats");
        assert_eq!(
            items.len(),
            2,
            "each chats.list entry is one conversation, got {}",
            items.len()
        );
        assert_eq!(items[0].conversation.id.as_deref(), Some("1"));
        assert_eq!(items[1].conversation.id.as_deref(), Some("2"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn left_chats_list_yields_conversation() {
        let dir = test_dir("telegram-left-chats");
        let path = dir.join(RESULT_FILE_NAME);
        fs::write(
            &path,
            r#"{
  "personal_information": {"user_id": 1, "username": "synthtelegramuser"},
  "chats": { "list": [] },
  "left_chats": {
    "list": [
      {
        "name": "left chat",
        "type": "personal_chat",
        "id": 9,
        "messages": [{"id": 1, "date": "2026-01-01T00:00:00", "text": "left-chat token"}]
      }
    ]
  }
}"#,
        )
        .expect("write left_chats result.json");
        let meta = telegram_meta(&path)
            .expect("peek left_chats")
            .expect("left_chats with personal_information is telegram");
        assert_eq!(meta.username.as_deref(), Some("synthtelegramuser"));
        let mut items = Vec::new();
        for_each_telegram_conversation(&path, |item| {
            items.push(item);
            Ok(())
        })
        .expect("stream left_chats");
        assert_eq!(
            items.len(),
            1,
            "left_chats.list entries are conversations, got {}",
            items.len()
        );
        assert_eq!(items[0].conversation.id.as_deref(), Some("9"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn chatexport_parent_name_is_telegram_path() {
        let dir = test_dir("telegram-chatexport-name");
        let export = dir.join("ChatExport_2026-05-06");
        fs::create_dir_all(&export).expect("ChatExport dir");
        let path = export.join(RESULT_FILE_NAME);
        fs::write(&path, r#"{"about":"not a grok dump"}"#).expect("write about-only result.json");
        assert!(is_chatexport_result_json(&path));
        assert!(telegram_meta(&path).expect("peek").is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
