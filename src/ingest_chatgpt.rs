//! Stream ChatGPT export `conversations-*.json` into conversation records.
//!
//! Each array item is one conversation. `mapping`, `title`, `create_time`, and
//! `conversation_id` become conversations/responses. Every schema key stays in
//! leftover extra. Searchable text comes from `content.parts`.
//! Account stem is process `$USER`, never an email from `user.json`.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::Error;
use crate::schema::{
    Conversation, ConversationItem, ExtraMap, JsonAtom, Response, ResponseItem, Timestamp,
};

pub(crate) const USER_FILE_NAME: &str = "user.json";

pub(crate) fn is_user_json_name(name: &str) -> bool {
    crate::zip::inner_file_name(name).eq_ignore_ascii_case(USER_FILE_NAME)
}

pub(crate) fn is_conversations_json_name(name: &str) -> bool {
    let name = crate::zip::inner_file_name(name);
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    stem == "conversations" || stem.starts_with("conversations-")
}

pub(crate) fn is_chatgpt_dir(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    if !dir.join(USER_FILE_NAME).is_file() {
        return false;
    }
    conversation_files_in_dir(dir)
        .ok()
        .is_some_and(|files| !files.is_empty())
}

pub(crate) fn conversation_files_in_dir(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if is_conversations_json_name(name) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Stream each array item as a conversation. Does not parse the file as one `Value`.
pub(crate) fn for_each_chatgpt_conversation<R: Read>(
    reader: R,
    label: &Path,
    mut each: impl FnMut(ConversationItem) -> Result<(), Error>,
) -> Result<(), Error> {
    let reader = BufReader::with_capacity(256 * 1024, reader);
    let mut de = serde_json::Deserializer::from_reader(reader);
    ArrayVisitor { each: &mut each }
        .deserialize(&mut de)
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    de.end()
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    Ok(())
}

pub(crate) fn read_user_json_atom<R: Read>(reader: R, label: &Path) -> Result<JsonAtom, Error> {
    let mut de = serde_json::Deserializer::from_reader(BufReader::new(reader));
    let value = JsonAtom::deserialize(&mut de)
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    de.end()
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    Ok(value)
}

pub(crate) fn read_user_json_file(path: &Path) -> Result<Option<JsonAtom>, Error> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(Some(read_user_json_atom(file, path)?))
}

struct ArrayVisitor<'a> {
    each: &'a mut dyn FnMut(ConversationItem) -> Result<(), Error>,
}

impl<'de> DeserializeSeed<'de> for ArrayVisitor<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for ArrayVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a ChatGPT conversations.json array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(item) = seq.next_element::<ChatGptConvo>()? {
            (self.each)(item.into_item()).map_err(de::Error::custom)?;
        }
        Ok(())
    }
}

struct ChatGptConvo {
    title: Option<String>,
    create_time: Option<JsonAtom>,
    conversation_id: Option<String>,
    mapping: Option<JsonAtom>,
    current_node: Option<String>,
    extra: ExtraMap,
}

impl ChatGptConvo {
    fn into_item(self) -> ConversationItem {
        let responses = mapping_to_responses(
            self.mapping.as_ref(),
            self.current_node.as_deref(),
            self.conversation_id.as_deref(),
        );
        ConversationItem {
            conversation: Conversation {
                id: self.conversation_id,
                title: self.title,
                create_time: self.create_time.as_ref().map(atom_to_timestamp),
                extra: self.extra,
                ..Default::default()
            },
            responses,
            extra: ExtraMap::new(),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ChatGptConvo {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ChatGptConvoVisitor)
    }
}

struct ChatGptConvoVisitor;

impl<'de> Visitor<'de> for ChatGptConvoVisitor {
    type Value = ChatGptConvo;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a ChatGPT conversation object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut title = None;
        let mut create_time = None;
        let mut conversation_id = None;
        let mut mapping = None;
        let mut current_node = None;
        let mut extra = ExtraMap::new();
        while let Some(key) = map.next_key::<String>()? {
            let value: JsonAtom = map.next_value()?;
            match key.as_str() {
                "title" => title = json_atom_text(&value),
                "create_time" => create_time = Some(value.clone()),
                "conversation_id" => conversation_id = stringify_id(&value),
                "mapping" => mapping = Some(value.clone()),
                "current_node" => current_node = json_atom_text(&value),
                _ => {}
            }
            extra.insert(key, value);
        }
        Ok(ChatGptConvo {
            title,
            create_time,
            conversation_id,
            mapping,
            current_node,
            extra,
        })
    }
}

fn mapping_to_responses(
    mapping: Option<&JsonAtom>,
    current_node: Option<&str>,
    conversation_id: Option<&str>,
) -> Vec<ResponseItem> {
    let Some(JsonAtom::Object(nodes)) = mapping else {
        return Vec::new();
    };
    let mut responses = Vec::new();
    for id in message_node_order(nodes, current_node) {
        let Some(JsonAtom::Object(node)) = nodes.get(&id) else {
            continue;
        };
        let Some(message) = node.get("message") else {
            continue;
        };
        if matches!(message, JsonAtom::Null) {
            continue;
        }
        if let Some(item) = message_to_response(node, message, conversation_id) {
            responses.push(item);
        }
    }
    responses
}

