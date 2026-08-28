//! Stream an X account archive (`data/account.js` plus `data/tweet.js` or
//! `data/tweets.js`) into conversation records.
//!
//! Official downloads are zip files whose inner names look like
//! `data/account.js` and `data/tweets.js`. Each payload file starts with a
//! `window.YTD.*.partN =` assignment and then a JSON array. One array item is
//! one tweet, like, or direct message conversation. Leftover keys stay in
//! extra. Searchable text is tweet `full_text` / `text`, like `fullText`, and
//! direct message `text`. Account stem is `account.username`, never email.
//! Do not parse a whole archive as one `serde_json::Value`.

use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde::Deserializer;
use serde::de::{self, DeserializeSeed, SeqAccess, Visitor};

use crate::Error;
use crate::schema::{
    Conversation, ConversationItem, ExtraMap, JsonAtom, Response, ResponseItem, Timestamp,
};
use crate::zip;

/// Account payload in an X archive (`data/account.js`).
pub(crate) const ACCOUNT_FILE_NAME: &str = "account.js";
/// Older tweet payload name.
pub(crate) const TWEET_FILE_NAME: &str = "tweet.js";
/// Current tweet payload name.
pub(crate) const TWEETS_FILE_NAME: &str = "tweets.js";

pub(crate) fn is_account_js_name(name: &str) -> bool {
    zip::inner_file_name(name).eq_ignore_ascii_case(ACCOUNT_FILE_NAME)
}

pub(crate) fn is_tweet_js_name(name: &str) -> bool {
    let file = zip::inner_file_name(name);
    file.eq_ignore_ascii_case(TWEET_FILE_NAME) || file.eq_ignore_ascii_case(TWEETS_FILE_NAME)
}

pub(crate) fn is_twitter_payload_js_name(name: &str) -> bool {
    let file = zip::inner_file_name(name);
    let Some(stem) = strip_js_stem(file) else {
        return false;
    };
    let stem = stem.to_ascii_lowercase();
    stem == "tweet"
        || stem == "tweets"
        || stem.starts_with("tweet-")
        || stem.starts_with("tweets-")
        || stem == "like"
        || stem == "likes"
        || stem.starts_with("like-")
        || stem.starts_with("likes-")
        || stem == "direct-messages"
        || stem.starts_with("direct-messages")
}

fn strip_js_stem(file: &str) -> Option<&str> {
    file.strip_suffix(".js")
        .or_else(|| file.strip_suffix(".JS"))
}

/// Zip or unzipped tree is an X archive when `account.js` and a tweet payload
/// share a `data/` folder, or sit in the directory itself.
pub(crate) fn is_twitter_dir(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    data_root(dir)
        .map(|root| root.join(ACCOUNT_FILE_NAME).is_file() && tweet_payload_in_dir(&root))
        .unwrap_or(false)
}

fn tweet_payload_in_dir(root: &Path) -> bool {
    root.join(TWEET_FILE_NAME).is_file() || root.join(TWEETS_FILE_NAME).is_file()
}

pub(crate) fn data_root(dir: &Path) -> Option<PathBuf> {
    let nested = dir.join("data");
    if nested.is_dir() && nested.join(ACCOUNT_FILE_NAME).is_file() {
        return Some(nested);
    }
    if dir.join(ACCOUNT_FILE_NAME).is_file() {
        return Some(dir.to_path_buf());
    }
    None
}

