//! Mutation with link maintenance — the crate's hard, valuable half.
//!
//! Creating, moving, and deleting a document are never single-file operations
//! in a linked workspace: the spanning relation and its inverse live in *other*
//! documents, and every touched link must keep pointing at the truth. Each op
//! here computes the full set of affected documents, edits their metadata with
//! fig's comment-preserving [`fig::Embed`] editor (byte-minimal diffs, fence
//! style and format untouched, labels on `[label](path)` links kept), and only
//! then touches the filesystem.
//!
//! ## Identity is additive here (DESIGN §4)
//!
//! Everything below operates on paths and never *requires* an ID. When a
//! registry is present, each op additionally keeps it true — create registers
//! (if the policy's `on_create` fires), rename updates `id → path`, delete
//! tombstones — and a `colophon:<id>` entry in another document's metadata is
//! deliberately **not** rewritten by a move: the registry update is what keeps
//! it resolving, which is the entire point of linking by ID. With
//! [`crate::identity::NoIdentity`]/[`crate::index::NoIndex`] these hooks
//! monomorphize to nothing.
//!
//! The vocabulary is never hardcoded: the spanning relation and its inverse
//! come from the workspace's [`crate::relation::RelationSet`]. First cut:
//! documents only (no directory moves), and best-effort atomicity (edits are
//! computed before any write, but writes are not transactional).

use std::path::{Path, PathBuf};

use fig::{Embed, EmbedType, Segment};

use crate::document::Document;
use crate::error::{Error, Result};
use crate::fs::Storage;
use crate::identity::{IdentityPolicy, Trigger};
use crate::index::IndexStore;
use crate::link::{self, Link};
use crate::meta::Value;
use crate::workspace::{Target, Workspace};

impl<FS: Storage, IdP: IdentityPolicy, Ix: IndexStore> Workspace<FS, IdP, Ix> {
    /// Create a new document at `path` (workspace-relative) as a spanning child
    /// of `parent`: the new file declares the inverse link back to `parent`, in
    /// `parent`'s embed archetype, and `parent`'s spanning field gains `path`.
    /// If the identity policy registers on create, the new document is also
    /// assigned a stable ID.
    pub async fn create(&mut self, path: &Path, parent: &Path) -> Result<()> {
        let path = link::normalize(path);
        let parent = link::normalize(parent);
        let (spanning, inverse) = self.spanning_pair()?;

        if self.fs().try_exists(&self.root().join(&path)).await? {
            return Err(Error::Structure(format!("{} already exists", path.display())));
        }
        let (parent_text, parent_doc) = self.load(&parent).await?;
        let kind = parent_doc.embed.unwrap_or(EmbedType::FrontmatterYaml);

        // The new document: title from the file stem, inverse link to parent.
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let up = link::relative(path.parent().unwrap_or(Path::new("")), &parent);
        let mut new_doc = Embed::open_or_init(b"", kind)?;
        new_doc.set_value(&[Segment::Key("title")], fig::Value::Str(title))?;
        new_doc.set_value(&[Segment::Key(&inverse)], fig::Value::Str(up))?;
        let new_text = new_doc.render()?.to_string();

        // The parent: append the child to its spanning field (creating it if
        // absent — `append` needs an existing sequence).
        let down = link::relative(parent.parent().unwrap_or(Path::new("")), &path);
        let mut parent_embed = Embed::open_or_init(parent_text.as_bytes(), kind)?;
        let span_path = [Segment::Key(&spanning)];
        if parent_embed
            .append_value(&span_path, fig::Value::Str(down.clone()))
            .is_err()
        {
            parent_embed.set_value(&span_path, fig::Value::Seq(vec![fig::Value::Str(down)]))?;
        }
        let parent_out = parent_embed.render()?.to_string();

        if let Some(dir) = self.root().join(&path).parent() {
            self.fs().create_dir_all(dir).await?;
        }
        self.fs().write(&self.root().join(&path), new_text.as_bytes()).await?;
        self.fs().write(&self.root().join(&parent), parent_out.as_bytes()).await?;

        // Identity hook — eager policies assign an ID from birth.
        if self.identity().registration().fires_on(Trigger::Create) {
            let id = self.mint_unique(&path);
            self.index_mut().register(&id, &path);
        }
        Ok(())
    }

