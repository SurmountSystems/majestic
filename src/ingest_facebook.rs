//! Stream Facebook Download Your Information JSON into conversation records.
//!
//! A zip or unzipped tree is Facebook when a path component is
//! `your_facebook_activity`. Each JSON array item or object is one
//! conversation. Leftover keys stay in extra. Searchable text is `title`,
//! string fields under `data`, and message `content`. Conversation id is
//! stable from the inner path plus `id`, `timestamp`, or array index. Same
//! id with different bytes uses the existing merge. Do not parse a whole
//! export as one `serde_json::Value`.

use std::fmt;
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use serde::Deserializer;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

use crate::Error;
use crate::schema::{
    Conversation, ConversationItem, ExtraMap, JsonAtom, Response, ResponseItem, Timestamp,
};
use crate::zip;

/// Inner directories that mark a Meta DYI export (Facebook or Instagram).
pub(crate) const FACEBOOK_ACTIVITY_DIR: &str = "your_facebook_activity";
pub(crate) const INSTAGRAM_ACTIVITY_DIR: &str = "your_instagram_activity";

pub(crate) fn has_activity_path(name: &str) -> bool {
    name.split(['/', '\\'])
        .any(|part| part == FACEBOOK_ACTIVITY_DIR || part == INSTAGRAM_ACTIVITY_DIR)
}

pub(crate) fn is_facebook_activity_json(name: &str) -> bool {
    if !has_activity_path(name) {
        return false;
    }
    zip::inner_file_name(name)
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("json"))
}

pub(crate) fn is_facebook_image_name(name: &str) -> bool {
    let file_name = zip::inner_file_name(name);
    let Some((_, ext)) = file_name.rsplit_once('.') else {
        return false;
    };
    ext.eq_ignore_ascii_case("jpg")
        || ext.eq_ignore_ascii_case("jpeg")
        || ext.eq_ignore_ascii_case("png")
        || ext.eq_ignore_ascii_case("gif")
}

pub(crate) fn is_facebook_dir(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let name = dir.file_name().and_then(|name| name.to_str());
    if name == Some(FACEBOOK_ACTIVITY_DIR) || name == Some(INSTAGRAM_ACTIVITY_DIR) {
        return true;
    }
    dir.join(FACEBOOK_ACTIVITY_DIR).is_dir() || dir.join(INSTAGRAM_ACTIVITY_DIR).is_dir()
}

pub(crate) fn activity_root(dir: &Path) -> PathBuf {
    let name = dir.file_name().and_then(|name| name.to_str());
    if name == Some(FACEBOOK_ACTIVITY_DIR) || name == Some(INSTAGRAM_ACTIVITY_DIR) {
        return dir.to_path_buf();
    }
    let facebook = dir.join(FACEBOOK_ACTIVITY_DIR);
    if facebook.is_dir() {
        return facebook;
    }
    let instagram = dir.join(INSTAGRAM_ACTIVITY_DIR);
    if instagram.is_dir() {
        instagram
    } else {
        dir.to_path_buf()
    }
}

/// Account stem from `facebook-{account}-` or `instagram-{account}-`.
///
/// Skips values that contain `@` (never email). `None` means use `$USER`.
pub(crate) fn account_from_path(path: &Path) -> Option<String> {
    if let Some(account) = account_from_file_name(path) {
        return Some(account);
    }
    account_from_file_name(path.parent()?)
}

fn account_from_file_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    let rest = strip_facebook_prefix(stem)?;
    let account = rest.split('-').next().filter(|part| !part.is_empty())?;
    if account.contains('@') {
        return None;
    }
    Some(account.to_owned())
}

fn strip_facebook_prefix(stem: &str) -> Option<&str> {
    for prefix in ["facebook-", "instagram-"] {
        if stem.len() < prefix.len() || !stem.is_char_boundary(prefix.len()) {
            continue;
        }
        if stem[..prefix.len()].eq_ignore_ascii_case(prefix) {
            return Some(&stem[prefix.len()..]);
        }
    }
    None
}

