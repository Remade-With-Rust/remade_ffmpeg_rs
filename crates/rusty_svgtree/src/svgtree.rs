//! The intermediate SVG document: the XML DOM reduced to SVG elements with
//! their attributes fully resolved (presentation attributes, `<style>` rules,
//! `style` attributes, `inherit`), and `use` references expanded into copies,
//! as usvg's `svgtree` does.
//!
//! Hostile-input bounds live here: element nesting depth, total node count
//! (which bounds `use` amplification) and reference-cycle breaking.

use std::collections::HashMap;

use rusty_xml::{NodeId as XId, NodeKind, XmlDoc};

use crate::css;
use crate::Error;

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// SVG element kinds we keep. Anything else is dropped with its subtree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EId {
    Svg,
    G,
    Defs,
    Symbol,
    Use,
    Switch,
    Path,
    Rect,
    Circle,
    Ellipse,
    Line,
    Polyline,
    Polygon,
    Image,
    Text,
    LinearGradient,
    RadialGradient,
    Stop,
    ClipPath,
    Mask,
    Pattern,
    Marker,
    Filter,
}

impl EId {
    fn from_name(name: &str) -> Option<EId> {
        Some(match name {
            "svg" => EId::Svg,
            "g" | "a" => EId::G,
            "defs" => EId::Defs,
            "symbol" => EId::Symbol,
            "use" => EId::Use,
            "switch" => EId::Switch,
            "path" => EId::Path,
            "rect" => EId::Rect,
            "circle" => EId::Circle,
            "ellipse" => EId::Ellipse,
            "line" => EId::Line,
            "polyline" => EId::Polyline,
            "polygon" => EId::Polygon,
            "image" => EId::Image,
            "text" => EId::Text,
            "linearGradient" => EId::LinearGradient,
            "radialGradient" => EId::RadialGradient,
            "stop" => EId::Stop,
            "clipPath" => EId::ClipPath,
            "mask" => EId::Mask,
            "pattern" => EId::Pattern,
            "marker" => EId::Marker,
            "filter" => EId::Filter,
            _ => return None,
        })
    }

    /// Elements that draw (SVG "graphics elements" plus `use`).
    pub(crate) fn is_graphic(self) -> bool {
        matches!(
            self,
            EId::Circle
                | EId::Ellipse
                | EId::Image
                | EId::Line
                | EId::Path
                | EId::Polygon
                | EId::Polyline
                | EId::Rect
                | EId::Text
                | EId::Use
        )
    }

    pub(crate) fn is_gradient(self) -> bool {
        matches!(self, EId::LinearGradient | EId::RadialGradient)
    }
}

/// Presentation attributes: settable from CSS and the `style` attribute.
fn is_presentation(name: &str) -> bool {
    matches!(
        name,
        "alignment-baseline"
            | "baseline-shift"
            | "background-color"
            | "clip-path"
            | "clip-rule"
            | "color"
            | "color-interpolation"
            | "color-interpolation-filters"
            | "color-rendering"
            | "direction"
            | "display"
            | "dominant-baseline"
            | "fill"
            | "fill-opacity"
            | "fill-rule"
            | "filter"
            | "flood-color"
            | "flood-opacity"
            | "font-family"
            | "font-kerning"
            | "font-size"
            | "font-size-adjust"
            | "font-stretch"
            | "font-style"
            | "font-variant"
            | "font-weight"
            | "image-rendering"
            | "isolation"
            | "letter-spacing"
            | "lighting-color"
            | "marker-end"
            | "marker-mid"
            | "marker-start"
            | "mask"
            | "mask-type"
            | "mix-blend-mode"
            | "opacity"
            | "overflow"
            | "paint-order"
            | "shape-rendering"
            | "stop-color"
            | "stop-opacity"
            | "stroke"
            | "stroke-dasharray"
            | "stroke-dashoffset"
            | "stroke-linecap"
            | "stroke-linejoin"
            | "stroke-miterlimit"
            | "stroke-opacity"
            | "stroke-width"
            | "text-anchor"
            | "text-decoration"
            | "text-rendering"
            | "transform"
            | "transform-origin"
            | "unicode-bidi"
            | "vector-effect"
            | "visibility"
            | "word-spacing"
            | "writing-mode"
    )
}

