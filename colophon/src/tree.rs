//! Traversal — materialize the spanning containment tree from a root document.
//!
//! This is the discovery walk the whole crate exists for: start at a document,
//! follow the spanning relation's links declared *in* each document, and the
//! workspace structure unfolds. The walk is resilient by design — a missing or
//! unparseable target becomes a marked node, not an error — because a
//! traversal that dies on the first broken link cannot power `tree`, `check`,
//! or any editor view of an imperfect (i.e. real) workspace.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::document::Document;
use crate::error::Result;
use crate::fs::Storage;
use crate::index::IndexStore;
use crate::link::{self, Link};
use crate::meta::Value;
use crate::workspace::{Target, Workspace};

/// Why a node appears in the tree the way it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// A document that was read and parsed.
    Doc,
    /// A spanning target that does not exist on disk.
    Missing,
    /// A target already on the path from the root — a containment cycle. Not
    /// descended into.
    Cycle,
    /// A file that exists but could not be read or parsed; the message says why.
    Unreadable(String),
    /// A `colophon:<id>` target the registry does not currently resolve
    /// (unknown, tombstoned, or no registry attached).
    UnresolvedId(crate::identity::Id),
}

/// One node of the materialized spanning tree.
#[derive(Debug, Clone)]
pub struct Node {
    /// Workspace-relative, normalized path.
    pub path: PathBuf,
    /// The document's `title` field, when present.
    pub title: Option<String>,
    /// The label the *parent's* link carried (`[label](path)`), when any.
    pub label: Option<String>,
    /// How this node was resolved.
    pub kind: NodeKind,
    /// Spanning children, in declaration order.
    pub children: Vec<Node>,
}

impl<FS: Storage, Id, Ix: IndexStore> Workspace<FS, Id, Ix> {
    /// Materialize the spanning tree rooted at `start` (a workspace-relative
    /// path). Missing, unreadable, cyclic, and unresolved-ID targets become
    /// marked nodes. `colophon:<id>` targets resolve through the registry.
    pub async fn tree(&self, start: impl AsRef<Path>) -> Result<Node> {
        let start = link::normalize(start);
        let mut trail: Vec<PathBuf> = Vec::new();
        self.tree_node(start, None, &mut trail).await
    }

    /// Read and parse the workspace-relative document at `path`, returning the
    /// raw text alongside. The building block traversal, validation, and
    /// mutation share.
    pub(crate) async fn load(&self, path: &Path) -> Result<(String, Document)> {
        let text = self.fs().read_to_string(&self.root().join(path)).await?;
        let doc = Document::parse(path, &text)?;
        Ok((text, doc))
    }

    fn tree_node<'a>(
        &'a self,
        path: PathBuf,
        label: Option<String>,
        trail: &'a mut Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<Node>> + 'a>> {
        Box::pin(async move {
            if trail.contains(&path) {
                return Ok(Node { path, title: None, label, kind: NodeKind::Cycle, children: Vec::new() });
            }
            match self.fs().try_exists(&self.root().join(&path)).await {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(Node { path, title: None, label, kind: NodeKind::Missing, children: Vec::new() });
                }
                Err(e) => {
                    return Ok(Node {
                        path,
                        title: None,
                        label,
                        kind: NodeKind::Unreadable(e.to_string()),
                        children: Vec::new(),
                    });
                }
            }
            let doc = match self.load(&path).await {
                Ok((_, doc)) => doc,
                Err(e) => {
                    return Ok(Node {
                        path,
                        title: None,
                        label,
                        kind: NodeKind::Unreadable(e.to_string()),
                        children: Vec::new(),
                    });
                }
            };
            let title = doc.meta.get("title").and_then(Value::as_str).map(str::to_owned);

            trail.push(path.clone());
            let mut children = Vec::new();
            for raw in self.relations().children(&doc.meta) {
                let child = Link::parse(&raw);
                let child_path = match self.resolve_link(&path, &child) {
                    Target::External => continue,
                    Target::UnresolvedId(id) => {
                        children.push(Node {
                            path: PathBuf::from(child.target.clone()),
                            title: None,
                            label: child.label,
                            kind: NodeKind::UnresolvedId(id),
                            children: Vec::new(),
                        });
                        continue;
                    }
                    Target::Path(p) => p,
                };
                children.push(self.tree_node(child_path, child.label, trail).await?);
            }
            trail.pop();

            Ok(Node { path, title, label, kind: NodeKind::Doc, children })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::block_on;
    use crate::fs::StdFs;

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("colophon-tree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn walks_the_spanning_tree_with_labels_and_titles() {
        let dir = tempdir("walk");
        write(&dir, "index.md", "---\ntitle: Root\ncontents:\n- '[A](notes/a.md)'\n- missing.md\n---\n");
        write(&dir, "notes/a.md", "---\ntitle: A\npart_of: ../index.md\n---\n");

        let ws = Workspace::builder(StdFs).root(&dir).build();
        let root = block_on(ws.tree("index.md")).unwrap();
        assert_eq!(root.title.as_deref(), Some("Root"));
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].path, PathBuf::from("notes/a.md"));
        assert_eq!(root.children[0].label.as_deref(), Some("A"));
        assert_eq!(root.children[0].kind, NodeKind::Doc);
        assert_eq!(root.children[1].kind, NodeKind::Missing);
    }

    #[test]
    fn cycles_are_marked_not_followed() {
        let dir = tempdir("cycle");
        write(&dir, "a.md", "---\ncontents:\n- b.md\n---\n");
        write(&dir, "b.md", "---\ncontents:\n- a.md\n---\n");

        let ws = Workspace::builder(StdFs).root(&dir).build();
        let root = block_on(ws.tree("a.md")).unwrap();
        let b = &root.children[0];
        assert_eq!(b.kind, NodeKind::Doc);
        assert_eq!(b.children[0].kind, NodeKind::Cycle);
        assert_eq!(b.children[0].path, PathBuf::from("a.md"));
    }
}
