//! JSON rendering of a [`Plan`] and the format matrix.
//!
//! Hand-written rather than serde-derived so the crate stays dependency-free:
//! this is the payload a web UI or an HTTP handler consumes, and it should not
//! oblige every consumer of `rff-targets` to compile serde.

use std::fmt::Write;

use crate::{Action, FormatSupport, Plan, Target};

/// Escape a string into a JSON string literal, quotes included.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Control characters have no literal form in JSON.
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn array(items: impl IntoIterator<Item = String>) -> String {
    format!("[{}]", items.into_iter().collect::<Vec<_>>().join(","))
}

impl Plan {
    /// Render the whole plan as JSON: the source streams, then every target.
    ///
    /// Shape (stable; new fields may be added):
    /// ```json
    /// {"source_format":"mp4",
    ///  "source":[{"index":0,"type":"video","codec":"h264"}],
    ///  "targets":[{"format":"mp4","extension":"mp4","extensions":["mp4"],
    ///              "long_name":"...","kind":"video","fidelity":"copy",
    ///              "summary":"h264 copy (copy)","args":["-c:v","copy"],
    ///              "notes":[],
    ///              "streams":[{"input_index":0,"type":"video","from":"h264",
    ///                          "action":"copy"}]}]}
    /// ```
    pub fn to_json(&self) -> String {
        let source = array(self.source.iter().map(|s| {
            format!(
                r#"{{"index":{},"type":{},"codec":{}}}"#,
                s.index,
                quote(&s.media_type.to_string()),
                quote(s.codec_id.name())
            )
        }));
        let targets = array(self.targets.iter().map(Target::to_json));
        let format = match &self.source_format {
            Some(f) => quote(f),
            None => "null".to_string(),
        };
        format!(r#"{{"source_format":{format},"source":{source},"targets":{targets}}}"#)
    }
}

impl Target {
    /// Render one target as a JSON object.
    pub fn to_json(&self) -> String {
        let streams = array(self.streams.iter().map(|s| {
            let action = match s.action {
                Action::Copy => r#""action":"copy""#.to_string(),
                Action::Transcode { to, lossy } => format!(
                    r#""action":"transcode","to":{},"lossy":{lossy}"#,
                    quote(to.name())
                ),
                Action::Drop(reason) => format!(
                    r#""action":"drop","reason":{},"detail":{}"#,
                    quote(reason.name()),
                    quote(&reason.to_string())
                ),
            };
            format!(
                r#"{{"input_index":{},"type":{},"from":{},{action}}}"#,
                s.input_index,
                quote(&s.media_type.to_string()),
                quote(s.from.name())
            )
        }));
        format!(
            concat!(
                r#"{{"format":{},"long_name":{},"extension":{},"extensions":{},"#,
                r#""kind":{},"fidelity":{},"summary":{},"args":{},"notes":{},"streams":{}}}"#
            ),
            quote(self.format),
            quote(self.long_name),
            quote(self.extension),
            array(self.extensions.iter().map(|e| quote(e))),
            quote(self.kind.name()),
            quote(self.fidelity.name()),
            quote(&self.summary()),
            array(self.args.iter().map(|a| quote(a))),
            array(self.notes.iter().map(|n| quote(n))),
            streams,
        )
    }
}

impl FormatSupport {
    /// Render one matrix row as a JSON object.
    pub fn to_json(&self) -> String {
        let codecs = |ids: &[rff_core::CodecId]| array(ids.iter().map(|c| quote(c.name())));
        let max = match self.max_streams {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        format!(
            concat!(
                r#"{{"format":{},"long_name":{},"extensions":{},"read":{},"write":{},"#,
                r#""still_image":{},"max_streams":{},"video":{},"audio":{},"subtitle":{}}}"#
            ),
            quote(self.format),
            quote(self.long_name),
            array(self.extensions.iter().map(|e| quote(e))),
            self.can_read,
            self.can_write,
            self.still_image,
            max,
            codecs(&self.video),
            codecs(&self.audio),
            codecs(&self.subtitle),
        )
    }
}

/// Render a whole [`format_matrix`](crate::format_matrix) as a JSON array.
pub fn matrix_to_json(rows: &[FormatSupport]) -> String {
    array(rows.iter().map(FormatSupport::to_json))
}