fn is_non_inheritable(name: &str) -> bool {
    matches!(
        name,
        "alignment-baseline"
            | "baseline-shift"
            | "clip-path"
            | "display"
            | "dominant-baseline"
            | "filter"
            | "flood-color"
            | "flood-opacity"
            | "mask"
            | "opacity"
            | "overflow"
            | "lighting-color"
            | "stop-color"
            | "stop-opacity"
            | "text-decoration"
            | "transform"
            | "transform-origin"
    )
}

/// Whether an attribute is inherited by descendants.
pub(crate) fn is_inheritable(name: &str) -> bool {
    is_presentation(name) && !is_non_inheritable(name)
}

#[derive(Clone, Debug)]
struct Attr {
    name: Box<str>,
    value: Box<str>,
    important: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(crate) struct NodeId(u32);

#[derive(Debug)]
struct NodeData {
    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    next_sibling: Option<NodeId>,
    tag: Option<EId>,
    attrs: std::ops::Range<u32>,
}

/// The resolved SVG document.
pub(crate) struct Document {
    nodes: Vec<NodeData>,
    attrs: Vec<Attr>,
    links: HashMap<Box<str>, NodeId>,
}

/// A borrowed node handle.
#[derive(Clone, Copy)]
pub(crate) struct SvgNode<'a> {
    id: NodeId,
    doc: &'a Document,
}

impl PartialEq for SvgNode<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && std::ptr::eq(self.doc, other.doc)
    }
}

impl std::fmt::Debug for SvgNode<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SvgNode({:?}, {:?})", self.id, self.tag_name())
    }
}

impl<'a> SvgNode<'a> {
    fn d(&self) -> &'a NodeData {
        &self.doc.nodes[self.id.0 as usize]
    }

    pub(crate) fn id(&self) -> NodeId {
        self.id
    }

    pub(crate) fn document(&self) -> &'a Document {
        self.doc
    }

    pub(crate) fn tag_name(&self) -> Option<EId> {
        self.d().tag
    }

    pub(crate) fn element_id(&self) -> &'a str {
        self.attribute("id").unwrap_or("")
    }

    pub(crate) fn attribute(&self, name: &str) -> Option<&'a str> {
        let r = self.d().attrs.clone();
        self.doc.attrs[r.start as usize..r.end as usize]
            .iter()
            .find(|a| &*a.name == name)
            .map(|a| &*a.value)
    }

    pub(crate) fn has_attribute(&self, name: &str) -> bool {
        self.attribute(name).is_some()
    }

    pub(crate) fn parent(&self) -> Option<SvgNode<'a>> {
        self.d().parent.map(|id| SvgNode { id, doc: self.doc })
    }

    /// The parent, if it is an element (not the document root).
    pub(crate) fn parent_element(&self) -> Option<SvgNode<'a>> {
        self.parent().filter(|p| p.tag_name().is_some())
    }

    pub(crate) fn first_child(&self) -> Option<SvgNode<'a>> {
        self.d().first_child.map(|id| SvgNode { id, doc: self.doc })
    }

    pub(crate) fn children(&self) -> Children<'a> {
        Children(self.first_child())
    }

    pub(crate) fn has_children(&self) -> bool {
        self.d().first_child.is_some()
    }

    /// Self, then each ancestor up to (excluding) the document root.
    pub(crate) fn ancestors(&self) -> Ancestors<'a> {
        Ancestors(Some(*self))
    }

    /// Pre-order traversal including `self`.
    pub(crate) fn descendants(&self) -> Descendants<'a> {
        Descendants {
            root: *self,
            next: Some(*self),
        }
    }

    /// Looks an attribute up on self or, if inheritable, on ancestors; a
    /// non-inheritable attribute is also taken from the direct parent (usvg's
    /// `find_attribute`).
    pub(crate) fn find_attribute_node(&self, name: &str) -> Option<SvgNode<'a>> {
        if is_inheritable(name) {
            self.ancestors().find(|n| n.has_attribute(name))
        } else if self.has_attribute(name) {
            Some(*self)
        } else {
            self.parent_element().filter(|n| n.has_attribute(name))
        }
    }

    pub(crate) fn find_attribute(&self, name: &str) -> Option<&'a str> {
        self.find_attribute_node(name)?.attribute(name)
    }

    /// Resolves an `href` (`#id`) to a node.
    pub(crate) fn href(&self) -> Option<SvgNode<'a>> {
        let v = self.attribute("href")?;
        self.doc.element_by_id(crate::values::iri(v)?)
    }

    /// Resolves a `url(#id)` attribute to a node.
    pub(crate) fn func_iri_attribute(&self, name: &str) -> Option<SvgNode<'a>> {
        let v = self.attribute(name)?;
        self.doc.element_by_id(crate::values::func_iri(v)?)
    }

    /// The `href` chain starting at `self` (self first), cycle-safe and
    /// bounded.
    pub(crate) fn href_iter(&self) -> HrefIter<'a> {
        HrefIter {
            origin: self.id,
            curr: Some(*self),
            steps: 0,
        }
    }
}

