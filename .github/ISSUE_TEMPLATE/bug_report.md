---
name: Bug report
about: Something produced wrong output, crashed, or didn't match FFmpeg
title: "[bug] "
labels: bug
---

<!--
For SECURITY issues (a crash/hang/memory bug on untrusted input), do NOT file
here — report privately per SECURITY.md.
-->

## What happened

A clear description of the bug.

## Command / code to reproduce

```
# the exact ffmpeg/ffprobe command, or a minimal code snippet
```

- Input file/URL (attach a minimal sample if you can):
- Does upstream FFmpeg handle the same input/command correctly? (yes / no / unknown)

## Expected vs actual

- **Expected:**
- **Actual:** (paste the error, wrong output, or a description; include the diff
  vs FFmpeg if relevant)

## Environment

- `remade_ffmpeg_rs` version / commit:
- Built with which features (`https`, `h264-asm`, `h264-openh264`, default …):
- OS + architecture:
- Rust / toolchain (`rustc -V`):
