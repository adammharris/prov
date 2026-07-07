//! Validation — integrity findings over the workspace graph, from a root.
//!
//! The sleeper feature (DESIGN §8): walk the spanning tree and report every
//! violated invariant as a [`Finding`] — data, not a panic. The checks:
//!
//! - **broken link** — a relation target that resolves to nothing on disk;
//! - **case mismatch** — a target that only resolves because the filesystem is
//!   case-insensitive (`docs/design.md` vs `docs/DESIGN.md`): works on macOS,
//!   breaks on Linux. Caught by comparing exact directory listings;
//! - **cycle / duplicate containment** — a spanning target already visited
//!   (the spanning relation must be a single-parent tree);
//! - **missing inverse** — a spanning child whose inverse field (`part_of`)
//!   does not point back at its parent;
//! - **unreadable** — a document that exists but cannot be read or parsed.
//!
//! External targets (URLs, `mailto:`) are never checked. Autofix comes with
//! the mutation layer's growth; findings first.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::fs::Storage;
use crate::identity::{self, Id};
use crate::index::IndexStore;
use crate::link::{self, Link};
use crate::meta::Value;
use crate::workspace::{Target, Workspace};

/// One integrity finding. `doc` is always the document that *declares* the
/// problem (workspace-relative).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// `target` (declared under `relation`) resolves to nothing on disk.
    BrokenLink { doc: PathBuf, relation: String, target: String },
    /// `target` only resolves case-insensitively; the exact on-disk name is
    /// `actual`. Portable workspaces need the exact name.
    CaseMismatch { doc: PathBuf, relation: String, target: String, actual: String },
    /// A spanning target that was already reached — a containment cycle or a
    /// second parent, either of which breaks the single-parent spanning tree.
    DuplicateContainment { doc: PathBuf, target: String },
    /// A spanning child whose inverse field does not link back to `doc`.
    MissingInverse { doc: PathBuf, child: PathBuf, inverse: String },
    /// A document that exists but could not be read or parsed.
    Unreadable { doc: PathBuf, error: String },
    /// A `colophon:<id>` reference whose ID fails the shape/check-character
    /// test — almost certainly a typo, caught before it dangles silently.
    MalformedId { doc: PathBuf, relation: String, target: String },
    /// A well-formed `colophon:<id>` reference with no live registry entry.
    /// `tombstoned` distinguishes "that document was deleted" from "this ID
    /// was never issued here" (an out-of-band reference the registry has not
    /// reconciled — DESIGN §4's known hazard).
    DanglingId { doc: PathBuf, relation: String, id: Id, tombstoned: bool },
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Finding::BrokenLink { doc, relation, target } => {
                write!(f, "{}: broken {relation} link: {target}", doc.display())
            }
            Finding::CaseMismatch { doc, relation, target, actual } => write!(
                f,
                "{}: case mismatch in {relation} link: {target} is {actual} on disk",
                doc.display()
            ),
            Finding::DuplicateContainment { doc, target } => write!(
                f,
                "{}: {target} is already contained elsewhere (cycle or second parent)",
                doc.display()
            ),
            Finding::MissingInverse { doc, child, inverse } => write!(
                f,
                "{}: child {} does not declare {inverse} back to it",
                doc.display(),
                child.display()
            ),
            Finding::Unreadable { doc, error } => {
                write!(f, "{}: unreadable: {error}", doc.display())
            }
            Finding::MalformedId { doc, relation, target } => write!(
                f,
                "{}: malformed ID in {relation} link: {target} (bad shape or check character)",
                doc.display()
            ),
            Finding::DanglingId { doc, relation, id, tombstoned } => write!(
                f,
                "{}: dangling {relation} ID: colophon:{id} ({})",
                doc.display(),
                if *tombstoned { "document was deleted" } else { "never issued in this registry" }
            ),
        }
    }
}