/// `*.json` files under Facebook and Instagram activity folders, sorted.
pub(crate) fn json_files_in_tree(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut files = Vec::new();
    for root in activity_roots(dir) {
        collect_files(&root, 32, &mut files, is_json_file)?;
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn activity_roots(dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let name = dir.file_name().and_then(|name| name.to_str());
    if name == Some(FACEBOOK_ACTIVITY_DIR) || name == Some(INSTAGRAM_ACTIVITY_DIR) {
        return vec![dir.to_path_buf()];
    }
    let facebook = dir.join(FACEBOOK_ACTIVITY_DIR);
    if facebook.is_dir() {
        roots.push(facebook);
    }
    let instagram = dir.join(INSTAGRAM_ACTIVITY_DIR);
    if instagram.is_dir() {
        roots.push(instagram);
    }
    if roots.is_empty() {
        roots.push(activity_root(dir));
    }
    roots
}

/// jpg/png/gif files under the Facebook export root, sorted.
pub(crate) fn image_files_in_tree(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut files = Vec::new();
    collect_files(dir, 32, &mut files, is_image_file)?;
    files.sort();
    Ok(files)
}

fn is_json_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

fn is_image_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_facebook_image_name)
}

fn collect_files(
    dir: &Path,
    depth: u32,
    out: &mut Vec<PathBuf>,
    want: fn(&Path) -> bool,
) -> Result<(), Error> {
    if depth == 0 {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_files(&path, depth - 1, out, want)?;
            continue;
        }
        if file_type.is_file() && want(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// Inner path used as the stable id prefix (`your_facebook_activity/...` or Instagram).
pub(crate) fn json_label(export_root: &Path, file: &Path) -> String {
    let root = activity_root(export_root);
    let rel = file
        .strip_prefix(&root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    let activity = if file
        .to_string_lossy()
        .split(['/', '\\'])
        .any(|part| part == INSTAGRAM_ACTIVITY_DIR)
    {
        INSTAGRAM_ACTIVITY_DIR
    } else {
        FACEBOOK_ACTIVITY_DIR
    };
    if rel == activity || rel.starts_with(&format!("{activity}/")) {
        rel
    } else {
        format!("{activity}/{rel}")
    }
}

/// Stream each JSON array item or the root object as a conversation.
pub(crate) fn for_each_facebook_item<R: Read>(
    reader: R,
    label: &str,
    mut each: impl FnMut(ConversationItem) -> Result<(), Error>,
) -> Result<(), Error> {
    let reader = BufReader::with_capacity(256 * 1024, reader);
    let mut de = serde_json::Deserializer::from_reader(reader);
    RootVisitor {
        label,
        each: &mut each,
    }
    .deserialize(&mut de)
    .map_err(|error| Error::ingest(format!("failed to parse {label}: {error}")))?;
    de.end()
        .map_err(|error| Error::ingest(format!("failed to parse {label}: {error}")))?;
    Ok(())
}

struct RootVisitor<'a> {
    label: &'a str,
    each: &'a mut dyn FnMut(ConversationItem) -> Result<(), Error>,
}

impl<'de> DeserializeSeed<'de> for RootVisitor<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for RootVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a Facebook DYI JSON array or object")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut index = 0usize;
        while let Some(item) = seq.next_element::<FacebookObject>()? {
            (self.each)(item.into_item(self.label, index)).map_err(de::Error::custom)?;
            index += 1;
        }
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<(), A::Error> {
        let item = FacebookObjectVisitor.visit_map(map)?;
        (self.each)(item.into_item(self.label, 0)).map_err(de::Error::custom)?;
        Ok(())
    }
}

struct FacebookObject {
    title: Option<String>,
    id: Option<String>,
    timestamp: Option<JsonAtom>,
    data: Option<JsonAtom>,
    content: Option<String>,
    extra: ExtraMap,
}

impl FacebookObject {
    fn into_item(self, path: &str, index: usize) -> ConversationItem {
        let path = path.replace('\\', "/");
        let id = stable_id(&path, index, &self.id, self.timestamp.as_ref());
        let mut responses = Vec::new();
        push_message_responses(&self.extra, &id, &mut responses);
        if let Some(text) = self.content {
            responses.push(text_response(text, Some(id.as_str()), None));
        }
        if let Some(data) = &self.data {
            let text = data_strings(data);
            if !text.is_empty() {
                responses.push(text_response(text, Some(id.as_str()), None));
            }
        }
        ConversationItem {
            conversation: Conversation {
                id: Some(id),
                title: self.title,
                create_time: self.timestamp.map(atom_to_timestamp_owned),
                extra: self.extra,
                ..Default::default()
            },
            responses,
            extra: ExtraMap::new(),
        }
    }
}

impl<'de> serde::Deserialize<'de> for FacebookObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(FacebookObjectVisitor)
    }
}