pub(crate) struct Children<'a>(Option<SvgNode<'a>>);

impl<'a> Iterator for Children<'a> {
    type Item = SvgNode<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let n = self.0?;
        self.0 = n.d().next_sibling.map(|id| SvgNode { id, doc: n.doc });
        Some(n)
    }
}

pub(crate) struct Ancestors<'a>(Option<SvgNode<'a>>);

impl<'a> Iterator for Ancestors<'a> {
    type Item = SvgNode<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let n = self.0?;
        if n.tag_name().is_none() {
            self.0 = None;
            return None;
        }
        self.0 = n.parent();
        Some(n)
    }
}

pub(crate) struct Descendants<'a> {
    root: SvgNode<'a>,
    next: Option<SvgNode<'a>>,
}

impl<'a> Iterator for Descendants<'a> {
    type Item = SvgNode<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let n = self.next?;
        // Pre-order successor, never leaving `root`'s subtree.
        self.next = if let Some(c) = n.first_child() {
            Some(c)
        } else {
            let mut cur = Some(n);
            let mut succ = None;
            while let Some(c) = cur {
                if c == self.root {
                    break;
                }
                if let Some(s) = c.d().next_sibling {
                    succ = Some(SvgNode { id: s, doc: c.doc });
                    break;
                }
                cur = c.parent();
            }
            succ
        };
        Some(n)
    }
}

/// Maximum `href` hops followed (gradients, patterns): reference chains in
/// real files are 1-3 long; this bounds a hostile chain.
const MAX_HREF_HOPS: u32 = 64;

pub(crate) struct HrefIter<'a> {
    origin: NodeId,
    curr: Option<SvgNode<'a>>,
    steps: u32,
}

impl<'a> Iterator for HrefIter<'a> {
    type Item = SvgNode<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let n = self.curr?;
        self.steps += 1;
        self.curr = match n.href() {
            Some(link) if link.id != n.id && link.id != self.origin => {
                if self.steps >= MAX_HREF_HOPS {
                    None
                } else {
                    Some(link)
                }
            }
            _ => None,
        };
        Some(n)
    }
}

impl Document {
    pub(crate) fn root(&self) -> SvgNode<'_> {
        SvgNode {
            id: NodeId(0),
            doc: self,
        }
    }

    pub(crate) fn root_element(&self) -> Option<SvgNode<'_>> {
        self.root().first_child()
    }

    pub(crate) fn element_by_id(&self, id: &str) -> Option<SvgNode<'_>> {
        self.links.get(id).map(|&id| SvgNode { id, doc: self })
    }

    fn append(&mut self, parent: NodeId, tag: EId, attrs: std::ops::Range<u32>) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(NodeData {
            parent: Some(parent),
            first_child: None,
            last_child: None,
            next_sibling: None,
            tag: Some(tag),
            attrs,
        });
        let p = parent.0 as usize;
        if let Some(last) = self.nodes[p].last_child {
            self.nodes[last.0 as usize].next_sibling = Some(id);
        } else {
            self.nodes[p].first_child = Some(id);
        }
        self.nodes[p].last_child = Some(id);
        id
    }

    fn set_attribute_value(&mut self, node: NodeId, name: &str, value: &str) {
        let r = self.nodes[node.0 as usize].attrs.clone();
        for a in &mut self.attrs[r.start as usize..r.end as usize] {
            if &*a.name == name {
                a.value = value.into();
            }
        }
    }
}

/// Compound-selector tests allowed for the whole document (about a second of
/// matching); past it, remaining style rules stop matching.
const CSS_MATCH_BUDGET: u64 = 50_000_000;

/// Node visits allowed for the cycle-breaking passes below (each pass).
const FIXUP_BUDGET: usize = 20_000_000;

