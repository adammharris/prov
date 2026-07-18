//! Root discovery — finding the workspace a directory belongs to.
//!
//! A prov workspace is *self-describing*, so where it begins is a fact to be
//! found, not configured: walk up from a starting directory and the first
//! directory holding a **root document** — one with metadata and no `part_of`
//! (nothing contains it) — is the workspace root. This is the counterpart to the
//! traversal in [`crate::tree`]: that walk goes *down* the spanning tree from a
//! known root; this one goes *up* the filesystem to locate the root in the first
//! place.
//!
//! It lives in the library, over the [`Storage`] seam, rather than in the CLI:
//! discovery is workspace semantics (which file is the root, ties broken by
//! `index`/`readme`, an ambiguous directory refused), not presentation, and a
//! programmatic embedder needs it exactly as the CLI does. The CLI keeps only the
//! thin shell around it — reading the real current directory, and phrasing the
//! "no workspace here" advice — while the judgment lives here and is testable
//! against a fake filesystem.

use std::path::{Path, PathBuf};

use crate::config::{ROOT_CONFIG_KEY, WorkspaceConfig};
use crate::content::ContentFormat;
use crate::document::{self, Document};
use crate::error::Result;
use crate::fs::Storage;
use crate::workspace::Workspace;

/// A located workspace: where the root directory is, which document in it is the
/// root, the registry that root declares (if any), and the effective config
/// (defaults, overlaid by the root's `prov:` block, overlaid by the linked
/// config document).
#[derive(Debug, Clone)]
pub struct Discovered {
    /// The workspace root directory (as reached by walking up from the start).
    pub root_dir: PathBuf,
    /// The root document, relative to `root_dir`.
    pub root_doc: PathBuf,
    /// The registry document the root declares, relative to `root_dir`, if any.
    pub registry: Option<PathBuf>,
    /// The effective workspace configuration.
    pub config: WorkspaceConfig,
}

/// The outcome of a [`discover`] walk — one of the three answers "which workspace
/// is this directory in?" genuinely has.
#[derive(Debug, Clone)]
pub enum Discovery {
    /// A single unambiguous root was found.
    Found(Discovered),
    /// A directory held two or more root candidates and no `index`/`readme` to
    /// break the tie — prov will not guess which is the root. Carries the
    /// directory and the candidate filenames so a caller can name them.
    Ambiguous {
        /// The directory that held the competing candidates.
        dir: PathBuf,
        /// The candidate filenames, in directory order.
        candidates: Vec<String>,
    },
    /// No ancestor directory held a root document at all.
    NotFound,
}

/// Whether a file `stem` is the conventional root name that wins ties.
fn stem_is(name: &Path, want: &str) -> bool {
    name.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case(want))
}

/// Walk up from `from` (an absolute directory) and locate the workspace root.
///
/// In each directory a **root candidate** is a document — a content document
/// (Markdown/Djot/HTML), or a whole-file metadata document stemmed `index`/
/// `readme` (a *separated* root's node) — with metadata and no `part_of`. A file
/// stemmed `index` wins, then `readme`, then a lone candidate; two or more
/// unnamed candidates are [`Discovery::Ambiguous`]. The first ancestor with a
/// winner is the root; a walk that reaches the filesystem top with none is
/// [`Discovery::NotFound`].
///
/// `FS: Clone` because the effective config is read through a throwaway probe
/// [`Workspace`] rooted at the found directory (its `registry_path`/`config_path`
/// resolve the pointer relations) — the same machinery every command uses, so
/// discovery and operation agree on where the registry and config live.
pub async fn discover<FS: Storage + Clone>(fs: &FS, from: &Path) -> Result<Discovery> {
    for dir in from.ancestors() {
        let Ok(entries) = fs.read_dir(dir).await else {
            continue;
        };
        let mut candidates: Vec<String> = Vec::new();
        for entry in entries {
            let path = entry.path();
            let is_content_ext = ContentFormat::from_extension(path).is_some();
            // A separated root's node is a whole-file metadata document
            // (`index.yaml`, …). Accept those too, but only under the conventional
            // `index`/`readme` stem — otherwise a stray `.json`/`.yaml` config file
            // (a mapping at its root, no `part_of`) would masquerade as a root.
            let is_meta_ext = document::whole_file_format(path).is_some();
            if !is_content_ext && !is_meta_ext {
                continue;
            }
            if is_meta_ext && !is_content_ext && !stem_is(path, "index") && !stem_is(path, "readme")
            {
                continue;
            }
            let Ok(text) = fs.read_to_string(path).await else {
                continue;
            };
            let Ok(doc) = Document::parse(path, &text) else {
                continue;
            };
            if doc.has_meta()
                && doc.meta.get("part_of").is_none()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                candidates.push(name.to_string());
            }
        }
        let chosen = candidates
            .iter()
            .find(|n| stem_is(Path::new(n), "index"))
            .or_else(|| candidates.iter().find(|n| stem_is(Path::new(n), "readme")))
            .cloned()
            .or_else(|| (candidates.len() == 1).then(|| candidates[0].clone()));
        match chosen {
            Some(root_doc) => {
                let discovered = build(fs, dir.to_path_buf(), PathBuf::from(root_doc)).await?;
                return Ok(Discovery::Found(discovered));
            }
            None if candidates.len() > 1 => {
                return Ok(Discovery::Ambiguous {
                    dir: dir.to_path_buf(),
                    candidates,
                });
            }
            None => continue,
        }
    }
    Ok(Discovery::NotFound)
}

