// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3b-2 — the one YAML reader a scan has: `yaml-rust2`'s event
//! parser, never its loader. The loader copies an anchor's whole subtree at
//! every alias, so a file of a few lines can grow without bound; here an
//! alias counts as its anchor's size against [`MAX_NODES`] a document, and a
//! document past it is refused whole.
//!
//! A key the caller names in `skip` is passed by at the top of each document
//! without a byte of its value kept: a kubeconfig's `users`, which holds its
//! credentials (decision 46).

use std::collections::HashMap;

use yaml_rust2::parser::{Event, EventReceiver, Parser};

/// The most nodes one document may hold, an alias counted as its anchor.
pub(super) const MAX_NODES: usize = 100_000;

/// A document, as far as a scan reads one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Node {
    Scalar(String),
    Seq(Vec<Node>),
    Map(Vec<(String, Node)>),
}

impl Node {
    /// The value under `key`, when this is a mapping holding it.
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The text, when this is a scalar.
    pub fn text(&self) -> Option<&str> {
        match self {
            Node::Scalar(s) => Some(s),
            _ => None,
        }
    }

    /// The items, when this is a sequence; none otherwise.
    pub fn items(&self) -> &[Node] {
        match self {
            Node::Seq(items) => items,
            _ => &[],
        }
    }

    /// The nodes it holds, itself included: what an alias of it costs.
    fn size(&self) -> usize {
        1 + match self {
            Node::Scalar(_) => 0,
            Node::Seq(items) => items.iter().map(Node::size).sum(),
            Node::Map(entries) => entries.iter().map(|(_, v)| 1 + v.size()).sum(),
        }
    }
}

/// Every document in `text`, or why not.
pub(super) fn documents(text: &str, skip: &[&str]) -> Result<Vec<Node>, String> {
    let mut b = Builder {
        skip,
        ..Builder::default()
    };
    Parser::new_from_str(text)
        .load(&mut b, true)
        .map_err(|e| e.to_string())?;
    match b.error {
        Some(e) => Err(e),
        None => Ok(b.docs),
    }
}

/// A container being read.
enum Open {
    Seq(Vec<Node>, usize),
    Map(Vec<(String, Node)>, Option<String>, usize),
}

#[derive(Default)]
struct Builder<'a> {
    skip: &'a [&'a str],
    docs: Vec<Node>,
    open: Vec<Open>,
    anchors: HashMap<usize, Node>,
    spent: usize,
    /// Inside a skipped value: how many containers deep.
    skipping: Option<usize>,
    error: Option<String>,
}

impl Builder<'_> {
    fn charge(&mut self, n: usize) -> bool {
        self.spent = self.spent.saturating_add(n);
        if self.spent > MAX_NODES {
            self.error = Some(format!("past {MAX_NODES} nodes in one document"));
            return false;
        }
        true
    }

    /// A finished node: kept under its anchor, then given to its container.
    fn finish(&mut self, node: Node, anchor: usize) {
        if anchor != 0 {
            self.anchors.insert(anchor, node.clone());
        }
        match self.open.last_mut() {
            None => self.docs.push(node),
            Some(Open::Seq(items, _)) => items.push(node),
            Some(Open::Map(entries, key, _)) => match key.take() {
                Some(k) => entries.push((k, node)),
                None => {
                    let k = match node {
                        Node::Scalar(s) => s,
                        // A complex key names nothing a scan reads.
                        _ => String::new(),
                    };
                    // At the top of a document, a key to pass by.
                    if self.open.len() == 1 && self.skip.contains(&k.as_str()) {
                        self.skipping = Some(0);
                    } else if let Some(Open::Map(_, key, _)) = self.open.last_mut() {
                        *key = Some(k);
                    }
                }
            },
        }
    }
}