/// Limits applied while building the document.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub max_nodes: usize,
    pub max_depth: u32,
}

/// A rusty_xml element handle for CSS matching.
#[derive(Clone, Copy)]
struct XNode<'a> {
    doc: &'a XmlDoc,
    id: XId,
}

impl<'a> XNode<'a> {
    fn is_element(&self) -> bool {
        self.doc.kind(self.id) == NodeKind::Element
    }

    fn attr(&self, name: &str) -> Option<&'a str> {
        let mut a = self.doc.first_attr(self.id);
        while let Some(id) = a {
            if self.doc.prefix(id).is_none() && self.doc.name(id) == name {
                return Some(self.doc.content(id));
            }
            a = self.doc.next_sibling(id);
        }
        None
    }

    fn svg_tag(&self) -> Option<EId> {
        if !self.is_element() {
            return None;
        }
        match self.doc.ns_uri(self.id) {
            None | Some(SVG_NS) => {}
            Some(_) => return None,
        }
        // Prefixed elements are not SVG unless the prefix maps to SVG.
        if self.doc.prefix(self.id).is_some() && self.doc.ns_uri(self.id) != Some(SVG_NS) {
            return None;
        }
        EId::from_name(self.doc.name(self.id))
    }

    /// `href`, preferring the unprefixed form over `xlink:href`.
    fn href(&self) -> Option<&'a str> {
        let mut plain = None;
        let mut xlink = None;
        let mut a = self.doc.first_attr(self.id);
        while let Some(id) = a {
            if self.doc.name(id) == "href" {
                match self.doc.prefix(id) {
                    None => plain = Some(self.doc.content(id)),
                    Some(p) => {
                        let ns = self.doc.ns_uri(id);
                        if ns == Some(XLINK_NS) || (ns.is_none() && p == "xlink") {
                            xlink = Some(self.doc.content(id));
                        }
                    }
                }
            }
            a = self.doc.next_sibling(id);
        }
        plain.or(xlink)
    }
}

impl css::Element for XNode<'_> {
    fn local_name(&self) -> &str {
        self.doc.name(self.id)
    }

    fn attribute(&self, name: &str) -> Option<&str> {
        self.attr(name)
    }

    fn parent_element(&self) -> Option<Self> {
        let p = self.doc.parent(self.id)?;
        let n = XNode {
            doc: self.doc,
            id: p,
        };
        n.is_element().then_some(n)
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        let mut cur = self.doc.prev_sibling(self.id);
        while let Some(id) = cur {
            if self.doc.kind(id) == NodeKind::Element {
                return Some(XNode { doc: self.doc, id });
            }
            cur = self.doc.prev_sibling(id);
        }
        None
    }
}

struct Builder<'a> {
    xml: &'a XmlDoc,
    /// `use` targets currently being expanded: a target already on this
    /// stack is a reference cycle and is skipped (usvg instead recurses until
    /// its depth limit and fails the whole document).
    use_chain: Vec<XId>,
    /// Shared work budget for `use` recursion checks (sum over all uses).
    use_check_steps: usize,
    /// Work budget for style-sheet matching (compound tests).
    css_budget: u64,
    sheet: Vec<css::Rule>,
    /// `sheet` bucketed by what each rule's subject needs (built once the
    /// whole sheet is parsed, for a sheet long enough to gain from it), and
    /// the scratch list of rules to test.
    rule_index: Option<css::RuleIndex>,
    rule_scratch: Vec<u32>,
    id_map: HashMap<&'a str, XId>,
    doc: Document,
    limits: Limits,
}

/// Parses XML text into an XML DOM with the safe rusty_xml defaults plus
/// internal-entity substitution (Illustrator files declare their namespace
/// URIs as entities).
pub(crate) fn parse_xml(data: &[u8]) -> Result<XmlDoc, Error> {
    let opts = rusty_xml::XML_PARSE_NOENT
        | rusty_xml::XML_PARSE_NONET
        | rusty_xml::XML_PARSE_NO_XXE
        | rusty_xml::XML_PARSE_NOERROR
        | rusty_xml::XML_PARSE_NOWARNING;
    rusty_xml::xml_read_memory(data, None, None, opts)
        .map_err(|e| Error::ParsingFailed(e.to_string()))
}