struct FacebookObjectVisitor;

impl<'de> Visitor<'de> for FacebookObjectVisitor {
    type Value = FacebookObject;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a Facebook DYI JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut title = None;
        let mut id = None;
        let mut timestamp = None;
        let mut data = None;
        let mut content = None;
        let mut extra = ExtraMap::new();
        while let Some(key) = map.next_key::<String>()? {
            let value: JsonAtom = map.next_value()?;
            match key.as_str() {
                "title" => title = json_atom_text(&value),
                "id" => id = stringify_id(&value),
                "timestamp" | "timestamp_ms" => {
                    if timestamp.is_none() {
                        timestamp = Some(value.clone());
                    }
                }
                "data" => data = Some(value.clone()),
                "content" => content = json_atom_text(&value),
                _ => {}
            }
            extra.insert(key, value);
        }
        Ok(FacebookObject {
            title,
            id,
            timestamp,
            data,
            content,
            extra,
        })
    }
}

fn stable_id(
    path: &str,
    index: usize,
    id: &Option<String>,
    timestamp: Option<&JsonAtom>,
) -> String {
    if let Some(id) = id.as_deref().filter(|id| !id.is_empty()) {
        return format!("{path}#{id}");
    }
    if let Some(stamp) = timestamp.and_then(stringify_id) {
        return format!("{path}#{stamp}");
    }
    format!("{path}#{index}")
}

fn push_message_responses(extra: &ExtraMap, conversation_id: &str, out: &mut Vec<ResponseItem>) {
    let Some(JsonAtom::Array(items)) = extra.get("messages") else {
        return;
    };
    for item in items {
        let JsonAtom::Object(fields) = item else {
            continue;
        };
        let Some(content) = fields.get("content").and_then(json_atom_text) else {
            continue;
        };
        let timestamp = fields
            .get("timestamp_ms")
            .cloned()
            .or_else(|| fields.get("timestamp").cloned());
        out.push(text_response(content, Some(conversation_id), timestamp));
    }
}

fn text_response(
    text: String,
    conversation_id: Option<&str>,
    timestamp: Option<JsonAtom>,
) -> ResponseItem {
    ResponseItem {
        response: Response {
            _id: timestamp.as_ref().and_then(stringify_id),
            conversation_id: conversation_id.map(str::to_owned),
            message: Some(JsonAtom::String(text)),
            create_time: timestamp.map(atom_to_timestamp_owned),
            extra: ExtraMap::new(),
            ..Default::default()
        },
        share_link: None,
        extra: ExtraMap::new(),
    }
}

fn data_strings(atom: &JsonAtom) -> String {
    let mut parts = Vec::new();
    collect_strings(atom, &mut parts);
    parts.join("\n")
}

fn collect_strings(atom: &JsonAtom, out: &mut Vec<String>) {
    match atom {
        JsonAtom::String(text) if !text.is_empty() => out.push(text.clone()),
        JsonAtom::Array(values) => {
            for value in values {
                collect_strings(value, out);
            }
        }
        JsonAtom::Object(fields) => {
            for value in fields.values() {
                collect_strings(value, out);
            }
        }
        _ => {}
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

fn atom_to_timestamp_owned(atom: JsonAtom) -> Timestamp {
    match atom {
        JsonAtom::String(text) => Timestamp::Iso(text),
        other => Timestamp::Other(other),
    }
}