impl EventReceiver for Builder<'_> {
    fn on_event(&mut self, ev: Event) {
        if self.error.is_some() {
            return;
        }
        // A skipped value is consumed to its end, nothing kept.
        if let Some(depth) = self.skipping {
            match ev {
                Event::SequenceStart(..) | Event::MappingStart(..) => {
                    self.skipping = Some(depth + 1)
                }
                Event::SequenceEnd | Event::MappingEnd => {
                    self.skipping = if depth <= 1 { None } else { Some(depth - 1) }
                }
                Event::Scalar(..) | Event::Alias(..) if depth == 0 => self.skipping = None,
                _ => {}
            }
            return;
        }
        match ev {
            Event::DocumentStart => {
                self.open.clear();
                self.anchors.clear();
                self.spent = 0;
            }
            // Each node is charged as it opens; past the budget, nothing more
            // is built and the error stands.
            Event::Scalar(text, _, anchor, _) if self.charge(1) => {
                self.finish(Node::Scalar(text), anchor);
            }
            Event::SequenceStart(anchor, _) if self.charge(1) => {
                self.open.push(Open::Seq(Vec::new(), anchor));
            }
            Event::MappingStart(anchor, _) if self.charge(1) => {
                self.open.push(Open::Map(Vec::new(), None, anchor));
            }
            Event::SequenceEnd => {
                if let Some(Open::Seq(items, anchor)) = self.open.pop() {
                    self.finish(Node::Seq(items), anchor);
                }
            }
            Event::MappingEnd => {
                if let Some(Open::Map(entries, _, anchor)) = self.open.pop() {
                    self.finish(Node::Map(entries), anchor);
                }
            }
            Event::Alias(id) => {
                let Some(node) = self.anchors.get(&id).cloned() else {
                    self.error = Some("an alias of no anchor".into());
                    return;
                };
                if self.charge(node.size()) {
                    self.finish(node, 0);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Node {
        Node::Scalar(v.into())
    }

    /// Mappings, sequences and scalars, several documents, an anchor used
    /// twice.
    #[test]
    fn documents_are_read_with_their_aliases() {
        let docs = documents(
            "base: &b {image: x}\nweb: *b\nlist: [1, 2]\n---\nkind: Pod\n",
            &[],
        )
        .unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(
            docs[0].get("web").and_then(|w| w.get("image")),
            Some(&s("x"))
        );
        assert_eq!(
            docs[0].get("list").map(Node::items).map(<[Node]>::len),
            Some(2)
        );
        assert_eq!(docs[1].get("kind").and_then(Node::text), Some("Pod"));
    }

    /// An alias costs its anchor's size: a few lines that would expand past
    /// the budget are refused, never built.
    #[test]
    fn an_alias_bomb_is_refused_never_expanded() {
        let mut text = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n");
        for i in 1..9 {
            let prev = format!("*a{}", i - 1);
            let items = [prev.as_str(); 10].join(", ");
            text.push_str(&format!("a{i}: &a{i} [{items}]\n"));
        }
        let why = documents(&text, &[]).unwrap_err();
        assert!(why.contains("nodes in one document"), "{why}");
    }

    /// A key named in `skip` is passed by at the top of a document, whatever
    /// its value's shape; the same key deeper is read.
    #[test]
    fn a_skipped_key_keeps_nothing_of_its_value() {
        let text = "users:\n- name: dev\n  user: {token: abc, nested: [1, {a: b}]}\nkeep: 1\n\
                    inner: {users: seen}\n---\nusers: plain\nafter: 2\n";
        let docs = documents(text, &["users"]).unwrap();
        assert!(docs[0].get("users").is_none());
        assert_eq!(docs[0].get("keep"), Some(&s("1")));
        assert_eq!(
            docs[0].get("inner").and_then(|i| i.get("users")),
            Some(&s("seen"))
        );
        assert!(docs[1].get("users").is_none());
        assert_eq!(docs[1].get("after"), Some(&s("2")));
    }

    #[test]
    fn what_is_not_yaml_is_refused() {
        assert!(documents("a: [1, 2\n", &[]).is_err());
        assert!(documents("a: *nowhere\n", &[]).is_err());
    }
}
