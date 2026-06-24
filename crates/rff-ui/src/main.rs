//! `rff-ui` — a minimal Dioxus front-end.
//!
//! Status: **scaffold**. It renders the engine's capabilities (the codecs and
//! formats that are registered) so there's a visible, runnable surface from day
//! one. As the engine grows, this becomes the transcode/probe UI — and because
//! Dioxus targets web, PWA, desktop and mobile from one codebase, the same
//! component tree ships everywhere.
//!
//! Run on desktop:  `cargo run -p rff-ui`
//! Web/mobile:      use the `dx` CLI (`dioxus-cli`) with the matching feature.

use dioxus::prelude::*;
use rff::Engine;

fn main() {
    dioxus::launch(app);
}

fn app() -> Element {
    // The engine is cheap to construct (just registration), so build it per
    // render for now; a future version will hold engine state and drive jobs.
    let engine = Engine::new();

    let codec_lines: Vec<String> = {
        let mut codecs: Vec<_> = engine.codecs.iter().collect();
        codecs.sort_by_key(|c| c.name);
        codecs
            .into_iter()
            .map(|c| {
                let caps = match (c.can_decode(), c.can_encode()) {
                    (true, true) => "decode+encode",
                    (true, false) => "decode",
                    (false, true) => "encode",
                    (false, false) => "none",
                };
                format!("{} ({}) — {} [{caps}]", c.name, c.media_type, c.long_name)
            })
            .collect()
    };

    let format_lines: Vec<String> = {
        let mut formats: Vec<_> = engine.formats.iter().collect();
        formats.sort_by_key(|f| f.name);
        formats
            .into_iter()
            .map(|f| format!("{} — {}", f.name, f.long_name))
            .collect()
    };

    rsx! {
        div {
            style: "font-family: system-ui, sans-serif; max-width: 760px; margin: 2rem auto; padding: 0 1rem;",
            h1 { "remade_ffmpeg_rs" }
            p {
                style: "color: #555;",
                "A clean-room, permissively-licensed media toolkit in Rust. Remade With Rust, by Mata Network."
            }

            h2 { "Codecs" }
            ul {
                for line in codec_lines {
                    li { "{line}" }
                }
            }

            h2 { "Formats" }
            ul {
                for line in format_lines {
                    li { "{line}" }
                }
            }
        }
    }
}