    /// Move/rename the document at `from` to `to`, maintaining every affected
    /// link: the parent's spanning entry, each spanning child's inverse link,
    /// and — when the directory changes — every relative link the document
    /// itself declares. Labels on `[label](path)` links are preserved.
    /// `colophon:<id>` entries pointing at the document are left untouched;
    /// the registry's `id → path` update keeps them resolving.
    pub async fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        let from = link::normalize(from);
        let to = link::normalize(to);
        let (spanning, inverse) = self.spanning_pair()?;

        if !self.fs().try_exists(&self.root().join(&from)).await? {
            return Err(Error::Structure(format!("{} does not exist", from.display())));
        }
        if self.fs().try_exists(&self.root().join(&to)).await? {
            return Err(Error::Structure(format!("{} already exists", to.display())));
        }
        let (from_text, from_doc) = self.load(&from).await?;

        // 1. The parent (via the doc's inverse link): retarget its spanning
        //    entry for `from` to reach `to`.
        let mut parent_write: Option<(PathBuf, String)> = None;
        if let Some(parent) = self.single_target(&from_doc, &inverse, &from) {
            let (parent_text, parent_doc) = self.load(&parent).await?;
            if let Some(updated) =
                self.retarget_entry(&parent_text, &parent_doc, &spanning, &parent, &from, &to)?
            {
                parent_write = Some((parent, updated));
            }
        }

        // 2. Each spanning child that exists: retarget its inverse link.
        let mut child_writes: Vec<(PathBuf, String)> = Vec::new();
        for raw in self.relations().children(&from_doc.meta) {
            let child = Link::parse(&raw);
            let child_path = match self.resolve_link(&from, &child) {
                Target::Path(p) => p,
                _ => continue,
            };
            let Ok((child_text, child_doc)) = self.load(&child_path).await else {
                continue; // broken link: nothing to maintain, `check` reports it
            };
            if let Some(updated) =
                self.retarget_entry(&child_text, &child_doc, &inverse, &child_path, &from, &to)?
            {
                child_writes.push((child_path, updated));
            }
        }

        // 3. The document itself: when its directory changes, every relative
        //    link it declares must be recomputed to keep resolving.
        let self_text = if from.parent() != to.parent() {
            rerelativize(&from_text, &from_doc, self.relations().relations(), &from, &to)?
        } else {
            from_text
        };

        // All edits computed; now write.
        if let Some(dir) = self.root().join(&to).parent() {
            self.fs().create_dir_all(dir).await?;
        }
        self.fs().rename(&self.root().join(&from), &self.root().join(&to)).await?;
        self.fs().write(&self.root().join(&to), self_text.as_bytes()).await?;
        if let Some((parent, text)) = parent_write {
            self.fs().write(&self.root().join(&parent), text.as_bytes()).await?;
        }
        for (child, text) in child_writes {
            self.fs().write(&self.root().join(&child), text.as_bytes()).await?;
        }