/// Builds the resolved SVG document from the XML DOM.
pub(crate) fn build(xml: &XmlDoc, limits: Limits) -> Result<Document, Error> {
    let root_el = xml.xml_doc_get_root_element().ok_or(Error::NotAnSvg)?;
    let mut b = Builder {
        xml,
        use_chain: Vec::new(),
        use_check_steps: 0,
        css_budget: CSS_MATCH_BUDGET,
        sheet: Vec::new(),
        rule_index: None,
        rule_scratch: Vec::new(),
        id_map: HashMap::new(),
        doc: Document {
            nodes: vec![NodeData {
                parent: None,
                first_child: None,
                last_child: None,
                next_sibling: None,
                tag: None,
                attrs: 0..0,
            }],
            attrs: Vec::new(),
            links: HashMap::new(),
        },
        limits,
    };

    // Style sheets and the id map, in one bounded iterative walk.
    let mut stack = vec![root_el];
    let mut visited = 0usize;
    while let Some(id) = stack.pop() {
        visited += 1;
        if visited > limits.max_nodes.saturating_mul(4) {
            return Err(Error::ElementsLimitReached);
        }
        let n = XNode { doc: xml, id };
        if !n.is_element() {
            continue;
        }
        if let Some(v) = n.attr("id") {
            b.id_map.entry(v).or_insert(id);
        }
        if xml.name(id) == "style"
            && matches!(xml.ns_uri(id), None | Some(SVG_NS))
            && matches!(n.attr("type"), None | Some("text/css") | Some(""))
        {
            let text = xml.xml_node_get_content(id);
            css::parse_sheet(&text, &mut b.sheet);
        }
        // Push children in reverse for document order.
        let mut kids = Vec::new();
        let mut c = xml.first_child(id);
        while let Some(k) = c {
            kids.push(k);
            c = xml.next_sibling(k);
        }
        stack.extend(kids.into_iter().rev());
    }

    let root = XNode {
        doc: xml,
        id: root_el,
    };
    if root.svg_tag() != Some(EId::Svg) {
        return Err(Error::NotAnSvg);
    }
    b.rule_index = css::RuleIndex::new(&b.sheet);
    b.parse_node(root_el, None, NodeId(0), false, 0)?;

    let mut links = HashMap::new();
    for (i, n) in b.doc.nodes.iter().enumerate() {
        if n.tag.is_none() {
            continue;
        }
        let node = SvgNode {
            id: NodeId(i as u32),
            doc: &b.doc,
        };
        if let Some(id) = node.attribute("id") {
            links.entry(id.into()).or_insert(NodeId(i as u32));
        }
    }
    b.doc.links = links;
    fix_recursive_links(&mut b.doc, EId::ClipPath, "clip-path");
    fix_recursive_links(&mut b.doc, EId::Mask, "mask");
    fix_recursive_patterns(&mut b.doc, "fill");
    fix_recursive_patterns(&mut b.doc, "stroke");
    Ok(b.doc)
}

