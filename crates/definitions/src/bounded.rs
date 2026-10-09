//! Shared JSON/YAML decoding budget and duplicate-key rejection before DTO decoding.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};
use std::{cell::Cell, fmt};

const MAX_NODES: usize = 65_536;
const MAX_DEPTH: usize = 32;

pub(crate) struct Document(pub Value);

impl<'de> Deserialize<'de> for Document {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Seed {
            remaining: &Cell::new(MAX_NODES),
            remaining_bytes: &Cell::new(crate::MAX_DOCUMENT_BYTES),
            depth: 0,
        }
        .deserialize(deserializer)
        .map(Self)
    }
}

struct Seed<'a> {
    remaining: &'a Cell<usize>,
    remaining_bytes: &'a Cell<usize>,
    depth: usize,
}

impl Seed<'_> {
    fn charge_string<E: de::Error>(&self, bytes: usize) -> Result<(), E> {
        let remaining = self
            .remaining_bytes
            .get()
            .checked_sub(bytes)
            .ok_or_else(|| E::custom("expanded document exceeds byte budget"))?;
        self.remaining_bytes.set(remaining);
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.depth > MAX_DEPTH || self.remaining.get() == 0 {
            return Err(de::Error::custom("document complexity limit exceeded"));
        }
        self.remaining.set(self.remaining.get() - 1);
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a bounded JSON-compatible value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        self.charge_string::<E>(v.len())?;
        Ok(Value::String(v.into()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        self.charge_string::<E>(v.len())?;
        Ok(Value::String(v))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(Seed {
            remaining: self.remaining,
            remaining_bytes: self.remaining_bytes,
            depth: self.depth + 1,
        })? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut items = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            self.charge_string::<A::Error>(key.len())?;
            if items.contains_key(&key) {
                return Err(de::Error::custom("duplicate mapping key"));
            }
            let value = map.next_value_seed(Seed {
                remaining: self.remaining,
                remaining_bytes: self.remaining_bytes,
                depth: self.depth + 1,
            })?;
            items.insert(key, value);
        }
        Ok(Value::Object(items))
    }
}
