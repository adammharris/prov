//! Directory-tree import — folding a folder hierarchy into the containment tree.
//!
//! This is the `mirror` strategy of `docs/init-adoption.md` (Phase 2): every
//! directory that holds content becomes a containment **node** — its own
//! `index`/`readme` document when one exists, otherwise a *synthesized*
//! folder-note stub — and each content file is linked under its directory's
//! node, each folder node under its parent's. Following the resulting links
//! reproduces the filesystem, at the cost of minting an index document for every
//! bare folder.
//!
//! The work splits in two so a caller can preview before it writes:
//! [`Workspace::plan_mirror`] walks the tree and returns a [`StructurePlan`]
//! without touching disk; [`Workspace::apply_plan`] realizes it, reusing
//! [`create`](Workspace::create) for the synthesized folder-notes (which mints
//! the stub and links it both ways) and [`adopt`](Workspace::adopt) for the
//! existing files (additive, idempotent, body untouched).
//!
//! This is the concrete `FilesystemSource` the design sketches as a
//! `StructureSource`; the trait itself is deferred until a second source (a
//! frontmatter-only or hybrid intake) needs it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::document::MetaCarrier;
use crate::error::{Error, Result};
use crate::fs::Storage;
use crate::identity::IdentityPolicy;
use crate::index::IndexStore;
use crate::link;
use crate::workspace::Workspace;

/// A plan to fold a directory tree into the containment tree — the `mirror`
/// strategy from `docs/init-adoption.md`. Produced by [`Workspace::plan_mirror`]
/// without touching disk, applied by [`Workspace::apply_plan`]; inspect it in
/// between for a dry-run preview.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructurePlan {
    /// Folder-note nodes to create for bare directories, **parents-first** so a
    /// nested folder-note's parent already exists when it is created. Applied
    /// with [`create`](Workspace::create).
    pub synthesized: Vec<SynthNode>,
    /// Existing files to link under a node. Covers each directory's own existing
    /// `index`/`readme` (linked under the *parent* directory's node) and every
    /// other content file (linked under its own directory's node). Applied with
    /// [`adopt`](Workspace::adopt).
    pub adoptions: Vec<Adoption>,
}

impl StructurePlan {
    /// Nothing to create and nothing to adopt — the directory holds no content
    /// beyond the root.
    ///
    /// A `mirror` import folds only *documents* into the tree; loose opaque files
    /// (images, PDFs, binaries) are deliberately left alone — attaching them is a
    /// separate, explicit act ([`attach`](Workspace::attach) /
    /// [`loose_attachments`](Workspace::loose_attachments)), never inferred in
    /// bulk from a directory walk.
    pub fn is_empty(&self) -> bool {
        self.synthesized.is_empty() && self.adoptions.is_empty()
    }
}

/// A folder-note node the mirror plan will create for a directory that has no
/// existing `index`/`readme` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthNode {
    /// Workspace-relative path of the stub to create (e.g. `notes/index.md`).
    pub path: PathBuf,
    /// The parent directory's node it is created under.
    pub parent: PathBuf,
    /// The title to give it — the folder's name, titleized. ([`create`] alone
    /// would title the stub after its `index` file stem.)
    ///
    /// [`create`]: Workspace::create
    pub title: String,
}

/// A containment edge in a [`StructurePlan`]: link the existing document
/// `child` under `parent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    pub child: PathBuf,
    pub parent: PathBuf,
}

/// What [`Workspace::apply_plan`] did: the folder-notes it created, the files it
/// adopted, and any adoptions it had to skip (a contested parent, a vanished
/// file) with the reason — so a fresh import never aborts halfway over one
/// stubborn file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanOutcome {
    /// Folder-note stubs created.
    pub synthesized: Vec<PathBuf>,
    /// Existing files linked into the tree.
    pub adopted: Vec<PathBuf>,
    /// Files an adoption declined, each with the reason.
    pub skipped: Vec<(PathBuf, String)>,
}