impl Builder<'_> {
    fn check_nodes(&self) -> Result<(), Error> {
        if self.doc.nodes.len() > self.limits.max_nodes {
            Err(Error::ElementsLimitReached)
        } else {
            Ok(())
        }
    }

    /// `origin` is the `use` element whose copy we are inside, if any.
    fn parse_node(
        &mut self,
        xid: XId,
        origin: Option<XId>,
        parent: NodeId,
        ignore_ids: bool,
        depth: u32,
    ) -> Result<(), Error> {
        if depth > self.limits.max_depth {
            return Err(Error::ElementsLimitReached);
        }
        let x = XNode {
            doc: self.xml,
            id: xid,
        };
        let Some(tag) = x.svg_tag() else {
            return Ok(());
        };
        let node_id = self.parse_element(x, parent, tag, ignore_ids)?;
        if tag == EId::Use {
            self.parse_use(x, origin, node_id, depth + 1)?;
        } else if tag != EId::Text {
            let mut c = self.xml.first_child(xid);
            while let Some(k) = c {
                self.parse_node(k, origin, node_id, ignore_ids, depth + 1)?;
                c = self.xml.next_sibling(k);
            }
        }
        Ok(())
    }

    fn resolve_href(&self, x: XNode) -> Option<XId> {
        let v = x.href()?;
        let id = crate::values::iri(v)?;
        self.id_map.get(id).copied()
    }

    fn parse_use(
        &mut self,
        x: XNode,
        origin: Option<XId>,
        node_id: NodeId,
        depth: u32,
    ) -> Result<(), Error> {
        let Some(link) = self.resolve_href(x) else {
            return Ok(());
        };
        if link == x.id || Some(link) == origin || self.use_chain.contains(&link) {
            return Ok(());
        }
        let lx = XNode {
            doc: self.xml,
            id: link,
        };
        if lx.svg_tag().is_none() {
            return Ok(());
        }
        // A `use` inside the target pointing back at us or at the target.
        let mut stack = vec![link];
        while let Some(id) = stack.pop() {
            self.use_check_steps += 1;
            if self.use_check_steps > self.limits.max_nodes.saturating_mul(4) {
                return Err(Error::ElementsLimitReached);
            }
            let n = XNode { doc: self.xml, id };
            if id != link && n.svg_tag() == Some(EId::Use) {
                if let Some(l2) = self.resolve_href(n) {
                    if l2 == x.id || l2 == link {
                        return Ok(());
                    }
                }
            }
            let mut c = self.xml.first_child(id);
            while let Some(k) = c {
                if self.xml.kind(k) == NodeKind::Element {
                    stack.push(k);
                }
                c = self.xml.next_sibling(k);
            }
        }
        self.use_chain.push(link);
        let r = self.parse_node(link, Some(x.id), node_id, true, depth + 1);
        self.use_chain.pop();
        r
    }

    fn parse_element(
        &mut self,
        x: XNode,
        parent: NodeId,
        tag: EId,
        ignore_ids: bool,
    ) -> Result<NodeId, Error> {
        self.check_nodes()?;
        let start = self.doc.attrs.len();

        // 1. XML attributes.
        let href = x.href();
        let mut a = self.xml.first_attr(x.id);
        while let Some(aid) = a {
            a = self.xml.next_sibling(aid);
            if self.xml.prefix(aid).is_some() {
                continue;
            }
            let name = self.xml.name(aid);
            if matches!(name, "style" | "class" | "href") {
                continue;
            }
            if ignore_ids && name == "id" {
                continue;
            }
            // usvg honours these only from CSS.
            if matches!(name, "mix-blend-mode" | "isolation" | "font-kerning") {
                continue;
            }
            let value = self.xml.content(aid);
            append_attribute(&mut self.doc, parent, start, name, value, false, false);
        }
        if let Some(h) = href {
            append_attribute(&mut self.doc, parent, start, "href", h, false, false);
        }

        // 2. Style sheet rules (sorted by specificity), then 3. `style`: each
        // declaration is applied where it is found, in that order.
        if !self.sheet.is_empty() {
            let doc = &mut self.doc;
            css::for_each_match(
                &self.sheet,
                self.rule_index.as_ref(),
                &mut self.rule_scratch,
                x,
                &mut self.css_budget,
                |rule| {
                    for d in &rule.declarations {
                        apply_declaration(doc, parent, start, &d.name, &d.value, d.important);
                    }
                },
            );
        }
        if let Some(style) = x.attr("style") {
            for d in css::parse_declarations(style) {
                apply_declaration(&mut self.doc, parent, start, &d.name, &d.value, d.important);
            }
        }

        let end = self.doc.attrs.len();
        let id = self.doc.append(parent, tag, start as u32..end as u32);
        Ok(id)
    }
}

/// Applies one style declaration (from a style sheet rule or a `style`
/// attribute) to the element being built, whose attributes start at `start`.
fn apply_declaration(
    doc: &mut Document,
    parent: NodeId,
    start: usize,
    name: &str,
    value: &str,
    important: bool,
) {
    if name == "marker" {
        for n in ["marker-start", "marker-mid", "marker-end"] {
            append_attribute(doc, parent, start, n, value, important, true);
        }
    } else if is_presentation(name) {
        append_attribute(doc, parent, start, name, value, important, true);
    }
}

/// Appends (or, with `replace`, overrides) an attribute of the element
/// being built, whose attributes start at `start`.
fn append_attribute(
    doc: &mut Document,
    parent: NodeId,
    start: usize,
    name: &str,
    value: &str,
    important: bool,
    replace: bool,
) {
    let value = value.trim();
    let resolved: Option<(Box<str>, bool)> = if value == "inherit" {
        resolve_inherit(doc, parent, name)
    } else {
        Some((value.into(), important))
    };
    let Some((value, important)) = resolved else {
        return;
    };
    if let Some(existing) = doc.attrs[start..].iter_mut().find(|a| &*a.name == name) {
        if replace && !existing.important {
            existing.value = value;
            existing.important = important;
        }
        return;
    }
    doc.attrs.push(Attr {
        name: name.into(),
        value,
        important,
    });
}

