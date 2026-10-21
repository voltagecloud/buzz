//! Inert HTML (`inertHtml`): parsed as a `<template>`'s contents, so
//! nothing in it runs or loads. GitHub bodies only ever read text, tag
//! names and a few attributes from it.

use html5ever::tendril::TendrilSink;
use html5ever::{QualName, local_name, namespace_url, ns};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// Elements nested deeper than this keep their markup as text, so a
/// hostile body cannot make the tree arbitrarily deep.
const MAX_TREE_DEPTH: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Text(String),
    Element(Element),
    /// An element past [`MAX_TREE_DEPTH`], as its serialized markup.
    Raw(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// The fragment's top-level nodes.
pub fn parse(html: &str) -> Vec<Node> {
    let context = QualName::new(None, ns!(html), local_name!("template"));
    let dom = html5ever::parse_fragment(RcDom::default(), Default::default(), context, Vec::new())
        .one(html);
    // The fragment parser puts everything under one `html` element.
    let root = dom.document.children.borrow().first().cloned();
    let Some(root) = root else {
        return Vec::new();
    };
    let children = root.children.borrow().clone();
    children
        .iter()
        .filter_map(|child| convert(child, 0))
        .collect()
}

fn convert(handle: &Handle, depth: usize) -> Option<Node> {
    match &handle.data {
        NodeData::Text { contents } => Some(Node::Text(contents.borrow().to_string())),
        NodeData::Element { name, attrs, .. } => {
            if depth > MAX_TREE_DEPTH {
                return Some(Node::Raw(serialize(handle)));
            }
            let children = handle.children.borrow().clone();
            Some(Node::Element(Element {
                name: name.local.to_string(),
                attrs: attrs
                    .borrow()
                    .iter()
                    .map(|a| (a.name.local.to_string(), a.value.to_string()))
                    .collect(),
                children: children
                    .iter()
                    .filter_map(|child| convert(child, depth + 1))
                    .collect(),
            }))
        }
        _ => None,
    }
}

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

fn escape_text(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
}

fn escape_attr(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
}

/// `outerHTML` of a parsed subtree, walked with an explicit stack.
fn serialize(handle: &Handle) -> String {
    enum Step {
        Open(Handle),
        Close(String),
    }
    let mut out = String::new();
    let mut steps = vec![Step::Open(handle.clone())];
    while let Some(step) = steps.pop() {
        let node = match step {
            Step::Close(name) => {
                out.push_str("</");
                out.push_str(&name);
                out.push('>');
                continue;
            }
            Step::Open(node) => node,
        };
        match &node.data {
            NodeData::Text { contents } => escape_text(&contents.borrow(), &mut out),
            NodeData::Element { name, attrs, .. } => {
                let tag = name.local.to_string();
                out.push('<');
                out.push_str(&tag);
                for attr in attrs.borrow().iter() {
                    out.push(' ');
                    out.push_str(&attr.name.local);
                    out.push_str("=\"");
                    escape_attr(&attr.value, &mut out);
                    out.push('"');
                }
                out.push('>');
                if VOID.contains(&tag.as_str()) {
                    continue;
                }
                steps.push(Step::Close(tag));
                for child in node.children.borrow().iter().rev() {
                    steps.push(Step::Open(child.clone()));
                }
            }
            _ => {}
        }
    }
    out
}

/// `outerHTML` of a converted node.
pub fn outer_html(node: &Node) -> String {
    let mut out = String::new();
    write_html(node, &mut out);
    out
}

fn write_html(node: &Node, out: &mut String) {
    match node {
        Node::Text(text) => escape_text(text, out),
        Node::Raw(raw) => out.push_str(raw),
        Node::Element(element) => {
            out.push('<');
            out.push_str(&element.name);
            for (key, value) in &element.attrs {
                out.push(' ');
                out.push_str(key);
                out.push_str("=\"");
                escape_attr(value, out);
                out.push('"');
            }
            out.push('>');
            if VOID.contains(&element.name.as_str()) {
                return;
            }
            for child in &element.children {
                write_html(child, out);
            }
            out.push_str("</");
            out.push_str(&element.name);
            out.push('>');
        }
    }
}

/// `textContent`.
pub fn text_content(nodes: &[Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Element(element) => out.push_str(&text_content(&element.children)),
            Node::Raw(_) => {}
        }
    }
    out
}

/// Every element in document order with its ancestors (nearest last),
/// as `querySelectorAll("*")` would visit them.
pub fn walk<'a>(nodes: &'a [Node], visit: &mut impl FnMut(&'a Element, &[&'a Element])) {
    fn go<'a>(
        nodes: &'a [Node],
        ancestors: &mut Vec<&'a Element>,
        visit: &mut impl FnMut(&'a Element, &[&'a Element]),
    ) {
        for node in nodes {
            if let Node::Element(element) = node {
                visit(element, ancestors);
                ancestors.push(element);
                go(&element.children, ancestors, visit);
                ancestors.pop();
            }
        }
    }
    go(nodes, &mut Vec::new(), visit);
}

/// The first element named `name` under `nodes`, in document order.
pub fn find<'a>(nodes: &'a [Node], name: &str) -> Option<&'a Element> {
    let mut found = None;
    walk(nodes, &mut |element, _| {
        if found.is_none() && element.name == name {
            found = Some(element);
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inertly_with_entities_and_table_cells() {
        let nodes = parse("<p onclick=\"x()\">A &amp; B</p><script>run()</script>");
        assert_eq!(text_content(&nodes), "A & Brun()");
        let Node::Element(p) = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(p.attr("onclick"), Some("x()"));
        // A template keeps table cells a body would drop.
        let cells = parse("<td>cell</td>");
        assert_eq!(find(&cells, "td").map(|td| td.name.as_str()), Some("td"));
    }

    #[test]
    fn serializes_outer_html() {
        let nodes = parse("<div a=\"1&quot;\">x<img src=\"y\"><br></div>");
        assert_eq!(
            outer_html(&nodes[0]),
            "<div a=\"1&quot;\">x<img src=\"y\"><br></div>"
        );
    }

    #[test]
    fn caps_depth_without_recursing_through_hostile_nesting() {
        let html = format!("{}deep{}", "<div>".repeat(5_000), "</div>".repeat(5_000));
        let nodes = parse(&html);
        let mut depth = 0;
        let mut node = &nodes[0];
        while let Node::Element(element) = node {
            depth += 1;
            node = &element.children[0];
        }
        assert!(depth <= MAX_TREE_DEPTH + 1);
        assert!(matches!(node, Node::Raw(raw) if raw.contains(">deep</div>")));
    }
}