/// The directory node for a set of files: an `index`-stemmed document wins, then
/// a `readme`-stemmed one; `None` when the directory has neither (a folder-note
/// must be synthesized). Mirrors the CLI's `pick_root_candidate` at directory
/// scope.
fn existing_node(files: &[PathBuf]) -> Option<PathBuf> {
    let stem_is = |p: &Path, want: &str| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case(want))
    };
    files
        .iter()
        .find(|p| stem_is(p, "index"))
        .or_else(|| files.iter().find(|p| stem_is(p, "readme")))
        .cloned()
}

impl<FS: Storage, Id, Ix: IndexStore> Workspace<FS, Id, Ix> {
    /// Compute a [`StructurePlan`] that mirrors the on-disk directory tree under
    /// `root_doc` into the containment tree, without mutating anything. Every
    /// directory holding content (directly or beneath) becomes a node: its own
    /// `index`/`readme` when present, else a synthesized folder-note. `root_doc`
    /// and — trivially — its own directory are the tree's root.
    ///
    /// Refuses a **separated** or whole-file root: folder-note synthesis assumes
    /// a combined content grammar (so the node *is* the content file), which a
    /// caller can always fall back to with flat adoption.
    pub async fn plan_mirror(&self, root_doc: &Path) -> Result<StructurePlan> {
        let root_doc = link::normalize(root_doc);
        let (_, root) = self.load(&root_doc).await?;
        if root.content_attr().is_some() || matches!(root.carrier, Some(MetaCarrier::WholeFile(_)))
        {
            return Err(Error::Structure(
                "mirror import needs a combined-document root (folder-notes inherit its \
                 grammar); re-run with flat adoption instead"
                    .into(),
            ));
        }
        // Folder-notes are minted in the root's own content grammar.
        let ext = root_doc
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("md")
            .to_string();

        // Every content document, minus the root itself (neither is loose content
        // to re-file).
        let files: Vec<PathBuf> = self
            .content_documents()
            .await?
            .into_iter()
            .filter(|p| *p != root_doc)
            .collect();

        // Group files by directory, and collect every directory on the path to
        // some content (each file's parent and all its ancestors up to the root).
        let mut by_dir: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
        dirs.insert(PathBuf::new()); // the root directory
        for file in &files {
            let dir = file.parent().unwrap_or(Path::new("")).to_path_buf();
            by_dir.entry(dir.clone()).or_default().push(file.clone());
            let mut d = dir;
            while !d.as_os_str().is_empty() {
                dirs.insert(d.clone());
                d = d.parent().unwrap_or(Path::new("")).to_path_buf();
            }
        }

        // Each directory's node: `root_doc` for the root, an existing
        // `index`/`readme` where present, else a synthesized folder-note.
        let mut node: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        let mut synth_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        for dir in &dirs {
            if dir.as_os_str().is_empty() {
                node.insert(dir.clone(), root_doc.clone());
                continue;
            }
            match by_dir.get(dir).and_then(|files| existing_node(files)) {
                Some(n) => {
                    node.insert(dir.clone(), n);
                }
                None => {
                    node.insert(
                        dir.clone(),
                        link::normalize(dir.join(format!("index.{ext}"))),
                    );
                    synth_dirs.insert(dir.clone());
                }
            }
        }

        // Synthesized folder-notes, parents-first (shallower directories carry
        // fewer path components), so a nested stub's parent exists when created.
        let mut synth_sorted: Vec<&PathBuf> = synth_dirs.iter().collect();
        synth_sorted.sort_by_key(|d| d.components().count());
        let synthesized: Vec<SynthNode> = synth_sorted
            .into_iter()
            .map(|dir| SynthNode {
                path: node[dir].clone(),
                parent: node[dir.parent().unwrap_or(Path::new(""))].clone(),
                title: link::path_to_title(dir),
            })
            .collect();

        // Adoptions: (a) each directory's *existing* node under its parent's node,
        // then (b) every non-node content file under its own directory's node.
        let mut adoptions: Vec<Adoption> = Vec::new();
        for dir in &dirs {
            if dir.as_os_str().is_empty() || synth_dirs.contains(dir) {
                continue;
            }
            adoptions.push(Adoption {
                child: node[dir].clone(),
                parent: node[dir.parent().unwrap_or(Path::new(""))].clone(),
            });
        }
        for (dir, dir_files) in &by_dir {
            let n = &node[dir];
            for file in dir_files {
                if file != n {
                    adoptions.push(Adoption {
                        child: file.clone(),
                        parent: n.clone(),
                    });
                }
            }
        }

        Ok(StructurePlan {
            synthesized,
            adoptions,
        })
    }
}

