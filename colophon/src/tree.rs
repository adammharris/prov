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
    /// An `id:<id>` target the registry does not currently resolve
    /// (unknown, tombstoned, or no registry attached).
    UnresolvedId(crate::identity::Id),
    /// A nominal (alias) target whose name several documents claim — a
    /// containment link that cannot be resolved to one child.
    AmbiguousAlias(String),
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
    /// path). Missing, unreadable, cyclic, unresolved-ID, and ambiguous-alias
    /// targets become marked nodes. `id:<id>` targets resolve through the
    /// registry; nominal (`[[My File]]`) targets resolve through the title
    /// index, built once for the whole walk so spanning alias links (a
    /// `contents: alias` vocabulary) descend like any other.
    pub async fn tree(&self, start: impl AsRef<Path>) -> Result<Node> {
        let start = link::normalize(start);
        // The title index is built lazily — only if a nominal (`[[alias]]`) link
        // is actually encountered. A path/id workspace never needs it, so it never
        // pays for a full-workspace scan (which, at the root of a larger repo,
        // would read every file under `target/`, vendored trees, and the rest).
        let mut titles: Option<crate::title::TitleIndex> = None;
        let mut trail: Vec<PathBuf> = Vec::new();
        self.tree_node(start, None, &mut titles, &mut trail).await
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
        titles: &'a mut Option<crate::title::TitleIndex>,
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
                // Build the title index on first sight of a nominal link, never
                // before — this is the only place the tree walk can need it.
                if titles.is_none() && crate::title::is_alias_shaped(&child.target) {
                    *titles = Some(self.title_index().await?);
                }
                let child_path = match self.resolve_link_with(&path, &child, titles.as_ref()) {
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
                    Target::AmbiguousAlias(name) => {
                        children.push(Node {
                            path: PathBuf::from(name.clone()),
                            title: None,
                            label: child.label,
                            kind: NodeKind::AmbiguousAlias(name),
                            children: Vec::new(),
                        });
                        continue;
                    }
                    Target::Path(p) => p,
                };
                children.push(self.tree_node(child_path, child.label, titles, trail).await?);
                // (titles carried by &mut, so a nominal link deeper in the tree
                // reuses the index built above rather than rescanning.)
            }
            trail.pop();

            Ok(Node { path, title, label, kind: NodeKind::Doc, children })
        })
    }
}

// These tests use YAML frontmatter fixtures, so they run under the `yaml` feature.
#[cfg(all(test, feature = "yaml"))]
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
    fn spanning_alias_links_resolve_through_the_title_index() {
        // A workspace whose containment links are nominal `[[Title]]` aliases:
        // the walk must resolve them through the title index and descend, and
        // flag a name several documents share as ambiguous.
        let dir = tempdir("alias");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- '[[Alpha]]'\n- '[[Dup]]'\n- '[[Ghost]]'\n---\n",
        );
        write(&dir, "notes/alpha.md", "---\ntitle: Alpha\n---\n");
        write(&dir, "one.md", "---\ntitle: Dup\n---\n");
        write(&dir, "two.md", "---\ntitle: Dup\n---\n");

        let ws = Workspace::builder(StdFs).root(&dir).build();
        let root = block_on(ws.tree("index.md")).unwrap();
        assert_eq!(root.children.len(), 3);

        // `[[Alpha]]` → the unique document titled Alpha, descended into.
        assert_eq!(root.children[0].kind, NodeKind::Doc);
        assert_eq!(root.children[0].path, PathBuf::from("notes/alpha.md"));

        // `[[Dup]]` → two documents claim the title, so it cannot resolve.
        assert_eq!(root.children[1].kind, NodeKind::AmbiguousAlias("Dup".into()));

        // `[[Ghost]]` → no document claims it; falls through to a missing path.
        assert_eq!(root.children[2].kind, NodeKind::Missing);
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
