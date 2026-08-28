//! Known Grok export fields plus a leftover map so unknown keys round-trip.
//!
//! Primary types are these structs, not `serde_json::Value`. Leftover keys live
//! in [`ExtraMap`] as [`JsonAtom`]. BSON `$date` wrappers stay representable.

use std::collections::BTreeMap;
use std::fmt;

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Unknown JSON keys on a typed object, keyed as they appeared.
pub type ExtraMap = BTreeMap<String, JsonAtom>;

/// JSON value that rkyv can archive. Integers prefer `i64`, then `u64`, else `f64`.
#[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
#[rkyv(derive(Debug, PartialEq))]
#[rkyv(serialize_bounds(
    __S: rkyv::ser::Writer + rkyv::ser::Allocator,
    __S::Error: rkyv::rancor::Source,
))]
#[rkyv(deserialize_bounds(__D::Error: rkyv::rancor::Source))]
#[rkyv(bytecheck(bounds(__C: rkyv::validation::ArchiveContext)))]
pub enum JsonAtom {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    Array(#[rkyv(omit_bounds)] Vec<JsonAtom>),
    Object(#[rkyv(omit_bounds)] ExtraMap),
}

impl Serialize for JsonAtom {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::I64(value) => serializer.serialize_i64(*value),
            Self::U64(value) => serializer.serialize_u64(*value),
            Self::F64(value) => serializer.serialize_f64(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => serde::Serialize::serialize(values, serializer),
            Self::Object(values) => serde::Serialize::serialize(values, serializer),
        }
    }
}

impl<'de> Deserialize<'de> for JsonAtom {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonAtomVisitor)
    }
}

struct JsonAtomVisitor;

impl<'de> Visitor<'de> for JsonAtomVisitor {
    type Value = JsonAtom;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<JsonAtom, E> {
        Ok(JsonAtom::Bool(value))
    }

    fn visit_i8<E: de::Error>(self, value: i8) -> Result<JsonAtom, E> {
        self.visit_i64(i64::from(value))
    }

    fn visit_i16<E: de::Error>(self, value: i16) -> Result<JsonAtom, E> {
        self.visit_i64(i64::from(value))
    }

    fn visit_i32<E: de::Error>(self, value: i32) -> Result<JsonAtom, E> {
        self.visit_i64(i64::from(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<JsonAtom, E> {
        Ok(JsonAtom::I64(value))
    }

    fn visit_u8<E: de::Error>(self, value: u8) -> Result<JsonAtom, E> {
        self.visit_u64(u64::from(value))
    }

    fn visit_u16<E: de::Error>(self, value: u16) -> Result<JsonAtom, E> {
        self.visit_u64(u64::from(value))
    }

    fn visit_u32<E: de::Error>(self, value: u32) -> Result<JsonAtom, E> {
        self.visit_u64(u64::from(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<JsonAtom, E> {
        if let Ok(signed) = i64::try_from(value) {
            Ok(JsonAtom::I64(signed))
        } else {
            Ok(JsonAtom::U64(value))
        }
    }

    fn visit_f32<E: de::Error>(self, value: f32) -> Result<JsonAtom, E> {
        self.visit_f64(f64::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<JsonAtom, E> {
        Ok(JsonAtom::F64(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<JsonAtom, E> {
        Ok(JsonAtom::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<JsonAtom, E> {
        Ok(JsonAtom::String(value))
    }

    fn visit_none<E: de::Error>(self) -> Result<JsonAtom, E> {
        Ok(JsonAtom::Null)
    }

    fn visit_unit<E: de::Error>(self) -> Result<JsonAtom, E> {
        Ok(JsonAtom::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<JsonAtom, D::Error> {
        JsonAtom::deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<JsonAtom, A::Error> {
        let mut values = Vec::new();
        if let Some(hint) = seq.size_hint() {
            values.reserve(hint);
        }
        while let Some(value) = seq.next_element()? {
            values.push(value);
        }
        Ok(JsonAtom::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<JsonAtom, A::Error> {
        let mut values = ExtraMap::new();
        while let Some((key, value)) = map.next_entry()? {
            values.insert(key, value);
        }
        Ok(JsonAtom::Object(values))
    }
}

/// Conversation ISO-8601 string, BSON `$date` wrapper, or leftover JSON.
#[derive(
    Archive, RkyvSerialize, RkyvDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq,
)]
#[rkyv(derive(Debug, PartialEq))]
#[serde(untagged)]
pub enum Timestamp {
    Iso(String),
    BsonDate {
        #[serde(rename = "$date")]
        date: JsonAtom,
    },
    Other(JsonAtom),
}

/// `prod-grok-backend.json` root. Unknown top-level keys go in `extra`.
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Default,
)]
#[rkyv(derive(Debug, PartialEq))]
pub struct BackendExport {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversations: Vec<ConversationItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media_posts: Vec<JsonAtom>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<JsonAtom>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<JsonAtom>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: ExtraMap,
}

/// Wrapper `{ "conversation": {...}, "responses": [...] }`.
#[derive(
    Archive, RkyvSerialize, RkyvDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq,
)]
#[rkyv(derive(Debug, PartialEq))]
pub struct ConversationItem {
    pub conversation: Conversation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub responses: Vec<ResponseItem>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: ExtraMap,
}

/// One conversation record. Named fields are optional so absence is not invented.
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Default,
)]
#[rkyv(derive(Debug, PartialEq))]
pub struct Conversation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anon_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_ids: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_time: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_types: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modify_time: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_asset_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_with_team: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_with_user_ids: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starred: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_result_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_user_id: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: ExtraMap,
}

/// Wrapper `{ "response": {...}, "share_link": ... }`.
#[derive(
    Archive, RkyvSerialize, RkyvDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq,
)]
#[rkyv(derive(Debug, PartialEq))]
pub struct ResponseItem {
    pub response: Response,
    #[serde(default)]
    pub share_link: Option<JsonAtom>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: ExtraMap,
}

/// One response record. Structured leftover fields stay [`JsonAtom`].
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Default,
)]
#[rkyv(derive(Debug, PartialEq))]
pub struct Response {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub _id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_time: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_web_search_results: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_image_urls: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_attachments: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search_results: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_trace: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_start_time: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_end_time: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_thinking_traces: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xpost_ids: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webpage_urls: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_attachments_json: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonAtom>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: ExtraMap,
}

/// Auth sibling file (`api_keys`, sessions, and the rest). Do not log secret values.
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Default,
)]
#[rkyv(derive(Debug, PartialEq))]
pub struct AuthFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_keys: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitations: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub management_api_keys: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_acls: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teams: Option<JsonAtom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<JsonAtom>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: ExtraMap,
}

/// Billing sibling file. `balance_map` is leftover JSON.
#[derive(
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Default,
)]
#[rkyv(derive(Debug, PartialEq))]
pub struct BillingFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance_map: Option<JsonAtom>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: ExtraMap,
}
