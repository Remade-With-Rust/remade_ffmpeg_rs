# rff-format-ass

ASS/SSA (Advanced SubStation Alpha) subtitle demuxer for
[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs).

Parses `[Events]` `Dialogue:` lines into plain-text cues (override tags
stripped, `\N` resolved), yielding the same packet contract as the SubRip and
WebVTT crates — so `.ass → .srt`/`.vtt`, or into Matroska as `S_TEXT/UTF8`,
is a stream copy. Demux only: styled ASS output would fabricate a style;
write `.srt`/`.vtt` instead.
