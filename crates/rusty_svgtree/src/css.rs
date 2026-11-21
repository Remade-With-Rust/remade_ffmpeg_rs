//! A small CSS subset for SVG `<style>` sheets and `style` attributes.
//!
//! Selectors: `*`, type, `.class`, `#id`, `[attr]`, `[attr=v]`, `~=`, `|=`,
//! `^=`, `$=`, `*=`, `:first-child`, and the descendant (` `), child (`>`)
//! and adjacent-sibling (`+`) combinators, in comma-separated lists. Rules
//! are stably sorted by specificity, as `simplecss` (what usvg uses) does.
//! At-rules are skipped. Anything unparseable is skipped, never fatal.

use std::collections::HashMap;

/// One `name: value` declaration.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Declaration {
    pub name: String,
    pub value: String,
    pub important: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum AttrOp {
    Exists,
    Eq(String),
    Includes(String),
    DashMatch(String),
    Prefix(String),
    Suffix(String),
    Substring(String),
}

#[derive(Clone, Debug, PartialEq)]
enum SubSel {
    Attr(String, AttrOp),
    FirstChild,
    /// A pseudo-class we do not support: never matches.
    Never,
}

#[derive(Clone, Debug, PartialEq, Default)]
struct Compound {
    /// `None` = universal.
    tag: Option<String>,
    subs: Vec<SubSel>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Combinator {
    Descendant,
    Child,
    Adjacent,
}

/// A complex selector, stored right-to-left: `parts[0]` is the subject.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Selector {
    subject: Compound,
    /// `(combinator, compound)` pairs walking leftwards from the subject.
    ancestors: Vec<(Combinator, Compound)>,
}

/// A rule: one selector and its declarations.
#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub selector: Selector,
    pub declarations: Vec<Declaration>,
}

/// The element interface the matcher needs.
pub(crate) trait Element: Sized + Copy {
    fn local_name(&self) -> &str;
    fn attribute(&self, name: &str) -> Option<&str>;
    fn parent_element(&self) -> Option<Self>;
    fn prev_sibling_element(&self) -> Option<Self>;
}

impl Selector {
    fn specificity(&self) -> [u32; 3] {
        let mut s = [0u32; 3];
        let mut add = |c: &Compound| {
            if c.tag.is_some() {
                s[2] += 1;
            }
            for sub in &c.subs {
                match sub {
                    SubSel::Attr(name, AttrOp::Eq(_)) if name == "id" => s[0] += 1,
                    _ => s[1] += 1,
                }
            }
        };
        add(&self.subject);
        for (_, c) in &self.ancestors {
            add(c);
        }
        s
    }

    /// Whether the selector matches `e`. Every compound test spends one
    /// unit of `budget`; once it is exhausted nothing matches, which bounds
    /// the backtracking a hostile descendant-combinator chain can cause.
    pub(crate) fn matches<E: Element>(&self, e: E, budget: &mut u64) -> bool {
        if !compound_matches(&self.subject, e, budget) {
            return false;
        }
        match_from(&self.ancestors, e, budget)
    }
}

