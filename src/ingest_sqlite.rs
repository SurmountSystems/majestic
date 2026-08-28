//! Read-only ingest of grok-oss `session_docs` sqlite.
//!
//! This module is an ingest reader. The memex store is the mmap archive, not
//! sqlite. Do not open `grok_oss.db` as chat transcripts.

use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

use crate::Error;
use crate::schema::{
    Conversation, ConversationItem, ExtraMap, JsonAtom, Response, ResponseItem, Timestamp,
};

/// True when `session_docs` exists. False on missing file, not-sqlite, or error.
pub(crate) fn has_session_docs_table(path: &Path) -> bool {
    let Ok(conn) = open_ro(path) else {
        return false;
    };
    table_exists(&conn, "session_docs").unwrap_or(false)
}

pub(crate) fn for_each_session_doc(
    path: &Path,
    mut each: impl FnMut(ConversationItem) -> Result<(), Error>,
) -> Result<(), Error> {
    let conn = open_ro(path)?;
    if !table_exists(&conn, "session_docs")? {
        return Err(Error::ingest(format!(
            "sqlite file {} has no session_docs table",
            path.display()
        )));
    }
    let columns = session_docs_columns(&conn)?;
    if !columns.iter().any(|name| name == "session_id") {
        return Err(Error::ingest(
            "session_docs has no session_id column; pass a grok-oss session index",
        ));
    }
    let quoted = columns
        .iter()
        .map(|name| quote_ident(name))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {quoted} FROM session_docs");
    let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
    let mut rows = stmt.query([]).map_err(sqlite_err)?;
    while let Some(row) = rows.next().map_err(sqlite_err)? {
        let Some(item) = row_to_item(row, &columns)? else {
            continue;
        };
        each(item)?;
    }
    Ok(())
}

fn open_ro(path: &Path) -> Result<Connection, Error> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|error| {
        Error::ingest(format!(
            "failed to open sqlite {} read-only: {error}",
            path.display()
        ))
    })
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, Error> {
    let mut stmt = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1")
        .map_err(sqlite_err)?;
    stmt.exists([name]).map_err(sqlite_err)
}

fn session_docs_columns(conn: &Connection) -> Result<Vec<String>, Error> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(session_docs)")
        .map_err(sqlite_err)?;
    let mut rows = stmt.query([]).map_err(sqlite_err)?;
    let mut names = Vec::new();
    while let Some(row) = rows.next().map_err(sqlite_err)? {
        let name: String = row.get(1).map_err(sqlite_err)?;
        if !name.is_empty() {
            names.push(name);
        }
    }
    Ok(names)
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn row_to_item(
    row: &rusqlite::Row<'_>,
    columns: &[String],
) -> Result<Option<ConversationItem>, Error> {
    let mut extra = ExtraMap::new();
    let mut session_id = None;
    let mut title = None;
    let mut content = None;
    let mut content_hash = None;
    let mut updated = None;
    for (index, name) in columns.iter().enumerate() {
        let value = row.get_ref(index).map_err(sqlite_err)?;
        match name.as_str() {
            "session_id" => session_id = value_to_string(value),
            "title" => title = value_to_string(value),
            "content" => content = value_to_string(value),
            "content_hash" => {
                if let Some(hash) = value_to_string(value) {
                    extra.insert("content_hash".to_owned(), JsonAtom::String(hash.clone()));
                    content_hash = Some(hash);
                }
            }
            "cwd" => {
                if let Some(cwd) = value_to_string(value) {
                    extra.insert("cwd".to_owned(), JsonAtom::String(cwd));
                }
            }
            "updated_at" => updated = value_to_timestamp(value),
            other => {
                if let Some(atom) = value_to_atom(value) {
                    extra.insert(other.to_owned(), atom);
                }
            }
        }
    }
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let response_id = content_hash.clone().unwrap_or_else(|| session_id.clone());
    let title = title.filter(|text| !text.is_empty());
    Ok(Some(ConversationItem {
        conversation: Conversation {
            id: Some(session_id.clone()),
            title,
            create_time: updated.clone(),
            modify_time: updated.clone(),
            extra,
            ..Default::default()
        },
        responses: vec![ResponseItem {
            response: Response {
                _id: Some(response_id),
                conversation_id: Some(session_id),
                message: content.map(JsonAtom::String),
                create_time: updated,
                extra: ExtraMap::new(),
                ..Default::default()
            },
            share_link: None,
            extra: ExtraMap::new(),
        }],
        extra: ExtraMap::new(),
    }))
}

fn value_to_string(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Text(text) => {
            let text = String::from_utf8_lossy(text).into_owned();
            if text.is_empty() { None } else { Some(text) }
        }
        ValueRef::Integer(value) => Some(value.to_string()),
        ValueRef::Real(value) => Some(value.to_string()),
        ValueRef::Blob(bytes) => {
            let text = String::from_utf8_lossy(bytes).into_owned();
            if text.is_empty() { None } else { Some(text) }
        }
        ValueRef::Null => None,
    }
}

fn value_to_timestamp(value: ValueRef<'_>) -> Option<Timestamp> {
    match value {
        ValueRef::Text(text) => {
            let text = String::from_utf8_lossy(text);
            if text.is_empty() {
                None
            } else {
                Some(Timestamp::Iso(text.into_owned()))
            }
        }
        ValueRef::Integer(value) => Some(Timestamp::Other(JsonAtom::I64(value))),
        ValueRef::Real(value) => Some(Timestamp::Other(JsonAtom::F64(value))),
        ValueRef::Null | ValueRef::Blob(_) => None,
    }
}

fn value_to_atom(value: ValueRef<'_>) -> Option<JsonAtom> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(value) => Some(JsonAtom::I64(value)),
        ValueRef::Real(value) => Some(JsonAtom::F64(value)),
        ValueRef::Text(text) => Some(JsonAtom::String(String::from_utf8_lossy(text).into_owned())),
        ValueRef::Blob(_) => None,
    }
}

fn sqlite_err(error: rusqlite::Error) -> Error {
    Error::ingest(format!("sqlite: {error}"))
}