        // Identity hook — the registry follows the move, so every
        // `colophon:<id>` reference to this document survives untouched.
        if let Some(id) = self.index().id_for_path(&from) {
            self.index_mut().set_path(&id, &to);
        }
        Ok(())
    }

    /// Delete the document at `path`, removing the parent's spanning entry for
    /// it. Refuses when the document has spanning children (they would be
    /// orphaned) unless `force` is set. A registered ID is retired — with a
    /// tombstoning store it is never reissued, so dangling references stay
    /// diagnosable.
    pub async fn delete(&mut self, path: &Path, force: bool) -> Result<()> {
        let path = link::normalize(path);
        let (spanning, inverse) = self.spanning_pair()?;
        let (_, doc) = self.load(&path).await?;

        let children: Vec<String> = self
            .relations()
            .children(&doc.meta)
            .iter()
            .map(|raw| Link::parse(raw).target)
            .collect();
        if !children.is_empty() && !force {
            return Err(Error::Structure(format!(
                "{} contains {} document(s) ({}); delete them first or force",
                path.display(),
                children.len(),
                children.join(", ")
            )));
        }

        let mut parent_write: Option<(PathBuf, String)> = None;
        if let Some(parent) = self.single_target(&doc, &inverse, &path) {
            let (parent_text, parent_doc) = self.load(&parent).await?;
            let kind = parent_doc.embed.unwrap_or(EmbedType::FrontmatterYaml);
            if let Some(index) = self.entry_index(&parent_doc, &spanning, &parent, &path) {
                let mut embed = Embed::open(parent_text.as_bytes(), kind)?;
                embed.remove_item(&[Segment::Key(&spanning)], index)?;
                parent_write = Some((parent, embed.render()?.to_string()));
            }
        }

        self.fs().remove_file(&self.root().join(&path)).await?;
        if let Some((parent, text)) = parent_write {
            self.fs().write(&self.root().join(&parent), text.as_bytes()).await?;
        }

        // Identity hook — retire the ID (a tombstoning store keeps it known
        // forever, so it is never minted again to mean something else).
        if let Some(id) = self.index().id_for_path(&path) {
            self.index_mut().unregister(&id);
        }
        Ok(())
    }

    /// The spanning relation's name and its inverse — mutations need both.
    fn spanning_pair(&self) -> Result<(String, String)> {
        let spanning = self
            .relations()
            .spanning_relation()
            .ok_or_else(|| Error::Structure("no spanning relation configured".into()))?;
        let inverse = self
            .relations()
            .relations()
            .iter()
            .find(|r| r.name == spanning)
            .and_then(|r| r.inverse.clone())
            .ok_or_else(|| {
                Error::Structure(format!("spanning relation `{spanning}` has no inverse"))
            })?;
        Ok((spanning.to_string(), inverse))
    }

    /// The single resolved target of `field` in `doc`, if it resolves to an
    /// on-workspace path (by relative path or through the registry).
    /// (`doc_path` anchors a relative target.)
    fn single_target(&self, doc: &Document, field: &str, doc_path: &Path) -> Option<PathBuf> {
        let raw = doc.meta.get(field).map(Value::link_strings)?.into_iter().next()?;
        match self.resolve_link(doc_path, &Link::parse(&raw)) {
            Target::Path(p) => Some(p),
            _ => None,
        }
    }

    /// The index of the entry in `doc`'s `field` sequence whose target
    /// resolves to `wanted` — by relative path or through the registry.
    fn entry_index(&self, doc: &Document, field: &str, doc_path: &Path, wanted: &Path) -> Option<usize> {
        doc.meta
            .get(field)
            .map(Value::link_strings)?
            .iter()
            .position(|raw| {
                self.resolve_link(doc_path, &Link::parse(raw)) == Target::Path(wanted.to_path_buf())
            })
    }

    /// Rewrite the entry of `field` in `doc` whose target resolves to `old` so
    /// it reaches `new` instead, preserving the entry's label and the
    /// document's formatting. Returns the updated text, or `None` when no
    /// entry matches — or when the matching entry is a `colophon:<id>`
    /// reference, which needs no rewrite: the registry keeps it resolving.
    fn retarget_entry(
        &self,
        text: &str,
        doc: &Document,
        field: &str,
        doc_path: &Path,
        old: &Path,
        new: &Path,
    ) -> Result<Option<String>> {
        let Some(value) = doc.meta.get(field) else {
            return Ok(None);
        };
        let entries = value.link_strings();
        let dir = doc_path.parent().unwrap_or(Path::new(""));
        let Some(index) = entries.iter().position(|raw| {
            self.resolve_link(doc_path, &Link::parse(raw)) == Target::Path(old.to_path_buf())
        }) else {
            return Ok(None);
        };
        let entry = Link::parse(&entries[index]);
        if entry.id_target().is_some() {
            // Linked by ID: stable across the move by construction.
            return Ok(None);
        }
        let updated = entry.with_target(link::relative(dir, new));
        let kind = doc.embed.unwrap_or(EmbedType::FrontmatterYaml);
        let mut embed = Embed::open(text.as_bytes(), kind)?;
        // A scalar field is addressed by key; a sequence entry by key + index.
        if value.as_sequence().is_some() {
            embed.replace_value(
                &[Segment::Key(field), Segment::Index(index)],
                fig::Value::Str(updated.render()),
            )?;
        } else {
            embed.replace_value(&[Segment::Key(field)], fig::Value::Str(updated.render()))?;
        }
        Ok(Some(embed.render()?.to_string()))
    }
}

