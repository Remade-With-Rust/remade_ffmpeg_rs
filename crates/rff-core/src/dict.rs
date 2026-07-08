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

    /// Parse a bitrate-valued option with FFmpeg's `k`/`M` suffixes
    /// (`128k` → 128 000, `1M` → 1 000 000; a bare number is taken as-is).
    /// Case-insensitive; returns `None` if absent or malformed.
    pub fn get_bitrate(&self, key: &str) -> Option<i64> {
        let v = self.get(key)?.trim();
        let (num, mult) = match v.chars().last() {
            Some('k') | Some('K') => (&v[..v.len() - 1], 1_000i64),
            Some('m') | Some('M') => (&v[..v.len() - 1], 1_000_000i64),
            _ => (v, 1),
        };
        num.trim().parse::<f64>().ok().map(|n| (n * mult as f64) as i64)
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

#[cfg(test)]
mod tests {
    use super::Dictionary;

    #[test]
    fn get_bitrate_parses_suffixes() {
        let mut d = Dictionary::new();
        d.set("b", "128k");
        assert_eq!(d.get_bitrate("b"), Some(128_000));
        d.set("b", "1M");
        assert_eq!(d.get_bitrate("b"), Some(1_000_000));
        d.set("b", "24000");
        assert_eq!(d.get_bitrate("b"), Some(24_000));
        d.set("b", "1.5M");
        assert_eq!(d.get_bitrate("b"), Some(1_500_000));
        d.set("b", "96K");
        assert_eq!(d.get_bitrate("b"), Some(96_000));
        assert_eq!(d.get_bitrate("missing"), None);
        d.set("b", "garbage");
        assert_eq!(d.get_bitrate("b"), None);
    }
}