/// What an element must carry for a rule's subject compound to match: a
/// necessary condition, read off the selector once.
enum SubjectKey<'a> {
    Id(&'a str),
    Class(&'a str),
    Tag(&'a str),
    Any,
}

impl Selector {
    fn subject_key(&self) -> SubjectKey<'_> {
        let mut class = None;
        for sub in &self.subject.subs {
            match sub {
                SubSel::Attr(name, AttrOp::Eq(v)) if name == "id" => return SubjectKey::Id(v),
                SubSel::Attr(name, AttrOp::Includes(v)) if name == "class" => {
                    class = class.or(Some(v.as_str()));
                }
                _ => {}
            }
        }
        match (class, &self.subject.tag) {
            (Some(c), _) => SubjectKey::Class(c),
            (None, Some(t)) => SubjectKey::Tag(t),
            (None, None) => SubjectKey::Any,
        }
    }
}

/// The rules of a sheet, bucketed by what their subject compound requires of
/// an element (an `id`, else a class, else a tag name), so an element is
/// tested only against the rules it could match.
#[derive(Default)]
pub(crate) struct RuleIndex {
    by_id: HashMap<Box<str>, Vec<u32>>,
    by_class: HashMap<Box<str>, Vec<u32>>,
    by_tag: HashMap<Box<str>, Vec<u32>>,
    any: Vec<u32>,
}

/// Sheets shorter than this are matched rule by rule: looking an element up
/// in the index costs about as much as testing this many rules against it.
const INDEX_MIN_RULES: usize = 8;

impl RuleIndex {
    /// The index of a sheet long enough to gain from one.
    pub(crate) fn new(rules: &[Rule]) -> Option<Self> {
        (rules.len() >= INDEX_MIN_RULES).then(|| RuleIndex::build(rules))
    }

    fn build(rules: &[Rule]) -> Self {
        let mut ix = RuleIndex::default();
        for (i, rule) in rules.iter().enumerate() {
            // A sheet cannot hold 2^32 rules; one past that would simply
            // never be a candidate.
            let Ok(i) = u32::try_from(i) else { break };
            match rule.selector.subject_key() {
                SubjectKey::Id(v) => ix.by_id.entry(v.into()).or_default().push(i),
                SubjectKey::Class(v) => ix.by_class.entry(v.into()).or_default().push(i),
                SubjectKey::Tag(v) => ix.by_tag.entry(v.into()).or_default().push(i),
                SubjectKey::Any => ix.any.push(i),
            }
        }
        ix
    }

    /// The rules `e` could match, going by its tag name, `id` and `class`,
    /// ascending (rule order).
    fn candidates<E: Element>(&self, e: E, out: &mut Vec<u32>) {
        out.clear();
        let mut lists = 0;
        let mut add = |list: Option<&Vec<u32>>| {
            if let Some(list) = list {
                out.extend_from_slice(list);
                lists += 1;
            }
        };
        add(Some(&self.any).filter(|l| !l.is_empty()));
        if !self.by_tag.is_empty() {
            add(self.by_tag.get(e.local_name()));
        }
        if !self.by_id.is_empty() {
            if let Some(id) = e.attribute("id") {
                add(self.by_id.get(id));
            }
        }
        if !self.by_class.is_empty() {
            if let Some(class) = e.attribute("class") {
                for word in class.split_ascii_whitespace() {
                    add(self.by_class.get(word));
                }
            }
        }
        if lists > 1 {
            // Each list is ascending; a repeated class word repeats one.
            out.sort_unstable();
            out.dedup();
        }
    }
}

/// Calls `hit` for each rule that matches `e`, in rule order. With an
/// `index` (built from `rules`), only the rules it cannot rule out are
/// tested.
///
/// A rule ruled out by the index would have failed its subject compound,
/// which spends one unit of `budget` while any is left. The same units are
/// charged here, so matching runs out at exactly the rule where testing
/// every rule in turn runs out.
pub(crate) fn for_each_match<E: Element>(
    rules: &[Rule],
    index: Option<&RuleIndex>,
    scratch: &mut Vec<u32>,
    e: E,
    budget: &mut u64,
    mut hit: impl FnMut(&Rule),
) {
    let Some(index) = index else {
        for rule in rules {
            if rule.selector.matches(e, budget) {
                hit(rule);
            }
        }
        return;
    };
    index.candidates(e, scratch);
    let mut next = 0usize;
    for &i in scratch.iter() {
        let i = i as usize;
        *budget = budget.saturating_sub((i - next) as u64);
        if *budget == 0 {
            return;
        }
        next = i + 1;
        let Some(rule) = rules.get(i) else {
            return;
        };
        if rule.selector.matches(e, budget) {
            hit(rule);
        }
    }
    *budget = budget.saturating_sub(rules.len().saturating_sub(next) as u64);
}

fn match_from<E: Element>(rest: &[(Combinator, Compound)], e: E, budget: &mut u64) -> bool {
    let Some(((comb, comp), tail)) = rest.split_first() else {
        return true;
    };
    match comb {
        Combinator::Child => match e.parent_element() {
            Some(p) => compound_matches(comp, p, budget) && match_from(tail, p, budget),
            None => false,
        },
        Combinator::Adjacent => match e.prev_sibling_element() {
            Some(p) => compound_matches(comp, p, budget) && match_from(tail, p, budget),
            None => false,
        },
        Combinator::Descendant => {
            // Bounded by the document depth, which the tree builder limits.
            let mut cur = e.parent_element();
            while let Some(p) = cur {
                if compound_matches(comp, p, budget) && match_from(tail, p, budget) {
                    return true;
                }
                cur = p.parent_element();
            }
            false
        }
    }
}

fn compound_matches<E: Element>(c: &Compound, e: E, budget: &mut u64) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    if let Some(tag) = &c.tag {
        if e.local_name() != tag {
            return false;
        }
    }
    c.subs.iter().all(|s| match s {
        SubSel::FirstChild => e.prev_sibling_element().is_none(),
        SubSel::Never => false,
        SubSel::Attr(name, op) => {
            let Some(v) = e.attribute(name) else {
                return false;
            };
            match op {
                AttrOp::Exists => true,
                AttrOp::Eq(x) => v == x,
                AttrOp::Includes(x) => v.split_ascii_whitespace().any(|w| w == x),
                AttrOp::DashMatch(x) => v == x || v.starts_with(&format!("{x}-")),
                AttrOp::Prefix(x) => !x.is_empty() && v.starts_with(x.as_str()),
                AttrOp::Suffix(x) => !x.is_empty() && v.ends_with(x.as_str()),
                AttrOp::Substring(x) => !x.is_empty() && v.contains(x.as_str()),
            }
        }
    })
}