/// `*.js` payload files under the archive `data/` folder, sorted. Skips
/// `account.js` so email never becomes a searchable span.
pub(crate) fn payload_files_in_tree(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let Some(root) = data_root(dir) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    let entries = match fs::read_dir(&root) {
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
        if is_twitter_payload_js_name(name) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

pub(crate) fn account_file_in_tree(dir: &Path) -> Option<PathBuf> {
    let root = data_root(dir)?;
    let path = root.join(ACCOUNT_FILE_NAME);
    path.is_file().then_some(path)
}

/// Account stem from `account.username`. Skips values that contain `@`.
pub(crate) fn username_from_reader<R: Read>(
    reader: R,
    label: &Path,
) -> Result<Option<String>, Error> {
    let mut reader = BufReader::with_capacity(256 * 1024, reader);
    skip_ytd_assignment(&mut reader, label)?;
    let mut de = serde_json::Deserializer::from_reader(reader);
    let username = FirstUsernameVisitor
        .deserialize(&mut de)
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    de.end()
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    Ok(username.filter(|name| !name.is_empty() && !name.contains('@')))
}

pub(crate) fn username_from_file(path: &Path) -> Result<Option<String>, Error> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    username_from_reader(file, path)
}

struct FirstUsernameVisitor;

impl<'de> DeserializeSeed<'de> for FirstUsernameVisitor {
    type Value = Option<String>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for FirstUsernameVisitor {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an X account.js YTD array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut username = None;
        while let Some(item) = seq.next_element::<JsonAtom>()? {
            if username.is_none() {
                username = username_from_atom(&item);
            }
        }
        Ok(username)
    }
}

fn username_from_atom(atom: &JsonAtom) -> Option<String> {
    let JsonAtom::Object(root) = atom else {
        return None;
    };
    let account = root.get("account").unwrap_or(atom);
    let JsonAtom::Object(fields) = account else {
        return json_atom_text(account);
    };
    fields.get("username").and_then(json_atom_text)
}

/// Stream each YTD array item as a conversation. Skips the assignment prefix.
pub(crate) fn for_each_twitter_item<R: Read>(
    reader: R,
    label: &Path,
    mut each: impl FnMut(ConversationItem) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut reader = BufReader::with_capacity(256 * 1024, reader);
    skip_ytd_assignment(&mut reader, label)?;
    let mut de = serde_json::Deserializer::from_reader(reader);
    ArrayVisitor { each: &mut each }
        .deserialize(&mut de)
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    de.end()
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", label.display())))?;
    Ok(())
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
        formatter.write_str("an X YTD JSON array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(item) = seq.next_element::<JsonAtom>()? {
            if let Some(conversation) = item_from_atom(item) {
                (self.each)(conversation).map_err(de::Error::custom)?;
            }
        }
        Ok(())
    }
}

fn item_from_atom(atom: JsonAtom) -> Option<ConversationItem> {
    let JsonAtom::Object(fields) = atom else {
        return None;
    };
    if let Some(tweet) = fields.get("tweet").cloned() {
        return Some(tweet_item(&tweet, JsonAtom::Object(fields)));
    }
    if let Some(like) = fields.get("like").cloned() {
        return Some(like_item(&like, JsonAtom::Object(fields)));
    }
    if let Some(dm) = fields.get("dmConversation").cloned() {
        return Some(dm_item(&dm, JsonAtom::Object(fields)));
    }
    None
}

fn tweet_item(tweet: &JsonAtom, extra_atom: JsonAtom) -> ConversationItem {
    let fields = object_fields(tweet);
    let id = fields
        .and_then(|fields| fields.get("id_str").or_else(|| fields.get("id")))
        .and_then(stringify_id)
        .map(|id| format!("tweet-{id}"))
        .unwrap_or_else(|| "tweet".to_owned());
    let text = fields
        .and_then(|fields| fields.get("full_text").or_else(|| fields.get("text")))
        .and_then(json_atom_text);
    let created = fields.and_then(|fields| fields.get("created_at")).cloned();
    conversation_item(
        id,
        text,
        created,
        extra_map_from_atom(extra_atom),
        ExtraMap::new(),
    )
}

fn like_item(like: &JsonAtom, extra_atom: JsonAtom) -> ConversationItem {
    let fields = object_fields(like);
    let id = fields
        .and_then(|fields| fields.get("tweetId"))
        .and_then(stringify_id)
        .map(|id| format!("like-{id}"))
        .unwrap_or_else(|| "like".to_owned());
    let text = fields
        .and_then(|fields| fields.get("fullText").or_else(|| fields.get("full_text")))
        .and_then(json_atom_text);
    conversation_item(
        id,
        text,
        None,
        extra_map_from_atom(extra_atom),
        ExtraMap::new(),
    )
}