fn resolve_inherit(doc: &Document, parent: NodeId, name: &str) -> Option<(Box<str>, bool)> {
    let p = SvgNode { id: parent, doc };
    let found = if is_inheritable(name) {
        p.ancestors().find(|n| n.has_attribute(name))
    } else {
        Some(p).filter(|n| n.tag_name().is_some() && n.has_attribute(name))
    };
    if let Some(n) = found {
        return n.attribute(name).map(|v| (v.into(), false));
    }
    let def = match name {
        "image-rendering" | "shape-rendering" | "text-rendering" => "auto",
        "clip-path" | "filter" | "marker-end" | "marker-mid" | "marker-start" | "mask"
        | "stroke" | "stroke-dasharray" | "text-decoration" => "none",
        "font-stretch" | "font-style" | "font-variant" | "font-weight" | "letter-spacing"
        | "word-spacing" => "normal",
        "fill" | "flood-color" | "stop-color" => "black",
        "fill-opacity" | "flood-opacity" | "opacity" | "stop-opacity" | "stroke-opacity" => "1",
        "clip-rule" | "fill-rule" => "nonzero",
        "display" => "inline",
        "overflow" => "visible",
        "stroke-dashoffset" => "0",
        "stroke-linecap" => "butt",
        "stroke-linejoin" => "miter",
        "stroke-miterlimit" => "4",
        "stroke-width" => "1",
        "visibility" => "visible",
        _ => return None,
    };
    Some((def.into(), false))
}

/// Breaks 1- and 2-step reference cycles through `attr` (`clip-path` on a
/// `clipPath`'s content pointing back at it), exactly as usvg does: the
/// offending attribute becomes `none`. Longer cycles are caught by the
/// converter's recursion guard.
fn fix_recursive_links(doc: &mut Document, eid: EId, attr: &str) {
    let mut fixes: Vec<NodeId> = Vec::new();
    let mut steps = 0usize;
    'outer: {
        let d = &*doc;
        for node in d.root().descendants().filter(|n| n.tag_name() == Some(eid)) {
            for child in node.descendants() {
                steps += 1;
                if steps > FIXUP_BUDGET {
                    break 'outer;
                }
                if let Some(link) = child.func_iri_attribute(attr) {
                    if link == node {
                        fixes.push(child.id);
                    } else {
                        for n2 in link.descendants() {
                            steps += 1;
                            if steps > FIXUP_BUDGET {
                                break 'outer;
                            }
                            if n2.func_iri_attribute(attr) == Some(node) {
                                fixes.push(n2.id);
                            }
                        }
                    }
                }
            }
        }
    }
    for id in fixes {
        doc.set_attribute_value(id, attr, "none");
    }
}

/// Pattern content painting with its own pattern: the paint becomes `none`.
fn fix_recursive_patterns(doc: &mut Document, attr: &str) {
    let mut fixes: Vec<NodeId> = Vec::new();
    let mut steps = 0usize;
    'outer: {
        let d = &*doc;
        for pat in d
            .root()
            .descendants()
            .filter(|n| n.tag_name() == Some(EId::Pattern))
        {
            for node in pat.descendants() {
                steps += 1;
                if steps > FIXUP_BUDGET {
                    break 'outer;
                }
                let Some(v) = node.attribute(attr) else {
                    continue;
                };
                if let Some(crate::values::Paint::Url(link_id, _)) = crate::values::paint(v) {
                    if link_id == pat.element_id() {
                        fixes.push(node.id);
                    } else if let Some(linked) = d.element_by_id(link_id) {
                        for n2 in linked.descendants() {
                            steps += 1;
                            if steps > FIXUP_BUDGET {
                                break 'outer;
                            }
                            if let Some(v2) = n2.attribute(attr) {
                                if let Some(crate::values::Paint::Url(l2, _)) =
                                    crate::values::paint(v2)
                                {
                                    if l2 == pat.element_id() {
                                        fixes.push(n2.id);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    for id in fixes {
        doc.set_attribute_value(id, attr, "none");
    }
}