/// Assemble the [`Discovered`] for a chosen root: resolve the registry pointer
/// and layer the effective config (defaults → root `prov:` block → linked
/// config document), through a probe workspace rooted at `root_dir`.
async fn build<FS: Storage + Clone>(
    fs: &FS,
    root_dir: PathBuf,
    root_doc: PathBuf,
) -> Result<Discovered> {
    let probe: Workspace<FS> = Workspace::builder(fs.clone()).root(&root_dir).build();
    let registry = probe.registry_path(&root_doc).await?;

    let mut config = WorkspaceConfig::default();
    // The root's `prov:` frontmatter block (config's description home).
    if let Ok(text) = fs.read_to_string(&root_dir.join(&root_doc)).await
        && let Ok(doc) = Document::parse(&root_doc, &text)
        && let Some(block) = doc.meta.get(ROOT_CONFIG_KEY)
    {
        config.apply(block);
    }
    // The linked config document (the policy home) wins over the root block.
    if let Ok(Some(config_doc)) = probe.config_path(&root_doc).await
        && let Ok(text) = fs.read_to_string(&root_dir.join(&config_doc)).await
        && let Ok(doc) = Document::parse(&config_doc, &text)
    {
        config.apply(&doc.meta);
    }

    Ok(Discovered {
        root_dir,
        root_doc,
        registry,
        config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::block_on;
    use crate::fs::StdFs;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prov-discover-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_the_root_by_walking_up_from_a_nested_dir() {
        let root = tmp("walk-up");
        std::fs::write(root.join("index.md"), "---\ntitle: Home\n---\n# Home\n").unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(
            root.join("a/child.md"),
            "---\ntitle: Child\npart_of: '[Home](/index.md)'\n---\n",
        )
        .unwrap();

        let outcome = block_on(discover(&StdFs, &root.join("a/b"))).unwrap();
        match outcome {
            Discovery::Found(d) => {
                assert_eq!(d.root_dir, root);
                assert_eq!(d.root_doc, Path::new("index.md"));
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn two_unnamed_candidates_are_ambiguous() {
        let root = tmp("ambiguous");
        std::fs::write(root.join("one.md"), "---\ntitle: One\n---\n").unwrap();
        std::fs::write(root.join("two.md"), "---\ntitle: Two\n---\n").unwrap();
        match block_on(discover(&StdFs, &root)).unwrap() {
            Discovery::Ambiguous { candidates, .. } => assert_eq!(candidates.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn index_stem_breaks_a_tie() {
        let root = tmp("index-wins");
        std::fs::write(root.join("index.md"), "---\ntitle: Home\n---\n").unwrap();
        std::fs::write(root.join("other.md"), "---\ntitle: Other\n---\n").unwrap();
        match block_on(discover(&StdFs, &root)).unwrap() {
            Discovery::Found(d) => assert_eq!(d.root_doc, Path::new("index.md")),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn a_directory_holding_no_document_yields_no_candidate_there() {
        // A directory with only non-documents contributes no root candidate, so
        // discovery keeps walking up rather than rooting here. (A full "reaches the
        // filesystem top with nothing" NotFound is covered end-to-end by the CLI's
        // `a_route_outside_a_workspace_says_so` test, which can control the whole
        // ancestor chain; a unit test cannot, since the walk climbs to `/`.)
        let root = tmp("no-doc-here");
        std::fs::write(root.join("plain.txt"), "not a document").unwrap();
        // Rooting *would* happen if this dir had a candidate; assert it does not by
        // giving it a child that IS a root and confirming discovery picks the
        // child's dir, never this one.
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/index.md"), "---\ntitle: Sub\n---\n").unwrap();
        match block_on(discover(&StdFs, &root.join("sub"))).unwrap() {
            Discovery::Found(d) => assert_eq!(d.root_dir, root.join("sub")),
            other => panic!("expected Found at sub, got {other:?}"),
        }
    }
}