/// Recompute every relative link `doc` declares so it still resolves after the
/// document moves from `from` to `to`. External and `colophon:<id>` targets
/// are untouched — neither depends on where the document lives.
fn rerelativize(
    text: &str,
    doc: &Document,
    relations: &[crate::relation::Relation],
    from: &Path,
    to: &Path,
) -> Result<String> {
    let kind = doc.embed.unwrap_or(EmbedType::FrontmatterYaml);
    let mut embed = Embed::open(text.as_bytes(), kind)?;
    let new_dir = to.parent().unwrap_or(Path::new(""));
    for relation in relations {
        let Some(value) = doc.meta.get(&relation.name) else {
            continue;
        };
        let rewrite = |raw: &str| -> Option<String> {
            let target = Link::parse(raw);
            if target.is_external() || target.id_target().is_some() {
                return None;
            }
            let resolved = link::resolve(from, &target.target);
            Some(target.with_target(link::relative(new_dir, &resolved)).render())
        };
        match value {
            Value::String(raw) => {
                if let Some(updated) = rewrite(raw) {
                    embed.replace_value(&[Segment::Key(&relation.name)], fig::Value::Str(updated))?;
                }
            }
            Value::Sequence(items) => {
                for (i, item) in items.iter().enumerate() {
                    if let Some(raw) = item.as_str()
                        && let Some(updated) = rewrite(raw)
                    {
                        embed.replace_value(
                            &[Segment::Key(&relation.name), Segment::Index(i)],
                            fig::Value::Str(updated),
                        )?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(embed.render()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::block_on;
    use crate::fs::StdFs;
    use crate::identity::Minter;
    use crate::index::FileIndex;

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> String {
        std::fs::read_to_string(dir.join(rel)).unwrap()
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("colophon-mutate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ws(dir: &Path) -> Workspace<StdFs> {
        Workspace::builder(StdFs).root(dir).build()
    }

    /// An identity-bearing workspace: lazy minting, persistent-style index.
    fn id_ws(dir: &Path) -> Workspace<StdFs, Minter, FileIndex> {
        Workspace::builder(StdFs)
            .root(dir)
            .identity(Minter::lazy(42))
            .index(FileIndex::new(fig::Format::Yaml))
            .build()
    }

    #[test]
    fn create_links_both_directions_in_the_parents_format() {
        let dir = tempdir("create");
        write(&dir, "index.md", "```fig\ntitle = Root\n```\nbody\n");
        block_on(ws(&dir).create(Path::new("notes/new.md"), Path::new("index.md"))).unwrap();

        let child = read(&dir, "notes/new.md");
        assert!(child.starts_with("```fig\n"), "child inherits the parent's archetype: {child}");
        assert!(child.contains("part_of = ../index.md"), "{child}");
        let parent = read(&dir, "index.md");
        // fig ≥ 2.2 renders spliced containers as flow — the round-trippable
        // inline spelling.
        assert!(parent.contains("contents = [notes/new.md]"), "{parent}");
        assert!(parent.ends_with("body\n"), "body untouched: {parent}");
        // The result validates cleanly.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn rename_maintains_parent_children_and_own_links() {
        let dir = tempdir("rename");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- '[Mid](mid.md)'\n---\n",
        );
        write(
            &dir,
            "mid.md",
            "---\n# a comment to preserve\npart_of: index.md\ncontents:\n- leaf.md\n---\nmid body\n",
        );
        write(&dir, "leaf.md", "---\npart_of: mid.md\n---\n");

        block_on(ws(&dir).rename(Path::new("mid.md"), Path::new("sub/mid.md"))).unwrap();

        // Parent entry retargeted, label kept.
        let index = read(&dir, "index.md");
        assert!(index.contains("- '[Mid](sub/mid.md)'"), "{index}");
        // Child's inverse retargeted.
        let leaf = read(&dir, "leaf.md");
        assert!(leaf.contains("part_of: sub/mid.md"), "{leaf}");
        // The moved doc's own links re-relativized; comment and body kept.
        let mid = read(&dir, "sub/mid.md");
        assert!(mid.contains("part_of: ../index.md"), "{mid}");
        assert!(mid.contains("- ../leaf.md"), "{mid}");
        assert!(mid.contains("# a comment to preserve"), "{mid}");
        assert!(mid.ends_with("mid body\n"), "{mid}");
        // The whole workspace still validates.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn delete_refuses_children_then_forces() {
        let dir = tempdir("delete");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n- b.md\n---\n");
        write(&dir, "a.md", "---\npart_of: index.md\ncontents:\n- b.md\n---\n");
        write(&dir, "b.md", "---\npart_of: index.md\n---\n");

        let err = block_on(ws(&dir).delete(Path::new("a.md"), false)).unwrap_err();
        assert!(err.to_string().contains("contains 1 document"), "{err}");

        block_on(ws(&dir).delete(Path::new("a.md"), true)).unwrap();
        assert!(!dir.join("a.md").exists());
        let index = read(&dir, "index.md");
        assert!(!index.contains("a.md"), "parent entry removed: {index}");
        assert!(index.contains("- b.md"), "sibling kept: {index}");
    }

    #[test]
    fn create_refuses_an_existing_path() {
        let dir = tempdir("exists");
        write(&dir, "index.md", "---\ntitle: Root\n---\n");
        write(&dir, "a.md", "already here\n");
        let err = block_on(ws(&dir).create(Path::new("a.md"), Path::new("index.md"))).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    // ── identity: the additive layer, proven against the same ops ──────────

    #[test]
    fn id_links_survive_a_rename_without_any_text_edit() {
        let dir = tempdir("id-rename");
        write(&dir, "index.md", "---\ntitle: Root\ncontents:\n- a.md\n---\n");
        write(&dir, "a.md", "---\npart_of: index.md\n---\n");

        let mut w = id_ws(&dir);
        // Author a link-by-id: register, then write the id target into index.md.
        let id = block_on(w.register(Path::new("a.md"), Trigger::Link)).unwrap();
        let text = read(&dir, "index.md");
        let updated = crate::edit::set_in_text(
            &text,
            "contents.0",
            fig::Value::Str(link::id_target(&id)),
        )
        .unwrap();
        std::fs::write(dir.join("index.md"), &updated).unwrap();

        // The id target resolves in traversal and validation.
        let tree = block_on(w.tree("index.md")).unwrap();
        assert_eq!(tree.children[0].path, PathBuf::from("a.md"));
        assert_eq!(block_on(w.check("index.md")).unwrap(), vec![]);

        // Move the target. The parent's id entry must NOT be rewritten; the
        // registry follows instead.
        block_on(w.rename(Path::new("a.md"), Path::new("sub/a.md"))).unwrap();
        let index_text = read(&dir, "index.md");
        assert!(
            index_text.contains(&format!("colophon:{id}")),
            "id entry untouched: {index_text}"
        );
        assert_eq!(w.index().resolve(&id), Some(PathBuf::from("sub/a.md")));
        let tree = block_on(w.tree("index.md")).unwrap();
        assert_eq!(tree.children[0].path, PathBuf::from("sub/a.md"));
        assert_eq!(block_on(w.check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn delete_tombstones_and_check_diagnoses_the_dangler() {
        let dir = tempdir("id-delete");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n---\n");
        write(&dir, "a.md", "---\npart_of: index.md\n---\n");

        let mut w = id_ws(&dir);
        let id = block_on(w.register(Path::new("a.md"), Trigger::Link)).unwrap();
        let text = read(&dir, "index.md");
        let updated =
            crate::edit::set_in_text(&text, "contents.0", fig::Value::Str(link::id_target(&id)))
                .unwrap();
        std::fs::write(dir.join("index.md"), &updated).unwrap();

        block_on(w.delete(Path::new("a.md"), false)).unwrap();
        // Deleting removed the parent's entry too (matched through the registry
        // before the tombstone landed)… so re-add a dangling reference by hand
        // to simulate the out-of-band case.
        let text = read(&dir, "index.md");
        let updated =
            crate::edit::set_in_text(&text, "contents", fig::Value::Str(link::id_target(&id)))
                .unwrap();
        std::fs::write(dir.join("index.md"), &updated).unwrap();

        assert!(w.index().resolve(&id).is_none(), "tombstoned");
        assert!(w.index().is_known(&id), "but never forgotten");
        let findings = block_on(w.check("index.md")).unwrap();
        assert!(
            findings.iter().any(|f| matches!(
                f,
                crate::validate::Finding::DanglingId { tombstoned: true, .. }
            )),
            "{findings:?}"
        );
    }

    #[test]
    fn register_is_idempotent_and_policy_gated() {
        let dir = tempdir("id-register");
        write(&dir, "a.md", "---\ntitle: A\n---\n");

        let mut w = id_ws(&dir);
        let first = block_on(w.register(Path::new("a.md"), Trigger::Link)).unwrap();
        let again = block_on(w.register(Path::new("a.md"), Trigger::Link)).unwrap();
        assert_eq!(first, again, "idempotent");
        assert!(crate::identity::verify(first.as_str()));

        // Lazy policy: `Create` does not fire.
        write(&dir, "b.md", "---\ntitle: B\n---\n");
        let err = block_on(w.register(Path::new("b.md"), Trigger::Create)).unwrap_err();
        assert!(err.to_string().contains("does not register"), "{err}");
    }

    #[test]
    fn eager_create_assigns_an_id_from_birth() {
        let dir = tempdir("id-eager");
        write(&dir, "index.md", "---\ntitle: Root\n---\n");
        let mut w = Workspace::builder(StdFs)
            .root(&dir)
            .identity(Minter::eager(7))
            .index(FileIndex::new(fig::Format::Yaml))
            .build();
        block_on(w.create(Path::new("a.md"), Path::new("index.md"))).unwrap();
        let id = w.index().id_for_path(Path::new("a.md")).expect("registered at birth");
        assert!(crate::identity::verify(id.as_str()));
    }

    #[test]
    fn paths_only_workspace_is_untouched_by_the_identity_layer() {
        // The additive claim, negatively: the same mutations on a NoIdentity/
        // NoIndex workspace compile and run with the hooks monomorphized away.
        let dir = tempdir("no-id");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n---\n");
        write(&dir, "a.md", "---\npart_of: index.md\n---\n");
        let mut w = ws(&dir);
        block_on(w.rename(Path::new("a.md"), Path::new("b.md"))).unwrap();
        block_on(w.delete(Path::new("b.md"), false)).unwrap();
        assert_eq!(w.index().id_for_path(Path::new("b.md")), None);
    }
}