impl<FS: Storage, IdP, Ix: IndexStore> Workspace<FS, IdP, Ix> {
    /// Check the workspace reachable from `start`, returning every finding.
    /// An empty result means the reachable graph holds its invariants.
    /// `colophon:<id>` targets resolve through the registry; malformed and
    /// dangling IDs are findings of their own.
    pub async fn check(&self, start: impl AsRef<Path>) -> Result<Vec<Finding>> {
        let start = link::normalize(start);
        let mut findings = Vec::new();
        let mut visited = BTreeSet::new();
        let mut queue = vec![start];

        let spanning = self.relations().spanning_relation().map(str::to_owned);
        let inverse = spanning.as_deref().and_then(|s| {
            self.relations()
                .relations()
                .iter()
                .find(|r| r.name == s)
                .and_then(|r| r.inverse.clone())
        });

        while let Some(path) = queue.pop() {
            if !visited.insert(path.clone()) {
                continue;
            }
            let doc = match self.load(&path).await {
                Ok((_, doc)) => doc,
                Err(e) => {
                    findings.push(Finding::Unreadable { doc: path, error: e.to_string() });
                    continue;
                }
            };

            for edge in self.relations().edges(&doc.meta) {
                let target = Link::parse(&edge.target);
                let resolved = match self.resolve_link(&path, &target) {
                    Target::External => continue,
                    Target::UnresolvedId(id) => {
                        findings.push(if identity::verify(id.as_str()) {
                            Finding::DanglingId {
                                doc: path.clone(),
                                relation: edge.relation.clone(),
                                tombstoned: self.index().is_known(&id),
                                id,
                            }
                        } else {
                            Finding::MalformedId {
                                doc: path.clone(),
                                relation: edge.relation.clone(),
                                target: target.target.clone(),
                            }
                        });
                        continue;
                    }
                    Target::Path(p) => p,
                };
                match self.exact_name(&resolved).await {
                    NameMatch::Exact => {}
                    NameMatch::CaseOnly(actual) => {
                        findings.push(Finding::CaseMismatch {
                            doc: path.clone(),
                            relation: edge.relation.clone(),
                            target: target.target.clone(),
                            actual,
                        });
                    }
                    NameMatch::None => {
                        findings.push(Finding::BrokenLink {
                            doc: path.clone(),
                            relation: edge.relation.clone(),
                            target: target.target.clone(),
                        });
                        continue;
                    }
                }

                if Some(edge.relation.as_str()) != spanning.as_deref() {
                    continue;
                }
                // Spanning target: single-parent check, inverse check, descent.
                if visited.contains(&resolved) || queue.contains(&resolved) {
                    findings.push(Finding::DuplicateContainment {
                        doc: path.clone(),
                        target: target.target.clone(),
                    });
                    continue;
                }
                if let Some(inverse) = inverse.as_deref()
                    && let Ok((_, child_doc)) = self.load(&resolved).await
                    && child_doc.has_meta()
                {
                    let points_back = child_doc
                        .meta
                        .get(inverse)
                        .map(Value::link_strings)
                        .unwrap_or_default()
                        .iter()
                        .any(|t| {
                            self.resolve_link(&resolved, &Link::parse(t)) == Target::Path(path.clone())
                        });
                    if !points_back {
                        findings.push(Finding::MissingInverse {
                            doc: path.clone(),
                            child: resolved.clone(),
                            inverse: inverse.to_string(),
                        });
                    }
                }
                queue.push(resolved);
            }
        }
        Ok(findings)
    }

    /// How `path`'s final component matches its parent directory's listing:
    /// exactly, only case-insensitively (the portability hazard), or not at all.
    async fn exact_name(&self, path: &Path) -> NameMatch {
        let full = self.root().join(path);
        let (Some(parent), Some(name)) = (full.parent(), full.file_name()) else {
            return NameMatch::None;
        };
        let Ok(entries) = self.fs().read_dir(parent).await else {
            return NameMatch::None;
        };
        let mut case_only = None;
        for entry in entries {
            let Some(entry_name) = entry.file_name() else { continue };
            if entry_name == name {
                return NameMatch::Exact;
            }
            if entry_name.eq_ignore_ascii_case(name) {
                case_only = Some(entry_name.to_string_lossy().into_owned());
            }
        }
        match case_only {
            Some(actual) => NameMatch::CaseOnly(actual),
            None => NameMatch::None,
        }
    }
}

enum NameMatch {
    Exact,
    CaseOnly(String),
    None,
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
        let dir = std::env::temp_dir().join(format!("colophon-check-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_clean_workspace_has_no_findings() {
        let dir = tempdir("clean");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n---\n");
        write(&dir, "a.md", "---\npart_of: index.md\n---\n");
        let ws = Workspace::builder(StdFs).root(&dir).build();
        assert_eq!(block_on(ws.check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn broken_case_mismatched_and_uninversed_links_are_found() {
        let dir = tempdir("dirty");
        write(
            &dir,
            "index.md",
            "---\ncontents:\n- gone.md\n- '[D](docs/design.md)'\n- b.md\n---\n",
        );
        write(&dir, "docs/DESIGN.md", "---\npart_of: ../index.md\n---\n");
        write(&dir, "b.md", "---\ntitle: no part_of here\n---\n");

        let ws = Workspace::builder(StdFs).root(&dir).build();
        let findings = block_on(ws.check("index.md")).unwrap();
        assert!(
            findings.iter().any(|f| matches!(f, Finding::BrokenLink { target, .. } if target == "gone.md")),
            "{findings:?}"
        );
        assert!(
            findings.iter().any(|f| matches!(
                f,
                Finding::CaseMismatch { target, actual, .. } if target == "docs/design.md" && actual == "DESIGN.md"
            )),
            "{findings:?}"
        );
        assert!(
            findings.iter().any(|f| matches!(
                f,
                Finding::MissingInverse { child, .. } if child == &PathBuf::from("b.md")
            )),
            "{findings:?}"
        );
    }

    #[test]
    fn duplicate_containment_is_found() {
        let dir = tempdir("dup");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n- b.md\n---\n");
        write(&dir, "a.md", "---\npart_of: index.md\ncontents:\n- b.md\n---\n");
        write(&dir, "b.md", "---\npart_of: index.md\n---\n");
        let ws = Workspace::builder(StdFs).root(&dir).build();
        let findings = block_on(ws.check("index.md")).unwrap();
        assert!(
            findings.iter().any(|f| matches!(f, Finding::DuplicateContainment { .. })),
            "{findings:?}"
        );
    }
}
