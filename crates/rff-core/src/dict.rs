//! A small ordered string→string map for codec/format options — the role
//! `AVDictionary` plays in FFmpeg (e.g. `-crf 23`, `-preset fast`). Ordered so
//! that iteration (and any serialized form) is deterministic.

use std::collections::BTreeMap;

/// An ordered collection of `key = value` options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dictionary {
    entries: BTreeMap<String, String>,
}

impl Dictionary {
    pub fn new() -> Dictionary {
        Dictionary::default()
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.entries.insert(key.into(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    /// Parse an integer-valued option, returning `None` if absent or malformed.
    pub fn get_int(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.parse().ok())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}