impl<FS: Storage, IdP: IdentityPolicy, Ix: IndexStore> Workspace<FS, IdP, Ix> {
    /// Apply a [`StructurePlan`]: create every synthesized folder-note (linking
    /// it under its parent and retitling the stub after its folder), then adopt
    /// every existing file under its node. A folder-note creation failure aborts
    /// (a structural problem); an individual adoption that is refused — a
    /// contested parent, a file that vanished — is recorded in
    /// [`PlanOutcome::skipped`] and the import continues.
    pub async fn apply_plan(&mut self, plan: &StructurePlan) -> Result<PlanOutcome> {
        let mut outcome = PlanOutcome::default();
        for synth in &plan.synthesized {
            // Title the stub after its folder (not its `index` stem), so its own
            // title and the parent's spanning-entry label are authored in step.
            self.create_titled(&synth.path, &synth.parent, Some(&synth.title))
                .await?;
            outcome.synthesized.push(synth.path.clone());
        }
        for edge in &plan.adoptions {
            match self.adopt(&edge.child, &edge.parent).await {
                Ok(()) => outcome.adopted.push(edge.child.clone()),
                Err(e) => outcome.skipped.push((edge.child.clone(), e.to_string())),
            }
        }
        Ok(outcome)
    }
}

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

    fn read(dir: &Path, rel: &str) -> String {
        std::fs::read_to_string(dir.join(rel)).unwrap()
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prov-intake-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ws(dir: &Path) -> Workspace<StdFs> {
        Workspace::builder(StdFs).root(dir).build()
    }

    #[test]
    fn mirror_synthesizes_a_folder_note_for_a_bare_directory() {
        // A loose vault: a root plus a `notes/` folder of two files, no index.
        let dir = tempdir("mirror-synth");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        write(&dir, "notes/one.md", "---\ntitle: One\n---\nfirst\n");
        write(&dir, "notes/two.md", "---\ntitle: Two\n---\nsecond\n");

        let plan = block_on(ws(&dir).plan_mirror(Path::new("index.md"))).unwrap();
        // One synthesized folder-note, titled after the folder, under the root.
        assert_eq!(plan.synthesized.len(), 1);
        assert_eq!(plan.synthesized[0].path, PathBuf::from("notes/index.md"));
        assert_eq!(plan.synthesized[0].parent, PathBuf::from("index.md"));
        assert_eq!(plan.synthesized[0].title, "Notes");
        // Both files adopt under the (to-be-created) folder-note.
        assert!(plan.adoptions.iter().any(
            |a| a.child == Path::new("notes/one.md") && a.parent == Path::new("notes/index.md")
        ));
        assert!(plan.adoptions.iter().any(
            |a| a.child == Path::new("notes/two.md") && a.parent == Path::new("notes/index.md")
        ));

        let outcome = block_on(ws(&dir).apply_plan(&plan)).unwrap();
        assert_eq!(outcome.synthesized, vec![PathBuf::from("notes/index.md")]);
        assert_eq!(outcome.adopted.len(), 2);
        assert!(outcome.skipped.is_empty());

        // The folder-note exists, is titled "Notes", and links up to the root.
        let folder = read(&dir, "notes/index.md");
        assert!(folder.contains("title: Notes"), "{folder}");
        assert!(folder.contains("/index.md"), "part_of the root: {folder}");
        // The root contains the folder-note under its folder title (not the stale
        // `index` stem), because the title is authored before the down-link label.
        assert!(
            read(&dir, "index.md").contains("[Notes](/notes/index.md)"),
            "root's contents entry uses the folder title as its label: {}",
            read(&dir, "index.md")
        );
        assert!(
            folder.contains("one.md") && folder.contains("two.md"),
            "{folder}"
        );
        // Files keep their bodies and gain part_of up to the folder-note.
        assert!(
            read(&dir, "notes/one.md").contains("first"),
            "body preserved"
        );
        // The whole imported tree validates — nothing orphaned, no missing inverse.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn mirror_uses_an_existing_folder_index_instead_of_synthesizing() {
        // `notes/` already has its own index — the mirror reuses it as the node
        // rather than minting a competitor.
        let dir = tempdir("mirror-existing");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        write(
            &dir,
            "notes/index.md",
            "---\ntitle: Notes Home\n---\nfolder intro\n",
        );
        write(&dir, "notes/leaf.md", "---\ntitle: Leaf\n---\nleaf\n");

        let plan = block_on(ws(&dir).plan_mirror(Path::new("index.md"))).unwrap();
        assert!(
            plan.synthesized.is_empty(),
            "existing index means nothing to synthesize"
        );
        // The existing folder index adopts under the root; the leaf adopts under it.
        assert!(
            plan.adoptions.iter().any(
                |a| a.child == Path::new("notes/index.md") && a.parent == Path::new("index.md")
            )
        );
        assert!(
            plan.adoptions
                .iter()
                .any(|a| a.child == Path::new("notes/leaf.md")
                    && a.parent == Path::new("notes/index.md"))
        );

        block_on(ws(&dir).apply_plan(&plan)).unwrap();
        assert!(
            read(&dir, "notes/index.md").contains("folder intro"),
            "existing index body kept"
        );
        assert!(
            read(&dir, "notes/index.md").contains("title: Notes Home"),
            "existing title kept"
        );
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn mirror_nests_folder_notes_for_a_deep_tree() {
        // Content only at the leaf: intermediate directories still become nodes,
        // parents-first, so following links reproduces the path.
        let dir = tempdir("mirror-deep");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        write(&dir, "a/b/deep.md", "---\ntitle: Deep\n---\ndeep\n");

        let plan = block_on(ws(&dir).plan_mirror(Path::new("index.md"))).unwrap();
        // Two synthesized nodes, shallower first.
        let synth: Vec<_> = plan.synthesized.iter().map(|n| n.path.clone()).collect();
        assert_eq!(
            synth,
            vec![PathBuf::from("a/index.md"), PathBuf::from("a/b/index.md")]
        );

        block_on(ws(&dir).apply_plan(&plan)).unwrap();
        assert!(
            read(&dir, "a/index.md").contains("a/b/index.md"),
            "a contains a/b"
        );
        assert!(
            read(&dir, "a/b/index.md").contains("deep.md"),
            "a/b contains the leaf"
        );
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn mirror_leaves_loose_binaries_alone() {
        // A vault with a note tree and loose binaries: `mirror` folds the
        // documents in but never sidecars the binaries — attaching them is a
        // separate, explicit act, not inferred from a directory walk.
        let dir = tempdir("mirror-no-attach");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        write(&dir, "notes/one.md", "---\ntitle: One\n---\nfirst\n");
        write(&dir, "cover.jpg", "\u{fffd}binary");
        write(&dir, "src/main.rs", "fn main() {}\n");

        let plan = block_on(ws(&dir).plan_mirror(Path::new("index.md"))).unwrap();
        block_on(ws(&dir).apply_plan(&plan)).unwrap();

        // The document folded in; neither binary got a sidecar.
        assert!(read(&dir, "notes/index.md").contains("one.md"));
        assert!(
            !dir.join("cover.jpg.yaml").exists(),
            "no sidecar for the image"
        );
        assert!(
            !dir.join("src/index.md").exists(),
            "an all-code directory gets no folder-note"
        );
        // The binaries are simply not prov's concern — check is clean, no orphan.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn mirror_refuses_a_separated_root() {
        let dir = tempdir("mirror-separated");
        write(&dir, "index.yaml", "title: Root\ncontent: index.md\n");
        write(&dir, "index.md", "# Root\n");
        write(&dir, "loose.md", "---\ntitle: Loose\n---\n");
        let err = block_on(ws(&dir).plan_mirror(Path::new("index.yaml"))).unwrap_err();
        assert!(err.to_string().contains("combined-document root"), "{err}");
    }
}
