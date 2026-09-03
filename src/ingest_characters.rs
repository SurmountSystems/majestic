//! Ingest `characters/{id}.toml` into conversation records.
//!
//! Explicit path plus `--service` and `--account`. Home scan does not pick up
//! random TOML. JSON character files are ignored. Do not ingest `Cargo.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::schema::{Conversation, ConversationItem, ExtraMap, JsonAtom, Response, ResponseItem};

/// One TOML file, a `characters/` directory, or a dir that contains `characters/*.toml`.
pub(crate) fn looks_like_characters_input(path: &Path) -> Result<bool, Error> {
    Ok(!collect_character_toml(path)?.is_empty())
}

pub(crate) fn is_toml_file(path: &Path) -> bool {
    if !path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
    {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    !name.eq_ignore_ascii_case("Cargo.toml")
}

fn is_characters_dir(path: &Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "characters")
}

/// TOML character files under `path`. Never JSON. Never `Cargo.toml`.
pub(crate) fn collect_character_toml(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let meta = match fs::metadata(root) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    if meta.is_file() {
        if is_toml_file(root) {
            return Ok(vec![root.to_path_buf()]);
        }
        return Ok(Vec::new());
    }
    if !meta.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    if is_characters_dir(root) {
        collect_toml_in_dir(root, &mut files)?;
    } else {
        collect_toml_in_dir(&root.join("characters"), &mut files)?;
    }
    files.sort();
    Ok(files)
}

fn collect_toml_in_dir(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if is_toml_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

/// Parse each character TOML file as one conversation.
pub(crate) fn for_each_character(
    root: &Path,
    mut each: impl FnMut(ConversationItem) -> Result<(), Error>,
) -> Result<(), Error> {
    let files = collect_character_toml(root)?;
    if files.is_empty() {
        return Err(Error::ingest(format!(
            "no character TOML under {}",
            root.display()
        )));
    }
    for path in files {
        each(character_item(&path)?)?;
    }
    Ok(())
}

fn character_item(path: &Path) -> Result<ConversationItem, Error> {
    let text = fs::read_to_string(path)?;
    let table: toml::Table = toml::from_str(&text)
        .map_err(|error| Error::ingest(format!("failed to parse {}: {error}", path.display())))?;
    let mut extra = ExtraMap::new();
    let mut id = None;
    let mut title = None;
    let mut keys_text = None;
    let mut value = None;
    for (key, toml_value) in table {
        let atom = toml_to_atom(toml_value);
        match key.as_str() {
            "id" => {
                id = atom_plain_string(&atom);
                extra.insert(key, atom);
            }
            "title" | "name" => {
                if title.is_none() || key == "title" {
                    title = atom_plain_string(&atom);
                }
                extra.insert(key, atom);
            }
            "keys" => {
                keys_text = keys_as_text(&atom);
                extra.insert(key, atom);
            }
            "value" => {
                if atom_is_body(&atom) {
                    value = Some(atom);
                } else {
                    extra.insert(key, atom);
                }
            }
            _ => {
                extra.insert(key, atom);
            }
        }
    }
    let title = title.or_else(|| file_stem_title(path));
    let id = id
        .or_else(|| title.clone())
        .or_else(|| file_stem_title(path))
        .unwrap_or_else(|| path.display().to_string());
    let mut responses = Vec::new();
    if let Some(message) = value {
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
    Ok(ConversationItem {
        conversation: Conversation {
            id: Some(id),
            title,
            summary: keys_text,
            extra,
            ..Default::default()
        },
        responses,
        extra: ExtraMap::new(),
    })
}

fn file_stem_title(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_owned)
}

fn toml_to_atom(value: toml::Value) -> JsonAtom {
    match value {
        toml::Value::String(text) => JsonAtom::String(text),
        toml::Value::Integer(value) => JsonAtom::I64(value),
        toml::Value::Float(value) => JsonAtom::F64(value),
        toml::Value::Boolean(value) => JsonAtom::Bool(value),
        toml::Value::Datetime(value) => JsonAtom::String(value.to_string()),
        toml::Value::Array(items) => JsonAtom::Array(items.into_iter().map(toml_to_atom).collect()),
        toml::Value::Table(table) => JsonAtom::Object(
            table
                .into_iter()
                .map(|(key, value)| (key, toml_to_atom(value)))
                .collect(),
        ),
    }
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
