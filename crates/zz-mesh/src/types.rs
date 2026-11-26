//! Canonical identifiers and bounded binary values shared by all adapters.
use bincode::Options;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_FIELD: usize = 128 * 1024;
pub const MAX_TARGETS: usize = 128;
pub const MAX_BATCH: usize = 32;
pub const RECEIPTS: u64 = 128;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("stale owner, incarnation, revision or transfer phase")]
    Fenced,
    #[error("missing {0}")]
    Missing(String),
    #[error("backpressure: {0}")]
    Busy(&'static str),
    #[error("access denied")]
    Denied,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("codec: {0}")]
    Codec(#[from] bincode::Error),
}
pub fn invalid(reason: impl Into<String>) -> Error {
    Error::Invalid(reason.into())
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(bincode::DefaultOptions::new()
        .with_little_endian()
        .with_varint_encoding()
        .with_limit(MAX_FRAME as u64)
        .serialize(value)?)
}
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_FRAME {
        return Err(Error::Busy("frame length"));
    }
    Ok(bincode::DefaultOptions::new()
        .with_little_endian()
        .with_varint_encoding()
        .with_limit(MAX_FRAME as u64)
        .reject_trailing_bytes()
        .deserialize(bytes)?)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Id(pub [u8; 16]);
impl Id {
    pub fn new() -> Self {
        Self(*uuid::Uuid::new_v4().as_bytes())
    }
    pub fn numbered(n: u128) -> Self {
        Self(n.to_be_bytes())
    }
    pub fn child(self, n: u64) -> Self {
        let mut b = self.0.to_vec();
        b.extend(n.to_le_bytes());
        Self(digest(&b).0[..16].try_into().unwrap())
    }
}
impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}
impl std::str::FromStr for Id {
    type Err = uuid::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(*uuid::Uuid::parse_str(s)?.as_bytes()))
    }
}
impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        uuid::Uuid::from_bytes(self.0).fmt(f)
    }
}
impl rusqlite::ToSql for Id {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Blob(&self.0)))
    }
}
impl FromSql for Id {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        Ok(Self(
            v.as_blob()?
                .try_into()
                .map_err(|_| FromSqlError::InvalidType)?,
        ))
    }
}
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Digest(pub [u8; 32]);
pub fn digest(data: &[u8]) -> Digest {
    Digest(
        ring::digest::digest(&ring::digest::SHA256, data)
            .as_ref()
            .try_into()
            .unwrap(),
    )
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Worker {
    pub slot: u32,
    pub incarnation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authority {
    pub worker: Worker,
    pub generation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub version: u16,
    pub bytes: Vec<u8>,
}
impl Field {
    pub fn new<T: Serialize>(version: u16, value: &T) -> Result<Self> {
        let bytes = encode(value)?;
        if bytes.len() > MAX_FIELD {
            return Err(Error::Busy("component bytes"));
        }
        Ok(Self { version, bytes })
    }
    pub fn get<T: DeserializeOwned>(&self, version: u16) -> Result<T> {
        if self.version != version {
            return Err(invalid(format!(
                "expected schema version {version}, got {}",
                self.version
            )));
        }
        decode(&self.bytes)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: Id,
    pub group: Id,
    pub revision: u64,
    pub deleted: bool,
    pub fields: BTreeMap<u32, Field>,
}
impl Record {
    pub fn new(id: Id, group: Id) -> Self {
        Self {
            id,
            group,
            revision: 0,
            deleted: false,
            fields: BTreeMap::new(),
        }
    }
    pub fn put<T: Serialize>(&mut self, schema: u32, version: u16, value: &T) -> Result<()> {
        self.fields.insert(schema, Field::new(version, value)?);
        Ok(())
    }
    pub fn get<T: DeserializeOwned>(&self, schema: u32, version: u16) -> Result<T> {
        self.fields
            .get(&schema)
            .ok_or_else(|| Error::Missing(format!("schema {schema} on {}", self.id)))?
            .get(version)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reference {
    RequiredLocal(Id),
    OptionalLocal(Id),
    Remote(Id),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionKey {
    pub session: Id,
    pub seq: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Intent {
    pub key: ActionKey,
    pub group: Id,
    pub kind: u32,
    pub version: u16,
    pub after: Option<u64>,
    pub bytes: Vec<u8>,
}
impl Intent {
    pub fn typed<A: crate::Action>(key: ActionKey, group: Id, action: &A) -> Result<Self> {
        Ok(Self {
            key,
            group,
            kind: A::ID,
            version: A::VERSION,
            after: None,
            bytes: encode(action)?,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Committed(Vec<u8>),
    Rejected(String),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub key: ActionKey,
    pub revision: u64,
    pub outcome: Outcome,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Decided(Receipt),
    Expected(u64),
    Resync(u64),
    Retry(String),
}

/// Replaceable input with an independent sequence and a bounded server-time lease.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Input {
    pub session: Id,
    pub sequence: u64,
    pub at_ms: u64,
    pub axes: [i16; 3],
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputAck {
    pub sequence: u64,
    pub revision: u64,
}