struct Cur<'a> {
    s: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.p).copied()
    }

    fn at_end(&self) -> bool {
        self.p >= self.s.len()
    }

    /// Skips whitespace and comments. Returns whether anything was skipped.
    fn skip_ws(&mut self) -> bool {
        let start = self.p;
        loop {
            while matches!(self.peek(), Some(c) if c.is_ascii_whitespace()) {
                self.p += 1;
            }
            if self.s[self.p..].starts_with(b"/*") {
                match find(&self.s[self.p + 2..], b"*/") {
                    Some(i) => self.p += 2 + i + 2,
                    None => self.p = self.s.len(),
                }
            } else if self.s[self.p..].starts_with(b"<!--") {
                self.p += 4;
            } else if self.s[self.p..].starts_with(b"-->") {
                self.p += 3;
            } else {
                break;
            }
        }
        self.p != start
    }

    fn ident(&mut self) -> &'a str {
        let start = self.p;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c >= 0x80 {
                self.p += 1;
            } else {
                break;
            }
        }
        std::str::from_utf8(&self.s[start..self.p]).unwrap_or("")
    }

    fn str_or_ident(&mut self) -> String {
        match self.peek() {
            Some(q @ (b'"' | b'\'')) => {
                self.p += 1;
                let start = self.p;
                while let Some(c) = self.peek() {
                    if c == q {
                        break;
                    }
                    self.p += 1;
                }
                let v = String::from_utf8_lossy(&self.s[start..self.p]).into_owned();
                if self.peek() == Some(q) {
                    self.p += 1;
                }
                v
            }
            _ => self.ident().to_string(),
        }
    }

    /// Skips a balanced `{ ... }` block (the cursor is on `{`).
    fn skip_block(&mut self) {
        let mut depth = 0usize;
        while let Some(c) = self.peek() {
            self.p += 1;
            match c {
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_compound(c: &mut Cur) -> Option<Compound> {
    let mut comp = Compound::default();
    let mut any = false;
    if c.peek() == Some(b'*') {
        c.p += 1;
        any = true;
    } else if matches!(c.peek(), Some(ch) if ch.is_ascii_alphabetic() || ch == b'_') {
        comp.tag = Some(c.ident().to_string());
        any = true;
    }
    loop {
        match c.peek() {
            Some(b'.') => {
                c.p += 1;
                let name = c.ident();
                if name.is_empty() {
                    return None;
                }
                comp.subs
                    .push(SubSel::Attr("class".into(), AttrOp::Includes(name.into())));
            }
            Some(b'#') => {
                c.p += 1;
                let name = c.ident();
                if name.is_empty() {
                    return None;
                }
                comp.subs
                    .push(SubSel::Attr("id".into(), AttrOp::Eq(name.into())));
            }
            Some(b'[') => {
                c.p += 1;
                c.skip_ws();
                let name = c.ident().to_string();
                c.skip_ws();
                let op = match c.peek() {
                    Some(b']') => {
                        c.p += 1;
                        comp.subs.push(SubSel::Attr(name, AttrOp::Exists));
                        any = true;
                        continue;
                    }
                    Some(b'=') => {
                        c.p += 1;
                        0u8
                    }
                    Some(o @ (b'~' | b'|' | b'^' | b'$' | b'*')) => {
                        c.p += 1;
                        if c.peek() != Some(b'=') {
                            return None;
                        }
                        c.p += 1;
                        o
                    }
                    _ => return None,
                };
                c.skip_ws();
                let v = c.str_or_ident();
                c.skip_ws();
                if c.peek() != Some(b']') {
                    return None;
                }
                c.p += 1;
                let op = match op {
                    0 => AttrOp::Eq(v),
                    b'~' => AttrOp::Includes(v),
                    b'|' => AttrOp::DashMatch(v),
                    b'^' => AttrOp::Prefix(v),
                    b'$' => AttrOp::Suffix(v),
                    _ => AttrOp::Substring(v),
                };
                comp.subs.push(SubSel::Attr(name, op));
            }
            Some(b':') => {
                c.p += 1;
                if c.peek() == Some(b':') {
                    c.p += 1;
                }
                let name = c.ident();
                if c.peek() == Some(b'(') {
                    // Functional pseudo-classes are unsupported; skip the args.
                    while let Some(ch) = c.peek() {
                        c.p += 1;
                        if ch == b')' {
                            break;
                        }
                    }
                    comp.subs.push(SubSel::Never);
                } else if name == "first-child" {
                    comp.subs.push(SubSel::FirstChild);
                } else {
                    comp.subs.push(SubSel::Never);
                }
            }
            _ => break,
        }
        any = true;
    }
    any.then_some(comp)
}

/// Parses one selector up to `,` or `{`.
fn parse_selector(c: &mut Cur) -> Option<Selector> {
    let mut compounds: Vec<Compound> = Vec::new();
    let mut combs: Vec<Combinator> = Vec::new();
    c.skip_ws();
    loop {
        let comp = parse_compound(c)?;
        compounds.push(comp);
        let ws = c.skip_ws();
        match c.peek() {
            Some(b',') | Some(b'{') | None => break,
            Some(b'>') => {
                c.p += 1;
                c.skip_ws();
                combs.push(Combinator::Child);
            }
            Some(b'+') => {
                c.p += 1;
                c.skip_ws();
                combs.push(Combinator::Adjacent);
            }
            Some(_) if ws => combs.push(Combinator::Descendant),
            Some(_) => return None,
        }
        if compounds.len() > 64 {
            return None;
        }
    }
    let subject = compounds.pop()?;
    let mut ancestors = Vec::with_capacity(compounds.len());
    while let Some(comp) = compounds.pop() {
        let comb = combs.pop()?;
        ancestors.push((comb, comp));
    }
    Some(Selector { subject, ancestors })
}

/// Parses a declaration block body (`a: b; c: d !important`), as found in a
/// `style` attribute or between `{` and `}`.
pub(crate) fn parse_declarations(text: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    for item in split_top_level(text, b';') {
        let Some(colon) = item.find(':') else {
            continue;
        };
        let name = item[..colon].trim();
        let mut value = item[colon + 1..].trim();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let mut important = false;
        if let Some(i) = value.rfind('!') {
            if value[i + 1..].trim().eq_ignore_ascii_case("important") {
                important = true;
                value = value[..i].trim_end();
            }
        }
        // Strip comments inside values.
        let value = strip_comments(value);
        let name = strip_comments(name);
        if name.is_empty() || value.is_empty() {
            continue;
        }
        out.push(Declaration {
            name: name.to_ascii_lowercase(),
            value,
            important,
        });
    }
    out
}

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find("*/") {
            Some(j) => rest = &rest[i + 2 + j + 2..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Splits on `sep` outside quotes and parentheses.
fn split_top_level(text: &str, sep: u8) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut start = 0;
    for (i, &c) in b.iter().enumerate() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'"' | b'\'' => quote = Some(c),
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ if c == sep && depth <= 0 => {
                    out.push(&text[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(&text[start..]);
    out
}

/// Parses a style sheet, appending to `rules`. Re-sorts by specificity.
pub(crate) fn parse_sheet(text: &str, rules: &mut Vec<Rule>) {
    let mut c = Cur {
        s: text.as_bytes(),
        p: 0,
    };
    while !c.at_end() {
        c.skip_ws();
        if c.at_end() {
            break;
        }
        if c.peek() == Some(b'@') {
            // Skip an at-rule: up to `;` or a balanced block.
            while let Some(ch) = c.peek() {
                if ch == b';' {
                    c.p += 1;
                    break;
                }
                if ch == b'{' {
                    c.skip_block();
                    break;
                }
                c.p += 1;
            }
            continue;
        }
        let rule_start = c.p;
        let mut selectors = Vec::new();
        let mut ok = true;
        loop {
            match parse_selector(&mut c) {
                Some(s) => selectors.push(s),
                None => {
                    ok = false;
                    break;
                }
            }
            c.skip_ws();
            match c.peek() {
                Some(b',') => c.p += 1,
                _ => break,
            }
        }
        // Find the block regardless of selector validity.
        if !ok {
            c.p = rule_start;
            while let Some(ch) = c.peek() {
                if ch == b'{' {
                    break;
                }
                c.p += 1;
            }
        }
        if c.peek() != Some(b'{') {
            break;
        }
        let body_start = c.p + 1;
        c.skip_block();
        let body_end = c.p.saturating_sub(1).max(body_start);
        if !ok {
            continue;
        }
        let body = std::str::from_utf8(&c.s[body_start..body_end]).unwrap_or("");
        let decls = parse_declarations(body);
        if decls.is_empty() {
            continue;
        }
        for selector in selectors {
            rules.push(Rule {
                selector,
                declarations: decls.clone(),
            });
        }
    }
    rules.sort_by_cached_key(|r| r.selector.specificity());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct E<'a> {
        name: &'a str,
        class: Option<&'a str>,
    }

    impl Element for E<'_> {
        fn local_name(&self) -> &str {
            self.name
        }
        fn attribute(&self, name: &str) -> Option<&str> {
            (name == "class").then_some(self.class).flatten()
        }
        fn parent_element(&self) -> Option<Self> {
            None
        }
        fn prev_sibling_element(&self) -> Option<Self> {
            None
        }
    }

    #[test]
    fn sheet() {
        let mut rules = Vec::new();
        parse_sheet(
            "/* c */ @media print { rect { fill: red } } .a, rect { fill: blue !important; stroke:red } bad[ { x: y } circle{fill:green}",
            &mut rules,
        );
        assert_eq!(rules.len(), 3);
        let e = E {
            name: "rect",
            class: Some("b a"),
        };
        let mut budget = 1000;
        assert!(
            rules
                .iter()
                .filter(|r| r.selector.matches(e, &mut budget))
                .count()
                == 2
        );
        // Type selectors sort before class selectors.
        assert!(rules[0].selector.subject.tag.is_some());
        assert!(rules[0].declarations[0].important);
    }

    /// A node of a small test tree.
    struct TreeNode {
        name: &'static str,
        id: Option<&'static str>,
        class: Option<&'static str>,
        parent: Option<usize>,
        prev: Option<usize>,
    }

    #[derive(Clone, Copy)]
    struct T<'a> {
        nodes: &'a [TreeNode],
        i: usize,
    }

    impl Element for T<'_> {
        fn local_name(&self) -> &str {
            self.nodes[self.i].name
        }
        fn attribute(&self, name: &str) -> Option<&str> {
            match name {
                "id" => self.nodes[self.i].id,
                "class" => self.nodes[self.i].class,
                _ => None,
            }
        }
        fn parent_element(&self) -> Option<Self> {
            self.nodes[self.i].parent.map(|i| T {
                nodes: self.nodes,
                i,
            })
        }
        fn prev_sibling_element(&self) -> Option<Self> {
            self.nodes[self.i].prev.map(|i| T {
                nodes: self.nodes,
                i,
            })
        }
    }

    /// Indexed matching against testing every rule in turn: the same rules
    /// match and the same budget is left, for every starting budget
    /// (including ones that run out part-way through the sheet).
    #[test]
    fn index_matches_like_a_full_scan() {
        let mut x = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let names = ["g", "rect", "path", "a"];
        let ids = [None, None, Some("i1"), Some("i2"), Some("")];
        let classes = [
            None,
            Some("x"),
            Some("y"),
            Some("x y"),
            Some("y  x x"),
            Some("z"),
            Some(""),
        ];
        let compounds = [
            "*",
            "g",
            "rect",
            "path",
            ".x",
            ".y",
            ".x.y",
            "#i1",
            "#i2",
            "rect.x",
            "g#i1",
            "[class~=x]",
            "[class~=\"\"]",
            "[id=i2]",
            "[id]",
            "[class=x]",
            ":first-child",
            "path:first-child",
            ":hover",
            ".q",
        ];
        for round in 0..300 {
            let mut nodes: Vec<TreeNode> = Vec::new();
            let mut last_child: Vec<Option<usize>> = Vec::new();
            for i in 0..12usize {
                let parent = (i > 0).then(|| (next() % i as u64) as usize);
                let prev = parent.and_then(|p| last_child[p]);
                if let Some(p) = parent {
                    last_child[p] = Some(i);
                }
                last_child.push(None);
                nodes.push(TreeNode {
                    name: names[(next() % names.len() as u64) as usize],
                    id: ids[(next() % ids.len() as u64) as usize],
                    class: classes[(next() % classes.len() as u64) as usize],
                    parent,
                    prev,
                });
            }
            let mut text = String::new();
            for _ in 0..(1 + next() % 14) {
                for part in 0..(1 + next() % 3) {
                    if part > 0 {
                        text.push_str([" ", " > ", " + "][(next() % 3) as usize]);
                    }
                    text.push_str(compounds[(next() % compounds.len() as u64) as usize]);
                }
                text.push_str(" { fill: red } ");
            }
            let mut rules = Vec::new();
            parse_sheet(&text, &mut rules);
            let index = RuleIndex::build(&rules);
            let mut scratch = Vec::new();
            for i in 0..nodes.len() {
                let e = T { nodes: &nodes, i };
                for start in [0u64, 1, 2, 3, 4, 5, 7, 9, 12, 16, 25, 40, 1000] {
                    let mut want_budget = start;
                    let want: Vec<usize> = (0..rules.len())
                        .filter(|&r| rules[r].selector.matches(e, &mut want_budget))
                        .collect();
                    let mut got_budget = start;
                    let mut got = Vec::new();
                    let mut push = |rule: &Rule| {
                        got.push(rules.iter().position(|r| std::ptr::eq(r, rule)).unwrap());
                    };
                    for_each_match(
                        &rules,
                        Some(&index),
                        &mut scratch,
                        e,
                        &mut got_budget,
                        &mut push,
                    );
                    // Without an index every rule is tested in turn.
                    let mut plain_budget = start;
                    let mut plain = Vec::new();
                    for_each_match(&rules, None, &mut scratch, e, &mut plain_budget, |rule| {
                        plain.push(rules.iter().position(|r| std::ptr::eq(r, rule)).unwrap());
                    });
                    assert_eq!((&plain, plain_budget), (&want, want_budget));
                    assert_eq!(
                        (got, got_budget),
                        (want, want_budget),
                        "round {round} node {i} budget {start}: {text}"
                    );
                }
            }
        }
    }
}
