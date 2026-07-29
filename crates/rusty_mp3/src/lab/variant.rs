//! Variant registry — the "modify a brick on the fly" mechanism.
//!
//! A *variant* is a named parameter preset (or alternate implementation) of one
//! brick. Each runnable brick owns a static table of variants; an experiment
//! selects one by name and may override individual params from the CLI without
//! recompiling. To add a knob, append a row to the brick's table — that's the
//! whole ceremony.
//!
//! Generic over the brick's parameter type `P`, so every future brick reuses this
//! same lookup.

/// A named preset of a brick's parameters.
#[derive(Debug, Clone, Copy)]
pub struct Preset<P: Copy + 'static> {
    pub name: &'static str,
    /// One-line note on what this variant is testing.
    pub blurb: &'static str,
    pub params: P,
}

/// Find a preset's params by name within a brick's table.
pub fn find<P: Copy>(table: &'static [Preset<P>], name: &str) -> Option<P> {
    table
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .map(|p| p.params)
}

/// Comma-joined variant names, for error messages and `--list`.
pub fn names<P: Copy>(table: &'static [Preset<P>]) -> String {
    table.iter().map(|p| p.name).collect::<Vec<_>>().join(", ")
}

/// Render a brick's variant table for the CLI.
pub fn list<P: Copy>(table: &'static [Preset<P>]) -> String {
    let mut s = String::new();
    for p in table {
        s.push_str(&format!("  {:<10} {}\n", p.name, p.blurb));
    }
    s
}