fn message_node_order(nodes: &ExtraMap, current_node: Option<&str>) -> Vec<String> {
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = current_node.map(str::to_owned);
    if cursor.is_none() {
        cursor = find_leaf(nodes);
    }
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            break;
        }
        chain.push(id.clone());
        cursor = nodes.get(&id).and_then(node_parent);
    }
    chain.reverse();
    let mut rest: Vec<String> = nodes
        .keys()
        .filter(|key| !seen.contains(*key))
        .cloned()
        .collect();
    rest.sort();
    chain.extend(rest);
    chain
}

fn find_leaf(nodes: &ExtraMap) -> Option<String> {
    let mut ids: Vec<&String> = nodes.keys().collect();
    ids.sort();
    ids.into_iter()
        .rev()
        .find(|id| {
            nodes.get(*id).is_some_and(|node| match node {
                JsonAtom::Object(fields) => match fields.get("children") {
                    Some(JsonAtom::Array(children)) => children.is_empty(),
                    _ => true,
                },
                _ => false,
            })
        })
        .cloned()
}

fn node_parent(node: &JsonAtom) -> Option<String> {
    let JsonAtom::Object(fields) = node else {
        return None;
    };
    match fields.get("parent") {
        Some(JsonAtom::String(name)) if !name.is_empty() => Some(name.clone()),
        _ => None,
    }
}

fn message_to_response(
    node: &ExtraMap,
    message: &JsonAtom,
    conversation_id: Option<&str>,
) -> Option<ResponseItem> {
    let JsonAtom::Object(msg) = message else {
        return None;
    };
    let body = content_parts_text(msg.get("content"));
    let sender = msg.get("author").and_then(author_role);
    let id = msg
        .get("id")
        .and_then(stringify_id)
        .or_else(|| node.get("id").and_then(stringify_id));
    let create_time = msg.get("create_time").cloned().map(atom_to_timestamp_owned);
    let mut extra = ExtraMap::new();
    for (key, value) in node {
        extra.insert(key.clone(), value.clone());
    }
    for (key, value) in msg {
        extra.insert(key.clone(), value.clone());
    }
    Some(ResponseItem {
        response: Response {
            _id: id,
            conversation_id: conversation_id.map(str::to_owned),
            message: body.map(JsonAtom::String),
            sender,
            create_time,
            extra,
            ..Default::default()
        },
        share_link: None,
        extra: ExtraMap::new(),
    })
}

fn content_parts_text(content: Option<&JsonAtom>) -> Option<String> {
    let content = content?;
    let mut out = String::new();
    push_content_text(&mut out, content);
    if out.is_empty() { None } else { Some(out) }
}

fn push_content_text(out: &mut String, atom: &JsonAtom) {
    match atom {
        JsonAtom::String(text) => out.push_str(text),
        JsonAtom::Array(parts) => {
            for part in parts {
                push_part_text(out, part);
            }
        }
        JsonAtom::Object(fields) => {
            if let Some(parts) = fields.get("parts") {
                push_content_text(out, parts);
            } else if let Some(text) = fields.get("text") {
                push_content_text(out, text);
            }
        }
        JsonAtom::Null
        | JsonAtom::Bool(_)
        | JsonAtom::I64(_)
        | JsonAtom::U64(_)
        | JsonAtom::F64(_) => {}
    }
}

fn push_part_text(out: &mut String, part: &JsonAtom) {
    match part {
        JsonAtom::String(text) => out.push_str(text),
        JsonAtom::Object(fields) => {
            if let Some(JsonAtom::String(text)) = fields.get("text") {
                out.push_str(text);
            }
        }
        JsonAtom::Array(parts) => {
            for part in parts {
                push_part_text(out, part);
            }
        }
        JsonAtom::Null
        | JsonAtom::Bool(_)
        | JsonAtom::I64(_)
        | JsonAtom::U64(_)
        | JsonAtom::F64(_) => {}
    }
}

fn author_role(author: &JsonAtom) -> Option<String> {
    match author {
        JsonAtom::Object(fields) => match fields.get("role") {
            Some(JsonAtom::String(role)) if !role.is_empty() => Some(role.clone()),
            _ => None,
        },
        JsonAtom::String(role) if !role.is_empty() => Some(role.clone()),
        _ => None,
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

fn atom_to_timestamp(atom: &JsonAtom) -> Timestamp {
    atom_to_timestamp_owned(atom.clone())
}

fn atom_to_timestamp_owned(atom: JsonAtom) -> Timestamp {
    match atom {
        JsonAtom::String(text) => Timestamp::Iso(text),
        other => Timestamp::Other(other),
    }
}
