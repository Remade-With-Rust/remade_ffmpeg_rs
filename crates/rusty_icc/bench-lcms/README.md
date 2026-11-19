# bench-lcms: rusty_icc against LittleCMS 2

Tooling, not shipped (outside the crate's `include` list; its own cargo
workspace). It links the system `liblcms2` through a few `extern "C"`
declarations (`src/lcms.rs`), because lcms2 is the oracle.

- `run-accuracy.sh`: grid comparison (`results/accuracy.md`) and
  black-point detection (`results/blackpoints.md`).
- `run-speed.sh [CPU] [PAIRS] [SIDE] [CASES]`: side-by-side timing
  (`results/speed.md`).

Both run in WSL2/Linux with `liblcms2-dev` installed. `sync-wsl.sh` copies the
crate into the native filesystem first (`/root/iccbench/rusty_icc`; cargo over
the 9p `/mnt` mount hangs) and rewrites the one workspace-inherited
dev-dependency. Coated FOGRA39 is read from `RUSTY_ICC_FOGRA39` (default
`/root/iccbench/CoatedFOGRA39.icc`) and never committed.

```sh
MSYS_NO_PATHCONV=1 wsl -e bash /mnt/e/.../crates/rusty_icc/bench-lcms/run-accuracy.sh
MSYS_NO_PATHCONV=1 wsl -e bash /mnt/e/.../crates/rusty_icc/bench-lcms/run-speed.sh 4 21 4096
```

The tables and method are in the crate README, section "Against LittleCMS 2".