fn dm_item(dm: &JsonAtom, extra_atom: JsonAtom) -> ConversationItem {
    let fields = object_fields(dm);
    let id = fields
        .and_then(|fields| fields.get("conversationId"))
        .and_then(stringify_id)
        .map(|id| format!("dm-{id}"))
        .unwrap_or_else(|| "dm".to_owned());
    let mut responses = Vec::new();
    if let Some(JsonAtom::Array(messages)) = fields.and_then(|fields| fields.get("messages")) {
        for message in messages {
            if let Some(item) = dm_response(message, &id) {
                responses.push(item);
            }
        }
    }
    ConversationItem {
        conversation: Conversation {
            id: Some(id),
            extra: extra_map_from_atom(extra_atom),
            ..Default::default()
        },
        responses,
        extra: ExtraMap::new(),
    }
}

fn dm_response(message: &JsonAtom, conversation_id: &str) -> Option<ResponseItem> {
    let JsonAtom::Object(wrapper) = message else {
        return None;
    };
    let body = wrapper
        .get("messageCreate")
        .or_else(|| wrapper.get("message"));
    let fields = body.and_then(object_fields).unwrap_or(wrapper);
    let text = fields.get("text").and_then(json_atom_text)?;
    let created = fields
        .get("createdAt")
        .cloned()
        .or_else(|| fields.get("created_at").cloned());
    Some(text_response(text, Some(conversation_id), created))
}

fn conversation_item(
    id: String,
    text: Option<String>,
    created: Option<JsonAtom>,
    extra: ExtraMap,
    item_extra: ExtraMap,
) -> ConversationItem {
    let responses = text
        .map(|text| vec![text_response(text, Some(id.as_str()), created.clone())])
        .unwrap_or_default();
    ConversationItem {
        conversation: Conversation {
            id: Some(id),
            create_time: created.map(atom_to_timestamp_owned),
            extra,
            ..Default::default()
        },
        responses,
        extra: item_extra,
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

fn extra_map_from_atom(atom: JsonAtom) -> ExtraMap {
    match atom {
        JsonAtom::Object(fields) => fields,
        other => {
            let mut extra = ExtraMap::new();
            extra.insert("value".to_owned(), other);
            extra
        }
    }
}

fn object_fields(atom: &JsonAtom) -> Option<&ExtraMap> {
    match atom {
        JsonAtom::Object(fields) => Some(fields),
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

fn atom_to_timestamp_owned(atom: JsonAtom) -> Timestamp {
    match atom {
        JsonAtom::String(text) => Timestamp::Iso(text),
        other => Timestamp::Other(other),
    }
}

/// Skip `window.YTD.*.partN =` and following whitespace so the next byte is
/// the JSON array. A file that already starts with `[` is left in place.
fn skip_ytd_assignment<R: BufRead>(reader: &mut R, label: &Path) -> Result<(), Error> {
    skip_ascii_whitespace(reader)?;
    if starts_with_byte(reader, b'[')? {
        return Ok(());
    }
    let mut found_eq = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Err(Error::ingest(format!(
                "missing window.YTD assignment in {}",
                label.display()
            )));
        }
        if !found_eq {
            if let Some(index) = buf.iter().position(|&byte| byte == b'=') {
                reader.consume(index + 1);
                found_eq = true;
                continue;
            }
            let len = buf.len();
            reader.consume(len);
            continue;
        }
        let skip = buf
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        if skip == buf.len() {
            reader.consume(skip);
            continue;
        }
        reader.consume(skip);
        return Ok(());
    }
}

fn skip_ascii_whitespace<R: BufRead>(reader: &mut R) -> Result<(), Error> {
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(());
        }
        let skip = buf
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        if skip == 0 {
            return Ok(());
        }
        if skip == buf.len() {
            reader.consume(skip);
            continue;
        }
        reader.consume(skip);
        return Ok(());
    }
}

fn starts_with_byte<R: BufRead>(reader: &mut R, want: u8) -> Result<bool, Error> {
    let buf = reader.fill_buf()?;
    Ok(buf.first().copied() == Some(want))
}
