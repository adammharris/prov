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
//! come from the workspace's [`crate::relation::RelationSet`].
//!
//! ## Writes are staged, not issued
//!
//! Every op here computes its edits and stages them into a
//! [`ChangeSet`](crate::change::ChangeSet), which lands as one unit — documents
//! and, when the op moved an ID, the registry with them. No error can leave the
//! workspace half-linked, and behind the write-ahead journal
//! ([`crate::journal`]) no crash can either: an interrupted op resolves to the
//! workspace fully before it or fully after it. Ops remain documents-only: no
//! directory moves.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use fig::Segment;

use crate::document::{Document, EmbedStyle, MetaCarrier, whole_file_format};
use crate::edit::MetaEditor;
use crate::error::{Error, Result};
use crate::fs::Storage;
use crate::identity::{IdentityPolicy, Trigger};
use crate::index::IndexStore;
use crate::link::{self, Link};
use crate::meta::Value;
use crate::validate::{Finding, LinkSite, Resolution};
use crate::workspace::{Target, Workspace};

/// Which axis a metadata reformat varies — the shared core of
/// [`Workspace::convert_meta_format`] and [`Workspace::convert_meta_embed`]. Both
/// re-emit a document's block in a new archetype resolved from the document's
/// *other* axis: `Format` keeps the embedding shape and swaps the frontmatter
/// language; `Embed` keeps the language and swaps the shape.
#[derive(Clone, Copy)]
enum ReformatAxis {
    /// Vary the frontmatter language (`metadata.format`), keep the embedding shape.
    Format(fig::Format),
    /// Vary the embedding shape (`metadata.embed`), keep the frontmatter language.
    Embed(EmbedStyle),
}

/// The files [`Workspace::create`] wrote. Under a combined parent this is just
/// the one document; under a **separated** parent it is the pair — the metadata
/// node the parent links, plus its sibling prose body file — so a caller (the
/// CLI) can report both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    /// The structural document: the node the parent's spanning entry points at,
    /// carrying the metadata (and, when separated, a `content` pointer). This is
    /// also the file any ID registers.
    pub node: PathBuf,
    /// The separated prose body file, present only when the new document is a
    /// separated pair (a whole-file node beside a plain body). `None` for a
    /// combined document, where the node *is* the whole file.
    pub body: Option<PathBuf>,
}

impl<FS: Storage, IdP: IdentityPolicy, Ix: IndexStore> Workspace<FS, IdP, Ix> {
    /// Create a new document at `path` (workspace-relative) as a spanning child
    /// of `parent`: the new file declares the inverse link back to `parent`, in
    /// `parent`'s embed archetype, and `parent`'s spanning field gains the child.
    /// If the identity policy registers on create, the new document is also
    /// assigned a stable ID.
    ///
    /// The child inherits the parent's *shape*. Under a combined parent it is a
    /// single combined file at `path`. Under a **separated** parent (a whole-file
    /// metadata node with a `content` pointer) it is a separated pair: `path`
    /// becomes the prose body and a sibling `path.<meta-ext>` the metadata node
    /// — the node is the structural document the parent links to and any ID
    /// registers. A `path` with a whole-file extension is always a bare metadata
    /// document, whatever the parent.
    ///
    /// Returns the [`Created`] files: always the structural node, plus the prose
    /// body file when the child is a separated pair.
    pub async fn create(&mut self, path: &Path, parent: &Path) -> Result<Created> {
        self.create_titled(path, parent, None).await
    }

    /// [`create`](Self::create) with an explicit `title` recorded in the new
    /// document's metadata, rather than one derived from its file stem. This is
    /// the title-primary authoring entry point (`prov new "My Great Note"`):
    /// the caller slugs the title into a readable filename ([`link::slug`]) and
    /// keeps the original title — casing, spaces, and punctuation — in the
    /// document, where structure and identity live (DESIGN §1). The parent's
    /// spanning-entry label follows the title too.
    pub async fn create_with_title(
        &mut self,
        path: &Path,
        parent: &Path,
        title: &str,
    ) -> Result<Created> {
        self.create_titled(path, parent, Some(title)).await
    }

    /// [`create`](Self::create) with an explicit title for the new document,
    /// used where the file stem is a poor title — a synthesized folder-note
    /// (`index.md`) that should read as its folder (`intake.rs`). `None` falls
    /// back to the stem, the plain-`create` behavior. Authoring the title here
    /// (rather than retitling after) keeps the parent's spanning-entry *label* in
    /// step with it, since that label is taken from the child's title.
    pub(crate) async fn create_titled(
        &mut self,
        path: &Path,
        parent: &Path,
        title_override: Option<&str>,
    ) -> Result<Created> {
        let path = link::normalize(path);
        let parent = link::normalize(parent);
        let (spanning, inverse) = self.spanning_pair()?;

        let (parent_text, parent_doc) = self.load(&parent).await?;

        // The child's shape follows the parent's. `node` is always the
        // *structural* document — the file registered, linked by the parent's
        // spanning entry, and carrying the inverse link; `body`, when present, is
        // a separated prose file written beside it. Three cases:
        //  - an explicit whole-file extension on `path` → a bare metadata
        //    document (config/registry-style node, no body);
        //  - a *separated* parent (a whole-file node pointing at prose via
        //    `content`) → a separated child: `path` is the body file and its
        //    sibling `path.<meta-ext>` the metadata node that points back at it;
        //  - otherwise → a combined document inheriting the parent's fenced block
        //    (or the workspace default when the parent is a bare config file).
        let (node, node_carrier, body): (PathBuf, MetaCarrier, Option<PathBuf>) =
            match whole_file_format(&path) {
                Some(format) => (path.clone(), MetaCarrier::WholeFile(format), None),
                None => match parent_doc.carrier {
                    Some(MetaCarrier::WholeFile(format)) if parent_doc.content_attr().is_some() => {
                        let node =
                            path.with_extension(crate::document::whole_file_extension(format));
                        (node, MetaCarrier::WholeFile(format), Some(path.clone()))
                    }
                    Some(MetaCarrier::Fenced(kind)) => {
                        (path.clone(), MetaCarrier::Fenced(kind), None)
                    }
                    _ => (
                        path.clone(),
                        crate::document::frontmatter_carrier(self.default_embed_format()),
                        None,
                    ),
                },
            };

        // Refuse if either file (the node, or a separated body) already exists.
        for existing in std::iter::once(&node).chain(body.iter()) {
            if self.fs().try_exists(&self.root().join(existing)).await? {
                return Err(Error::AlreadyExists(existing.to_path_buf()));
            }
        }

        // Titles for the authored links: the child's (an explicit override, else
        // from its stem) and the parent's (its own title, else derived from the
        // path).
        let title = title_override.map(str::to_owned).unwrap_or_else(|| {
            node.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let parent_title = parent_doc
            .meta
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| link::path_to_title(&parent));

        // Everything below can touch the index — authoring an id-form link
        // registers its target — so the change set (and with it the index
        // checkpoint that unwinds those registrations) opens here, before the
        // first of them, not down at the writes.
        let mut cs = self.change();

        // The child's inverse link back to the parent, authored in the `inverse`
        // relation's style (going "up"). The parent exists, so an id link
        // registers it by path.
        let up = self
            .authored_target(&inverse, &node, &parent, &parent_title, true)
            .await?;
        // The parent's spanning entry for the child, authored in the `spanning`
        // relation's style (going "down"). The node is not on disk yet, so
        // `target_exists = false` mints its id directly rather than register-by-path.
        let down = self
            .authored_target(&spanning, &parent, &node, &title, false)
            .await?;

        // Author the node's metadata: title, inverse link, and — for a separated
        // child — a `content` pointer at its body file. A separated node is
        // serialized from a mapping (a whole-file document, valid in any format
        // including empty JSON); a combined child grows its block via the editor.
        let new_text = match (&node_carrier, &body) {
            (MetaCarrier::WholeFile(format), Some(body_path)) => {
                let body_ref = body_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                let mut map = crate::meta::Mapping::new();
                map.insert("title".into(), Value::String(title));
                map.insert(inverse.clone(), Value::String(up));
                map.insert("content".into(), Value::String(body_ref));
                crate::meta::serialize_mapping(&map, *format)?
            }
            _ => {
                let mut new_doc = MetaEditor::open_or_init("", Some(node_carrier))?;
                new_doc.set_value(&[Segment::Key("title")], fig::Value::Str(title))?;
                new_doc.set_value(&[Segment::Key(&inverse)], fig::Value::Str(up))?;
                new_doc.render()?
            }
        };

        // The parent: append the child to its spanning field (creating it if
        // absent — `append` needs an existing sequence).
        let mut parent_editor = MetaEditor::open_or_init(&parent_text, parent_doc.carrier)?;
        let span_path = [Segment::Key(&spanning)];
        if parent_editor
            .append_value(&span_path, fig::Value::Str(down.clone()))
            .is_err()
        {
            parent_editor.set_value(&span_path, fig::Value::Seq(vec![fig::Value::Str(down)]))?;
        }
        let parent_out = parent_editor.render()?;

        // All edits computed; stage them.
        cs.write(&node, new_text);
        // A separated child's prose file starts empty (like a combined child's
        // body, which is just the synthesized block with nothing after it).
        if let Some(body_path) = &body {
            cs.write(body_path, Vec::new());
        }
        cs.write(&parent, parent_out);

        // Identity hook — eager policies assign an ID from birth (idempotent: an
        // id-linked child was already registered above). Staged before the
        // commit, so the registry lands with the documents rather than after.
        if self.identity().registration().fires_on(Trigger::Create)
            && self.index().id_for_path(&node).is_none()
        {
            let id = self.mint_unique(&node);
            self.index_mut().register(&id, &node);
        }
        self.commit(cs).await?;
        Ok(Created { node, body })
    }

    /// Adopt an existing document at `child` as a spanning child of `parent`,
    /// authoring **both** directions — the child's inverse link up and the
    /// parent's spanning entry down — without creating, moving, or rewriting the
    /// body of any file. The complement of [`create`](Self::create) for content
    /// that predates the workspace (`docs/init-adoption.md`, Phase 1).
    ///
    /// Additive and idempotent: whichever direction already exists is left as-is,
    /// so re-running (or adopting a partially-linked file) is a no-op. Both files
    /// must exist. Refuses when `child` already declares the inverse relation to a
    /// *different* parent — a contested containment a human must resolve, never
    /// overwritten (mirrors [`suggest_fix`](Self::suggest_fix) declining the same
    /// case). Registers `parent` when the workspace authors id links, exactly as
    /// `create` and the missing-inverse autofix do.
    pub async fn adopt(&mut self, child: &Path, parent: &Path) -> Result<()> {
        let child = link::normalize(child);
        let parent = link::normalize(parent);
        if child == parent {
            return Err(Error::Structure(format!(
                "{} cannot contain itself",
                parent.display()
            )));
        }
        let (spanning, inverse) = self.spanning_pair()?;

        for existing in [&child, &parent] {
            if !self.fs().try_exists(&self.root().join(existing)).await? {
                return Err(Error::NotFound(existing.to_path_buf()));
            }
        }

        let (child_text, child_doc) = self.load(&child).await?;
        let (parent_text, parent_doc) = self.load(&parent).await?;

        // Up: does the child already declare the inverse relation? If it points
        // here, that direction is done; if it points elsewhere, refuse rather than
        // clobber a deliberate parent claim.
        let already_up = match child_doc.meta.get(&inverse) {
            Some(existing) => {
                let points_here = existing.link_strings().iter().any(|t| {
                    self.resolve_link(&child, &Link::parse(t)) == Target::Path(parent.clone())
                });
                if !points_here {
                    return Err(Error::Structure(format!(
                        "{} already declares {inverse} to a different parent — resolve the \
                         contested containment by hand",
                        child.display()
                    )));
                }
                true
            }
            None => false,
        };
        // Down: does the parent's spanning field already resolve to the child?
        let already_down =
            self.relations().children(&parent_doc.meta).iter().any(|t| {
                self.resolve_link(&parent, &Link::parse(t)) == Target::Path(child.clone())
            });

        if already_up && already_down {
            return Ok(());
        }

        let child_title = child_doc
            .meta
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| link::path_to_title(&child));
        let parent_title = parent_doc
            .meta
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| link::path_to_title(&parent));

        let mut cs = self.change();
        // The child's inverse link back up. Comment-/format-preserving edit of the
        // existing document (its body is untouched), in the `inverse` relation's
        // reference style — the parent exists, so an id link registers it by path.
        if !already_up {
            let up = self
                .authored_target(&inverse, &child, &parent, &parent_title, true)
                .await?;
            let updated = crate::edit::set_in_text(
                &child_text,
                child_doc.carrier,
                &inverse,
                fig::Value::Str(up),
            )?;
            cs.write(&child, updated);
        }
        // The parent's spanning entry going down (the child exists on disk, so an
        // id link registers it by path). Append to the sequence, creating it if
        // the parent had no spanning field yet.
        if !already_down {
            let down = self
                .authored_target(&spanning, &parent, &child, &child_title, true)
                .await?;
            let mut parent_editor = MetaEditor::open_or_init(&parent_text, parent_doc.carrier)?;
            let span_path = [Segment::Key(&spanning)];
            if parent_editor
                .append_value(&span_path, fig::Value::Str(down.clone()))
                .is_err()
            {
                parent_editor
                    .set_value(&span_path, fig::Value::Seq(vec![fig::Value::Str(down)]))?;
            }
            cs.write(&parent, parent_editor.render()?);
        }
        self.commit(cs).await
    }

    /// Move the document at `child` to a different `parent` in the containment
    /// tree, leaving the file exactly where it is.
    ///
    /// The mirror image of [`rename`](Self::rename), and deliberately orthogonal
    /// to it: `rename` changes a document's **path** and preserves its place in
    /// the tree; `reparent` changes its **place in the tree** and preserves its
    /// path. Neither implies the other, because containment is link-shaped rather
    /// than directory-shaped (DESIGN §3) — a node may live in any directory, so
    /// relocating the file is a separate decision and a separate call.
    ///
    /// Distinct from [`adopt`](Self::adopt), which is *additive* and refuses a
    /// child that already claims a different parent. This is the verb for that
    /// refusal's other half: it *replaces* the claim, removing the old parent's
    /// spanning entry rather than leaving a document contained twice. An
    /// unparented child is accepted too, in which case there is nothing to remove
    /// and the effect is exactly `adopt`'s.
    ///
    /// ## Failure ordering
    ///
    /// Three documents change, and they land as one [`ChangeSet`]: an I/O
    /// failure at any of them unwinds the rest, so no error leaves the child
    /// contained twice or the old parent claiming a child that has moved on.
    ///
    /// The write *order* still matters, because a change set cannot rule out a
    /// crash (see [`crate::change`]). It is therefore chosen so that the windows
    /// a crash could expose are all findings `check` already reports: repointing
    /// the child first leaves the old parent claiming a child that does not claim
    /// it back ([`Finding::MissingInverse`]); adding the new entry before
    /// removing the old leaves the child contained twice
    /// ([`Finding::DuplicateContainment`]). Removing the old entry first would
    /// instead leave a child pointing up at a parent that has forgotten it — the
    /// one inconsistency in this set that `check` does *not* look for, so it is
    /// deliberately the last write rather than the first.
    pub async fn reparent(&mut self, child: &Path, parent: &Path) -> Result<()> {
        let child = link::normalize(child);
        let parent = link::normalize(parent);
        if child == parent {
            return Err(Error::Structure(format!(
                "{} cannot contain itself",
                parent.display()
            )));
        }
        let (spanning, inverse) = self.spanning_pair()?;
        for existing in [&child, &parent] {
            if !self.fs().try_exists(&self.root().join(existing)).await? {
                return Err(Error::NotFound(existing.to_path_buf()));
            }
        }

        // Refuse a cycle: walking up from the *new* parent must not arrive at the
        // child. Reparenting a node beneath its own descendant would sever the
        // pair from the tree entirely — both would still claim each other, so
        // nothing would look broken from inside the loop, and a spanning walk from
        // the root would simply never reach them again.
        let mut rung = parent.clone();
        let mut seen = BTreeSet::new();
        while seen.insert(rung.clone()) {
            if rung == child {
                return Err(Error::Structure(format!(
                    "cannot reparent {} into {} — {} is contained by it, so the move would \
                     detach both from the tree",
                    child.display(),
                    parent.display(),
                    parent.display(),
                )));
            }
            let Ok((_, doc)) = self.load(&rung).await else {
                break;
            };
            match self.single_target(&doc, &inverse, &rung) {
                Some(up) => rung = up,
                None => break,
            }
        }

        let (child_text, child_doc) = self.load(&child).await?;
        let old_parent = self.single_target(&child_doc, &inverse, &child);
        if old_parent.as_ref() == Some(&parent) {
            // Already there. Idempotent like `adopt`, and for the same reason: a
            // caller re-running a script should not have to ask first.
            return Ok(());
        }

        let child_title = child_doc
            .meta
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| link::path_to_title(&child));
        let (parent_text, parent_doc) = self.load(&parent).await?;
        let parent_title = parent_doc
            .meta
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| link::path_to_title(&parent));

        let mut cs = self.change();

        // 1. The child's inverse, repointed up at the new parent.
        let up = self
            .authored_target(&inverse, &child, &parent, &parent_title, true)
            .await?;
        let updated = crate::edit::set_in_text(
            &child_text,
            child_doc.carrier,
            &inverse,
            fig::Value::Str(up),
        )?;
        cs.write(&child, updated);

        // 2. The new parent's spanning entry, appended (created if it had none).
        let already_down =
            self.relations().children(&parent_doc.meta).iter().any(|t| {
                self.resolve_link(&parent, &Link::parse(t)) == Target::Path(child.clone())
            });
        if !already_down {
            let down = self
                .authored_target(&spanning, &parent, &child, &child_title, true)
                .await?;
            let mut editor = MetaEditor::open_or_init(&parent_text, parent_doc.carrier)?;
            let span_path = [Segment::Key(&spanning)];
            if editor
                .append_value(&span_path, fig::Value::Str(down.clone()))
                .is_err()
            {
                editor.set_value(&span_path, fig::Value::Seq(vec![fig::Value::Str(down)]))?;
            }
            cs.write(&parent, editor.render()?);
        }

        // 3. The old parent's entry, removed last (see the ordering note above).
        // Read through the change set: when the old parent is a document some
        // earlier step already staged, that staged text is what must be edited,
        // not the stale copy on disk.
        if let Some(old) = &old_parent
            && old != &parent
        {
            let (old_text, old_doc) = self.load_staged(&cs, old).await?;
            if let (Some(index), Some(carrier)) = (
                self.entry_index(&old_doc, &spanning, old, &child),
                old_doc.carrier,
            ) {
                let mut editor = MetaEditor::open(&old_text, carrier)?;
                editor.remove_item(&[Segment::Key(&spanning)], index)?;
                cs.write(old, editor.render()?);
            }
        }
        self.commit(cs).await
    }

    /// Move/rename the document at `from` to `to`, maintaining every affected
    /// link across the workspace. Every inbound reference that resolves to
    /// `from` by a path — the parent's spanning entry, each child's inverse,
    /// overlay `links`, and body `[[…]]` wikilinks, wherever they live — is
    /// retargeted to `to`; and, when the directory changes, every relative link
    /// the moved document itself declares (frontmatter and body alike) is
    /// recomputed. Labels on `[label](path)` links and `[[target|label]]`
    /// wikilinks are preserved. `colophon:<id>` references are left untouched:
    /// where a registry is present its `id → path` update keeps them resolving
    /// (the point of an ID link), and in a path-only (Diaryx-style) workspace
    /// they never appear.
    ///
    /// Inbound references are found by a [`census`](Workspace::census) over the
    /// spanning tree, whose root is discovered by walking `part_of` up from
    /// `from` — so the caller supplies no root. References living only in
    /// documents *unreachable* from that root are not seen (a malformed tree,
    /// which `check` reports separately).
    pub async fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        let from = link::normalize(from);
        let to = link::normalize(to);

        if !self.fs().try_exists(&self.root().join(&from)).await? {
            return Err(Error::NotFound(from.to_path_buf()));
        }
        if self.fs().try_exists(&self.root().join(&to)).await? {
            return Err(Error::AlreadyExists(to.to_path_buf()));
        }
        let (from_text, from_doc) = self.load(&from).await?;
        let mut cs = self.change();

        // 1. Inbound references: every document that links *to* `from` by a
        //    path, retargeted to `to` (parent's spanning entry, children's
        //    inverses, overlay `links`, body wikilinks). Id-form links resolve
        //    through the registry and are never rewritten.
        let inbound_writes = self.collect_inbound_rewrites(&from, &to).await?;

        // A separated document's prose lives in a sibling body file; move it
        // alongside (and keep the `content` pointer correct) so the pair travels
        // together.
        let body_move = self.plan_body_move(&from_doc, &from, &to).await?;

        // The body's destination needs the same refusal as the node's. A rename
        // overwrites, and an overwrite is the one thing staging cannot make good:
        // every other op records its undo before acting, but a clobbered file's
        // bytes are gone by the time anything could have copied them. So this is
        // a guard, not something rollback covers — and it is easy to walk into,
        // since the body's name is *derived* (`notes.yaml` → `notes.md`) and so
        // never passed by the caller, who therefore never sees the collision.
        if let Some(mv) = &body_move
            && self.fs().try_exists(&self.root().join(&mv.to)).await?
        {
            return Err(Error::Structure(format!(
                "{}'s content file would move to {}, which already exists",
                to.display(),
                mv.to.display()
            )));
        }

        // 2. The document itself: when its directory changes, every relative
        //    link it declares must be recomputed to keep resolving — first the
        //    frontmatter links, then the body wikilinks (whose spans MetaEditor
        //    leaves verbatim, so they can be spliced afterwards).
        let mut self_text = if from.parent() != to.parent() {
            let meta_rewritten = rerelativize(
                &from_text,
                &from_doc,
                self.relations().relations(),
                &from,
                &to,
            )?;
            rerelativize_body_links(&meta_rewritten, &from_doc.body, &from, &to)
        } else {
            from_text
        };
        // For a separated node, repoint its `content` to the (moved) body file.
        if let Some(mv) = &body_move
            && let Some(carrier) = from_doc.carrier
        {
            let mut editor = MetaEditor::open(&self_text, carrier)?;
            editor.replace_value(
                &[Segment::Key("content")],
                fig::Value::Str(mv.new_ref.clone()),
            )?;
            self_text = editor.render()?;
        }

        // All edits computed; stage them.
        cs.rename(&from, &to);
        cs.write(&to, self_text);
        if let Some(mv) = &body_move {
            cs.rename(&mv.from, &mv.to);
            // A prose body is rewritten with its re-relativized text; an opaque
            // payload (`text` is `None`) is left exactly as the rename moved it.
            if let Some(text) = &mv.text {
                cs.write(&mv.to, text.clone());
            }
        }
        for (source, text) in inbound_writes {
            cs.write(source, text);
        }

        // Identity hook — the registry follows the move, so every
        // `colophon:<id>` reference to this document survives untouched. Staged
        // with the documents: a move whose links are maintained but whose
        // registry is not is the one tear IDs exist to prevent.
        if let Some(id) = self.index().id_for_path(&from) {
            self.index_mut().set_path(&id, &to);
        }
        self.commit(cs).await
    }

    /// Delete the document at `path`, removing the parent's spanning entry for
    /// it. Refuses when the document has spanning children (they would be
    /// orphaned) unless `force` is set. A registered ID is retired — with a
    /// tombstoning store it is never reissued, so dangling references stay
    /// diagnosable.
    ///
    /// Returns the inbound references *left* dangling by the delete: every
    /// other document's overlay link or body wikilink that resolved to `path`
    /// (as [`Finding::BrokenLink`]), plus any `colophon:<id>` reference now
    /// pointing at the tombstone (as [`Finding::DanglingId`]). The parent's
    /// spanning entry is *not* reported — it is removed here — and a delete that
    /// nothing pointed at returns an empty list. Unlike `rename`, these are not
    /// rewritten: a link records intent, and there is no new target to send it
    /// to; the caller decides what to do with the diagnosis.
    pub async fn delete(&mut self, path: &Path, force: bool) -> Result<Vec<Finding>> {
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

        let parent = self.single_target(&doc, &inverse, &path);

        // Diagnose inbound references that will dangle: census the tree and keep
        // every link resolving to `path`, except the parent's spanning entry
        // (removed below) and any self-reference in the doomed document itself.
        let root = self.spanning_root(&path, &inverse).await?;
        let danglers: Vec<Finding> = self
            .census(&root)
            .await?
            .into_iter()
            .filter(|e| e.resolution.resolved_path() == Some(&path))
            .filter(|e| {
                e.source != path
                    && !(Some(&e.source) == parent.as_ref()
                        && matches!(&e.site, LinkSite::Relation(r) if *r == spanning))
            })
            .map(|e| match e.resolution {
                Resolution::Id { id, .. } => Finding::DanglingId {
                    doc: e.source,
                    site: e.site,
                    id,
                    tombstoned: true,
                },
                _ => Finding::BrokenLink {
                    doc: e.source,
                    site: e.site,
                    target: e.target_text,
                },
            })
            .collect();

        let mut parent_write: Option<(PathBuf, String)> = None;
        if let Some(parent) = &parent {
            let (parent_text, parent_doc) = self.load(parent).await?;
            if let (Some(index), Some(carrier)) = (
                self.entry_index(&parent_doc, &spanning, parent, &path),
                parent_doc.carrier,
            ) {
                let mut editor = MetaEditor::open(&parent_text, carrier)?;
                editor.remove_item(&[Segment::Key(&spanning)], index)?;
                parent_write = Some((parent.clone(), editor.render()?));
            }
        }

        // A separated node's body lives in a sibling file; delete the pair.
        let body_file = content_target(&doc, &path);
        let body_exists = match &body_file {
            Some(body) => self.fs().try_exists(&self.root().join(body)).await?,
            None => false,
        };

        let mut cs = self.change();
        cs.remove(&path);
        if let (Some(body), true) = (&body_file, body_exists) {
            cs.remove(body);
        }
        if let Some((parent, text)) = parent_write {
            cs.write(parent, text);
        }

        // Identity hook — retire the ID (a tombstoning store keeps it known
        // forever, so it is never minted again to mean something else).
        if let Some(id) = self.index().id_for_path(&path) {
            self.index_mut().unregister(&id);
        }
        self.commit(cs).await?;
        Ok(danglers)
    }

    /// Delete the document at `path` by moving it into the workspace **recycle
    /// bin** instead of destroying it — the recoverable counterpart of
    /// [`delete`](Self::delete), and the safe default for archival use.
    ///
    /// It shares `delete`'s structure — the parent's spanning entry is removed,
    /// a document with spanning children is refused unless `force`d, and the same
    /// dangling-inbound-reference diagnosis is returned — but rather than
    /// [`remove`](crate::ChangeSet::remove) the file it is **moved** into the bin
    /// and recorded there, so [`restore`](Self::restore) can bring it back.
    ///
    /// The bin is a first-class, reachable member: its index document (which the
    /// root links through the recycle relation, and which `check` validates like
    /// any other) records, per deletion, where the document came from and where
    /// its bytes now live. The whole operation — the file move, the parent edit,
    /// the bin-index update, and (the first time) the root's pointer to the bin —
    /// lands as one journaled [`ChangeSet`], so a bin-delete is exactly as
    /// crash-atomic as everything else.
    ///
    /// The deleted bytes are parked under `recyclebin/items/`, mirroring their
    /// original path. That subdirectory is deliberately *unreached* — nothing
    /// links into it — so the reachability-bounded orphan check (DESIGN §8) never
    /// mistakes a binned document for a stray one.
    ///
    /// `at` is an optional caller-supplied deletion timestamp recorded on the
    /// tombstone (the CLI passes the current time). The library takes it as an
    /// argument rather than reading a clock so the op stays deterministic.
    pub async fn recycle(
        &mut self,
        path: &Path,
        force: bool,
        at: Option<&str>,
    ) -> Result<Vec<Finding>> {
        let path = link::normalize(path);
        let (spanning, inverse) = self.spanning_pair()?;
        let (_, doc) = self.load(&path).await?;

        // Children guard — identical to `delete`'s: a document that contains
        // others would orphan them, so it is refused unless forced.
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

        let parent = self.single_target(&doc, &inverse, &path);
        let root = self.spanning_root(&path, &inverse).await?;

        // Inbound references the move leaves dangling — the same diagnosis
        // `delete` returns, since a binned document is out of the live graph.
        let danglers: Vec<Finding> = self
            .census(&root)
            .await?
            .into_iter()
            .filter(|e| e.resolution.resolved_path() == Some(&path))
            .filter(|e| {
                e.source != path
                    && !(Some(&e.source) == parent.as_ref()
                        && matches!(&e.site, LinkSite::Relation(r) if *r == spanning))
            })
            .map(|e| match e.resolution {
                Resolution::Id { id, .. } => Finding::DanglingId {
                    doc: e.source,
                    site: e.site,
                    id,
                    tombstoned: true,
                },
                _ => Finding::BrokenLink {
                    doc: e.source,
                    site: e.site,
                    target: e.target_text,
                },
            })
            .collect();

        // Locate the bin, or plan to bootstrap it on this first deletion.
        let format = self.default_embed_format();
        let ext = crate::document::whole_file_extension(format);
        let existing_index = self.recycle_bin_path(&root).await?;
        let bin_index = existing_index
            .clone()
            .unwrap_or_else(|| PathBuf::from("recyclebin").join(format!("index.{ext}")));
        let bin_dir = bin_index
            .parent()
            .unwrap_or(Path::new("recyclebin"))
            .to_path_buf();
        let items_dir = bin_dir.join("items");

        // The bin index's current records (and its own title, so a wholesale
        // re-render preserves it). The bin is machinery, reached one-way through
        // the root's `recycle_bin` pointer, so it carries no `part_of` back-link
        // (DESIGN §5, "link target kinds"). Absent bin → empty, with a default title.
        let (mut records, bin_title) = match &existing_index {
            Some(index) => {
                let (_, bin_doc) = self.load(index).await?;
                // The bin index is a record store — reject a markdown carrier
                // (DESIGN §5, whole-file rule).
                if let Some(carrier) = bin_doc.carrier {
                    crate::document::require_whole_file(index, carrier)?;
                }
                let recs = bin_doc
                    .meta
                    .get("deleted")
                    .and_then(Value::as_sequence)
                    .map(<[Value]>::to_vec)
                    .unwrap_or_default();
                let title = bin_doc
                    .meta
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Recycle Bin")
                    .to_string();
                (recs, title)
            }
            None => (Vec::new(), "Recycle Bin".to_string()),
        };

        // Where the bytes go: mirror the original path under the (unreached)
        // items directory, with a numeric suffix on the rare same-path collision.
        let mut node_bin = items_dir.join(&path);
        let mut bump = 1;
        while self.fs().try_exists(&self.root().join(&node_bin)).await? {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            node_bin = items_dir
                .join(path.parent().unwrap_or(Path::new("")))
                .join(format!("{name}.{bump}"));
            bump += 1;
        }

        // A separated document's prose body travels with it.
        let body_from = content_target(&doc, &path);
        let body_bin = match &body_from {
            Some(body) if self.fs().try_exists(&self.root().join(body)).await? => {
                Some((body.clone(), items_dir.join(body)))
            }
            _ => None,
        };

        // The tombstone record — everything `restore` needs to undo this.
        let title = doc
            .meta
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| link::path_to_title(&path));
        let id_opt = self.index().id_for_path(&path);
        let mut record = crate::meta::Mapping::new();
        record.insert("title".into(), Value::String(title));
        if let Some(id) = &id_opt {
            record.insert("id".into(), Value::String(id.to_string()));
        }
        record.insert(
            "from".into(),
            Value::String(path.to_string_lossy().into_owned()),
        );
        record.insert(
            "bin".into(),
            Value::String(node_bin.to_string_lossy().into_owned()),
        );
        if let Some(parent) = &parent {
            record.insert(
                "parent".into(),
                Value::String(parent.to_string_lossy().into_owned()),
            );
        }
        if let Some((from, to)) = &body_bin {
            record.insert(
                "body_from".into(),
                Value::String(from.to_string_lossy().into_owned()),
            );
            record.insert(
                "body_bin".into(),
                Value::String(to.to_string_lossy().into_owned()),
            );
        }
        if let Some(at) = at {
            record.insert("at".into(), Value::String(at.to_string()));
        }
        records.push(Value::Mapping(record));

        let mut bin_map = crate::meta::Mapping::new();
        bin_map.insert("title".into(), Value::String(bin_title));
        bin_map.insert("deleted".into(), Value::Sequence(records));
        let bin_text = crate::meta::serialize_mapping(&bin_map, format)?;

        // The parent's spanning entry for the doomed document, removed.
        let mut parent_write: Option<(PathBuf, String)> = None;
        if let Some(parent) = &parent {
            let (parent_text, parent_doc) = self.load(parent).await?;
            if let (Some(index), Some(carrier)) = (
                self.entry_index(&parent_doc, &spanning, parent, &path),
                parent_doc.carrier,
            ) {
                let mut editor = MetaEditor::open(&parent_text, carrier)?;
                editor.remove_item(&[Segment::Key(&spanning)], index)?;
                parent_write = Some((parent.clone(), editor.render()?));
            }
        }

        let mut cs = self.change();
        cs.rename(&path, &node_bin);
        if let Some((from, to)) = &body_bin {
            cs.rename(from, to);
        }
        cs.write(&bin_index, bin_text);

        // The root's pointer to the bin, authored the first time only — merged
        // with the parent edit when the parent *is* the root, so the one document
        // is written once with both changes rather than twice.
        let mut root_text: Option<String> = None;
        if let Some((parent_path, text)) = &parent_write {
            if *parent_path == root {
                root_text = Some(text.clone());
            } else {
                cs.write(parent_path.clone(), text.clone());
            }
        }
        if existing_index.is_none() {
            let base = match &root_text {
                Some(text) => text.clone(),
                None => self.load(&root).await?.0,
            };
            let root_doc = Document::parse(&root, &base)?;
            let relation = self
                .relations()
                .recycle_relation()
                .ok_or_else(|| Error::Structure("no recycle relation configured".into()))?
                .to_string();
            let root_dir = root.parent().unwrap_or(Path::new(""));
            let pointer = link::relative(root_dir, &bin_index);
            root_text = Some(crate::edit::set_in_text(
                &base,
                root_doc.carrier,
                &relation,
                crate::edit::infer_scalar(&pointer),
            )?);
        }
        if let Some(text) = root_text {
            cs.write(root.clone(), text);
        }

        // Identity hook — retire the ID to a tombstone exactly as `delete` does;
        // the record keeps its value so `restore` can re-register it.
        if let Some(id) = &id_opt {
            self.index_mut().unregister(id);
        }
        self.commit(cs).await?;
        Ok(danglers)
    }

    /// Bring a document back from the recycle bin to the path it was deleted
    /// from — the inverse of [`recycle`](Self::recycle).
    ///
    /// The tombstone record carries everything needed: the bin location to move
    /// the bytes back from, the parent to re-link under (only the parent → child
    /// direction was lost; the child's own inverse link travelled with it, so it
    /// is correct again the moment the file is home), and the ID to re-register.
    /// It all lands as one journaled [`ChangeSet`]. Refuses when something already
    /// occupies the restore path, or when `from` is not in the bin.
    ///
    /// `root_doc` names the workspace root, from which the bin is discovered.
    pub async fn restore(&mut self, from: &Path, root_doc: &Path) -> Result<()> {
        let from = link::normalize(from);
        let (spanning, _) = self.spanning_pair()?;
        let bin_index = self
            .recycle_bin_path(root_doc)
            .await?
            .ok_or_else(|| Error::Structure("workspace has no recycle bin".into()))?;
        let (_, bin_doc) = self.load(&bin_index).await?;
        // The bin index is a record store — reject a markdown carrier
        // (DESIGN §5, whole-file rule).
        if let Some(carrier) = bin_doc.carrier {
            crate::document::require_whole_file(&bin_index, carrier)?;
        }
        let records: Vec<Value> = bin_doc
            .meta
            .get("deleted")
            .and_then(Value::as_sequence)
            .map(<[Value]>::to_vec)
            .unwrap_or_default();

        let from_str = from.to_string_lossy();
        let pos = records
            .iter()
            .position(|r| r.get("from").and_then(Value::as_str) == Some(from_str.as_ref()))
            .ok_or_else(|| {
                Error::Structure(format!("{} is not in the recycle bin", from.display()))
            })?;
        let record = records[pos].clone();
        let str_field = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_owned);
        let node_bin = PathBuf::from(
            str_field("bin")
                .ok_or_else(|| Error::Structure("recycle record has no `bin` path".into()))?,
        );
        let parent = str_field("parent").map(PathBuf::from);
        let id = str_field("id").map(crate::identity::Id);
        let title = str_field("title").unwrap_or_else(|| link::path_to_title(&from));
        let body = match (str_field("body_from"), str_field("body_bin")) {
            (Some(from), Some(bin)) => Some((PathBuf::from(from), PathBuf::from(bin))),
            _ => None,
        };

        if self.fs().try_exists(&self.root().join(&from)).await? {
            return Err(Error::Structure(format!(
                "{} already exists; cannot restore over it",
                from.display()
            )));
        }

        // The bin index without this record, re-rendered whole (a machine file).
        let mut remaining = records;
        remaining.remove(pos);
        let bin_dir = bin_index
            .parent()
            .unwrap_or(Path::new("recyclebin"))
            .to_path_buf();
        let format = self.default_embed_format();
        let mut bin_map = crate::meta::Mapping::new();
        bin_map.insert(
            "title".into(),
            Value::String(
                bin_doc
                    .meta
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Recycle Bin")
                    .to_string(),
            ),
        );
        bin_map.insert(
            "part_of".into(),
            Value::String(
                bin_doc
                    .meta
                    .get("part_of")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| link::relative(&bin_dir, root_doc)),
            ),
        );
        bin_map.insert("deleted".into(), Value::Sequence(remaining));
        let bin_text = crate::meta::serialize_mapping(&bin_map, format)?;

        let mut cs = self.change();
        // Re-register the ID *after* `change`'s checkpoint, so authoring the
        // parent link below reuses the document's own id rather than minting a new
        // one, and so a failure rolls the re-registration back with everything else.
        if let Some(id) = &id {
            self.index_mut().register(id, &from);
        }
        cs.rename(&node_bin, &from);
        if let Some((body_from, body_bin)) = &body {
            cs.rename(body_bin, body_from);
        }
        cs.write(&bin_index, bin_text);

        // Re-add the parent's spanning entry (its removal is all `recycle` did to
        // the parent). Skip when the parent is gone or already links the child.
        if let Some(parent) = &parent
            && self.fs().try_exists(&self.root().join(parent)).await?
        {
            let (parent_text, parent_doc) = self.load(parent).await?;
            let already =
                self.relations().children(&parent_doc.meta).iter().any(|t| {
                    self.resolve_link(parent, &Link::parse(t)) == Target::Path(from.clone())
                });
            if !already {
                let down = self
                    .authored_target(&spanning, parent, &from, &title, false)
                    .await?;
                let mut editor = MetaEditor::open_or_init(&parent_text, parent_doc.carrier)?;
                let span_path = [Segment::Key(&spanning)];
                if editor
                    .append_value(&span_path, fig::Value::Str(down.clone()))
                    .is_err()
                {
                    editor.set_value(&span_path, fig::Value::Seq(vec![fig::Value::Str(down)]))?;
                }
                cs.write(parent.clone(), editor.render()?);
            }
        }
        self.commit(cs).await
    }

    /// Permanently purge every document in the recycle bin — the only hard
    /// delete, and always explicit. Returns how many records were purged.
    ///
    /// The bin's bytes are removed and its index emptied (the index member itself
    /// stays, still linked from the root), as one journaled [`ChangeSet`]. ID
    /// tombstones are untouched: an ID retired at deletion stays retired, so a
    /// `colophon:<id>` reference to a purged document remains diagnosable rather
    /// than silently reissuable.
    pub async fn empty_bin(&mut self, root_doc: &Path) -> Result<usize> {
        let bin_index = self
            .recycle_bin_path(root_doc)
            .await?
            .ok_or_else(|| Error::Structure("workspace has no recycle bin".into()))?;
        let (_, bin_doc) = self.load(&bin_index).await?;
        // The bin index is a record store — reject a markdown carrier
        // (DESIGN §5, whole-file rule).
        if let Some(carrier) = bin_doc.carrier {
            crate::document::require_whole_file(&bin_index, carrier)?;
        }
        let records: Vec<Value> = bin_doc
            .meta
            .get("deleted")
            .and_then(Value::as_sequence)
            .map(<[Value]>::to_vec)
            .unwrap_or_default();
        let count = records.len();

        let bin_dir = bin_index
            .parent()
            .unwrap_or(Path::new("recyclebin"))
            .to_path_buf();
        let format = self.default_embed_format();
        let mut bin_map = crate::meta::Mapping::new();
        bin_map.insert(
            "title".into(),
            Value::String(
                bin_doc
                    .meta
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Recycle Bin")
                    .to_string(),
            ),
        );
        bin_map.insert(
            "part_of".into(),
            Value::String(
                bin_doc
                    .meta
                    .get("part_of")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| link::relative(&bin_dir, root_doc)),
            ),
        );
        bin_map.insert("deleted".into(), Value::Sequence(Vec::new()));
        let bin_text = crate::meta::serialize_mapping(&bin_map, format)?;

        let mut cs = self.change();
        for record in &records {
            for key in ["bin", "body_bin"] {
                if let Some(path) = record.get(key).and_then(Value::as_str) {
                    let rel = PathBuf::from(path);
                    if self.fs().try_exists(&self.root().join(&rel)).await? {
                        cs.remove(rel);
                    }
                }
            }
        }
        cs.write(&bin_index, bin_text);
        self.commit(cs).await?;
        Ok(count)
    }

    /// Split the combined document at `path` into two linked plain-text files: a
    /// whole-file **metadata** document (in the document's own frontmatter
    /// format) that becomes the structural node, and a **body** file holding its
    /// prose, joined by a `content` attribute on the metadata file. Every inbound
    /// link to the document is retargeted to the new metadata file, and a
    /// registered ID follows it. Returns the metadata file's path. The inverse of
    /// [`combine`](Workspace::combine).
    pub async fn separate(&mut self, path: &Path) -> Result<PathBuf> {
        let path = link::normalize(path);
        if !self.fs().try_exists(&self.root().join(&path)).await? {
            return Err(Error::NotFound(path.to_path_buf()));
        }
        let (_, doc) = self.load(&path).await?;
        let Some(MetaCarrier::Fenced(kind)) = doc.carrier else {
            return Err(Error::Structure(format!(
                "{} is not a combined document (nothing to separate)",
                path.display()
            )));
        };
        if doc.content_attr().is_some() {
            return Err(Error::Structure(format!(
                "{} is already separated",
                path.display()
            )));
        }
        let Some(mapping) = doc.meta.as_mapping() else {
            return Err(Error::Structure(format!(
                "{} has no metadata to separate",
                path.display()
            )));
        };
        let format = kind.inner_format();
        let meta_path = path.with_extension(crate::document::whole_file_extension(format));
        if meta_path == path {
            return Err(Error::Structure(format!(
                "{} already has a metadata-file extension",
                path.display()
            )));
        }
        if self.fs().try_exists(&self.root().join(&meta_path)).await? {
            return Err(Error::AlreadyExists(meta_path.to_path_buf()));
        }
        let body_ref = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| Error::Structure(format!("{} has no filename", path.display())))?
            .to_string();

        // The metadata file = the document's mapping + a `content` pointer at the
        // body file (a sibling, so just its name).
        let mut map = mapping.clone();
        map.insert("content".into(), Value::String(body_ref));
        let meta_text = crate::meta::serialize_mapping(&map, format)?;
        let body_text = doc.body.clone();

        let mut cs = self.change();
        // Inbound links now point at the metadata file (the structural node).
        let inbound = self.collect_inbound_rewrites(&path, &meta_path).await?;

        cs.write(&meta_path, meta_text);
        cs.write(&path, body_text);
        for (source, text) in inbound {
            cs.write(source, text);
        }
        if let Some(id) = self.index().id_for_path(&path) {
            self.index_mut().set_path(&id, &meta_path);
        }
        self.commit(cs).await?;
        Ok(meta_path)
    }

    /// Fold the separated document whose metadata file is `path` back into one
    /// combined file: the body file regains its metadata as frontmatter (in the
    /// metadata file's format), the metadata file is removed, and inbound links
    /// are retargeted to the combined file. Returns the combined file's path. The
    /// inverse of [`separate`](Workspace::separate).
    pub async fn combine(&mut self, path: &Path) -> Result<PathBuf> {
        let path = link::normalize(path);
        let (_, doc) = self.load(&path).await?;
        let Some(content) = content_target(&doc, &path) else {
            return Err(Error::Structure(format!(
                "{} is not a separated document (no `content` attribute)",
                path.display()
            )));
        };
        let Some(MetaCarrier::WholeFile(format)) = doc.carrier else {
            return Err(Error::Structure(format!(
                "{} is not a whole-file metadata document",
                path.display()
            )));
        };
        let Some(mapping) = doc.meta.as_mapping() else {
            return Err(Error::Structure(format!(
                "{} has no metadata",
                path.display()
            )));
        };
        if !self.fs().try_exists(&self.root().join(&content)).await? {
            return Err(Error::Structure(format!(
                "{}'s content file {} is missing",
                path.display(),
                content.display()
            )));
        }
        let (body_raw, body_doc) = self.load(&content).await?;
        // Normally the body file is pure prose; tolerate a stray frontmatter.
        let body = match body_doc.carrier {
            Some(_) => body_doc.body,
            None => body_raw,
        };

        // Rebuild the combined document: a fresh frontmatter block (the metadata
        // format) carrying every key except `content`, then the body.
        let carrier = crate::document::frontmatter_carrier(format);
        let mut editor = MetaEditor::open_or_init(&body, Some(carrier))?;
        for (key, value) in mapping {
            if key.as_str() == "content" {
                continue;
            }
            editor.set_value(&[Segment::Key(key)], fig::Value::from(value))?;
        }
        let combined = editor.render()?;

        let mut cs = self.change();
        // Inbound links point back at the (now combined) content file.
        let inbound = self.collect_inbound_rewrites(&path, &content).await?;

        cs.write(&content, combined);
        cs.remove(&path);
        for (source, text) in inbound {
            cs.write(source, text);
        }
        if let Some(id) = self.index().id_for_path(&path) {
            self.index_mut().set_path(&id, &content);
        }
        self.commit(cs).await?;
        Ok(content)
    }

    /// Duplicate the document at `source` as a fresh sibling under the same
    /// parent, returning the new file's path. The copy carries `source`'s title,
    /// body, and metadata verbatim, with three deliberate exceptions:
    ///
    /// - **No cloned identity.** A stamped frontmatter `id` (frontmatter storage,
    ///   DESIGN §5) is dropped — an ID names *one* document, never two — so the
    ///   copy is unregistered until something links or publishes it. When the
    ///   workspace authors id links, attaching the copy below mints it a *new* ID.
    /// - **No cloned children.** The spanning field is dropped, so the copy is a
    ///   childless duplicate of just this node. Deep-copying the subtree would
    ///   leave every child claimed by two parents (its inverse still names
    ///   `source`); prov's link-shaped containment — unlike diaryx's
    ///   directory-shaped copy, which recurses the folder — makes the shallow
    ///   copy the only unambiguous one.
    /// - **A unique name.** `foo.md` → `foo-copy.md`, then `foo-copy-2.md`, ….
    ///
    /// The copy inherits `source`'s parent: its inverse link (copied verbatim, and
    /// still valid — the copy sits in the same directory) already points there,
    /// and the parent's spanning field gains the copy — the same bidirectional
    /// link `create`/`adopt` author. A `source` with no parent (the spanning root,
    /// or an orphan) is copied without attaching. A **separated** node (a
    /// whole-file node with a `content` pointer) duplicates its body file too, and
    /// the copy points at *its own* body.
    pub async fn duplicate(&mut self, source: &Path) -> Result<PathBuf> {
        let source = link::normalize(source);
        if !self.fs().try_exists(&self.root().join(&source)).await? {
            return Err(Error::NotFound(source.to_path_buf()));
        }
        let (source_text, doc) = self.load(&source).await?;
        let (spanning, inverse) = self.spanning_pair()?;

        // A separated node carries its prose/payload in a sibling file; the copy
        // needs its own. Resolve the source body up front so the unique-name
        // search can keep the node *and* its body collision-free together.
        let body_from = content_target(&doc, &source);
        let (dest, body_dest) = self.unique_copy_path(&source, body_from.as_deref()).await?;

        // The copy's text: `source`'s metadata and body verbatim, minus the cloned
        // `id` (identity is per-document) and the spanning field (no cloned
        // children), and — for a separated node — repointed `content` at its own
        // new body file. A carrier-less file (pure prose, not a linked node) is
        // copied byte-for-byte.
        let copy_text = if let Some(carrier) = doc.carrier {
            let mut editor = MetaEditor::open(&source_text, carrier)?;
            let _ = editor.delete(&[Segment::Key("id")]);
            let _ = editor.delete(&[Segment::Key(&spanning)]);
            if let Some((_, new_ref)) = &body_dest {
                editor
                    .replace_value(&[Segment::Key("content")], fig::Value::Str(new_ref.clone()))?;
            }
            editor.render()?
        } else {
            source_text.clone()
        };

        // The parent gains a spanning entry for the copy (going "down"). The copy
        // is not yet on disk, so `authored_target` mints its id directly rather
        // than register-by-path — exactly as `create`'s down-link does. The copy's
        // own inverse link "up" is already present (copied verbatim) and resolves
        // unchanged, so it is not re-authored. A parentless source just skips this.
        let parent = self.single_target(&doc, &inverse, &source);
        let mut cs = self.change();
        let parent_write = if let Some(parent) = &parent {
            let (parent_text, parent_doc) = self.load(parent).await?;
            let copy_title = doc
                .meta
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| link::path_to_title(&dest));
            let down = self
                .authored_target(&spanning, parent, &dest, &copy_title, false)
                .await?;
            let mut parent_editor = MetaEditor::open_or_init(&parent_text, parent_doc.carrier)?;
            let span_path = [Segment::Key(&spanning)];
            if parent_editor
                .append_value(&span_path, fig::Value::Str(down.clone()))
                .is_err()
            {
                parent_editor
                    .set_value(&span_path, fig::Value::Seq(vec![fig::Value::Str(down)]))?;
            }
            Some((parent.clone(), parent_editor.render()?))
        } else {
            None
        };

        // All edits computed; stage them. Node first, then its body copy (opaque
        // payload bytes carried verbatim), then the parent's updated entry.
        cs.write(&dest, copy_text);
        if let (Some(body_from), Some((body_to, _))) = (&body_from, &body_dest) {
            let bytes = self.fs().read(&self.root().join(body_from)).await?;
            cs.write(body_to, bytes);
        }
        if let Some((parent, text)) = parent_write {
            cs.write(parent, text);
        }

        // Identity hook — an eager policy assigns the copy an ID from birth
        // (idempotent: the down-link above already registered it under `id_links`).
        if self.identity().registration().fires_on(Trigger::Create)
            && self.index().id_for_path(&dest).is_none()
        {
            let id = self.mint_unique(&dest);
            self.index_mut().register(&id, &dest);
        }
        self.commit(cs).await?;
        Ok(dest)
    }

    /// The first free `stem-copy[-N].ext` beside `source`, paired with its body
    /// file's destination when `source` is a separated node (`body_from` is its
    /// current body). The suffix is bumped until *both* the node and its body are
    /// free, so a duplicated pair never half-collides with an existing one.
    async fn unique_copy_path(
        &self,
        source: &Path,
        body_from: Option<&Path>,
    ) -> Result<(PathBuf, Option<(PathBuf, String)>)> {
        let stem = source.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
            Error::Structure(format!("{} has no filename to copy", source.display()))
        })?;
        let ext = source.extension().and_then(|e| e.to_str());
        for n in 1.. {
            let suffix = if n == 1 {
                "-copy".to_string()
            } else {
                format!("-copy-{n}")
            };
            let name = match ext {
                Some(ext) => format!("{stem}{suffix}.{ext}"),
                None => format!("{stem}{suffix}"),
            };
            let node = match source.parent() {
                Some(dir) => dir.join(name),
                None => PathBuf::from(name),
            };
            let body = body_from.map(|b| body_sibling(&node, b));
            let node_free = !self.fs().try_exists(&self.root().join(&node)).await?;
            let body_free = match &body {
                Some((body_to, _)) => !self.fs().try_exists(&self.root().join(body_to)).await?,
                None => true,
            };
            if node_free && body_free {
                return Ok((node, body));
            }
        }
        unreachable!("the copy-suffix search is unbounded")
    }

    /// Convert the **path-form** links the document at `file` declares into
    /// `style` — re-spelling each relative/absolute path target in the target
    /// [`LinkStyle`](crate::link::LinkStyle) (root `/a`, relative `../a`, or bare
    /// canonical `a`) while its resolved destination, label, and wrapper stay
    /// exactly the same. Id-form, external, and nominal (alias) targets are left
    /// untouched — `style` governs only how a *path* is written. Both frontmatter
    /// relation links and body `[[…]]`/`[](…)` links are converted.
    ///
    /// **Per-file by default (DESIGN §8).** Converting `file` restyles only the
    /// links *it* declares; links elsewhere pointing *at* `file` are those
    /// documents' to convert (so a workspace can sit in a mixed style, which is
    /// valid and `check`-clean). With `recursive`, the same conversion is applied
    /// to every document in `file`'s spanning subtree. Returns the paths of the
    /// documents actually rewritten.
    pub async fn convert_link_style(
        &mut self,
        file: &Path,
        style: crate::link::LinkStyle,
        recursive: bool,
    ) -> Result<Vec<PathBuf>> {
        let file = link::normalize(file);
        if !self.fs().try_exists(&self.root().join(&file)).await? {
            return Err(Error::NotFound(file.to_path_buf()));
        }
        let targets = if recursive {
            self.spanning_subtree(&file).await?
        } else {
            vec![file]
        };
        // One set for the whole subtree, not one per document: a recursive
        // restyle is a single edit to the reader ("convert this subtree"), so a
        // failure two thirds of the way down should not leave the other third
        // converted. Every document is independent here — nothing reads what an
        // earlier one wrote — so the whole sweep stages cleanly.
        let mut cs = self.change();
        let mut changed = Vec::new();
        for path in &targets {
            if let Some(text) = self.restyle_document(path, style).await? {
                cs.write(path, text);
                changed.push(path.clone());
            }
        }
        self.commit(cs).await?;
        Ok(changed)
    }

    /// Convert the metadata block of the document at `file` to a different
    /// frontmatter language — re-emitting its embedded metadata in `format` while
    /// keeping the document's *embedding shape* (delimited frontmatter stays
    /// delimited, a ```` ```lang ```` code block stays a code block, an HTML island
    /// stays an island) and every value. Only the serialization changes; comments
    /// do not survive a cross-format rewrite (a YAML comment has no JSON home).
    ///
    /// **Per-file by default (DESIGN §8),** like [`convert_link_style`](Self::convert_link_style):
    /// a document's metadata format is its own to declare, so a workspace may hold
    /// a mix. With `recursive`, every document in `file`'s spanning subtree is
    /// converted. A document already in `format`, or one with no metadata block, is
    /// left untouched; a *whole-file* (separate/config) document is not an embedded
    /// block to re-fence — converting one would rename the file and re-point its
    /// links — so it is an error to name one directly and is skipped under a
    /// recursive sweep. Returns the paths of the documents actually rewritten.
    pub async fn convert_meta_format(
        &mut self,
        file: &Path,
        format: fig::Format,
        recursive: bool,
    ) -> Result<Vec<PathBuf>> {
        self.reformat_sweep(file, ReformatAxis::Format(format), recursive)
            .await
    }

    /// Convert the metadata block of the document at `file` to a different
    /// *embedding shape* — re-emitting its frontmatter as `style`
    /// (`delimited`/`code_block`/`html_script`/`html_code`) while keeping its
    /// frontmatter *language* and every value. The companion of
    /// [`convert_meta_format`](Self::convert_meta_format) on the other metadata axis:
    /// where that keeps the shape and swaps the language, this keeps the language
    /// and swaps the shape — so a `delimited` YAML block can become a ```` ```yaml ````
    /// code block that can then hold fig.
    ///
    /// Same discipline as its companion: per-file by default (`recursive` sweeps the
    /// spanning subtree), a no-op when the document is already in `style` or carries
    /// no block. Two shapes are out of scope and rejected: `separate` (moving
    /// metadata to a sibling file is a move, not a re-fence), and a language the
    /// target shape cannot carry (`delimited` + fig — fig has no delimiter syntax).
    /// Returns the paths of the documents actually rewritten.
    pub async fn convert_meta_embed(
        &mut self,
        file: &Path,
        style: EmbedStyle,
        recursive: bool,
    ) -> Result<Vec<PathBuf>> {
        self.reformat_sweep(file, ReformatAxis::Embed(style), recursive)
            .await
    }

    /// The shared engine behind [`convert_meta_format`](Self::convert_meta_format)
    /// and [`convert_meta_embed`](Self::convert_meta_embed): resolve the target
    /// document set (this file, or its spanning subtree under `recursive`) and
    /// reformat each along `axis`, staging the whole sweep as one change set.
    ///
    /// One set for the subtree, not one per document — a recursive convert is a
    /// single edit to the reader, so a failure partway down leaves nothing
    /// converted. Every document is independent (nothing reads what another wrote),
    /// so the sweep stages cleanly. `named` gates whether an out-of-scope document
    /// is a hard error (the user pointed at it) or a skip (it merely fell inside
    /// the subtree).
    async fn reformat_sweep(
        &mut self,
        file: &Path,
        axis: ReformatAxis,
        recursive: bool,
    ) -> Result<Vec<PathBuf>> {
        let file = link::normalize(file);
        if !self.fs().try_exists(&self.root().join(&file)).await? {
            return Err(Error::NotFound(file.to_path_buf()));
        }
        let targets = if recursive {
            self.spanning_subtree(&file).await?
        } else {
            vec![file.clone()]
        };
        let mut cs = self.change();
        let mut changed = Vec::new();
        for path in &targets {
            let named = path == &file;
            if let Some(text) = self.reformat_document(path, axis, named).await? {
                cs.write(path, text);
                changed.push(path.clone());
            }
        }
        self.commit(cs).await?;
        Ok(changed)
    }

    /// Reformat the metadata block of the document at `path` along `axis`, returning
    /// its new text, or `None` when nothing should change (no metadata block, or the
    /// document already sits at the requested value, or an out-of-scope whole-file
    /// document under a recursive sweep). `named` is true when the caller pointed at
    /// this document directly, so an out-of-scope document is a hard error rather
    /// than a silent skip.
    ///
    /// The two axes converge on the same reconstruction: resolve a target
    /// [`EmbedType`](crate::document::EmbedType) from the document's `(style, format)`
    /// pair with one coordinate replaced, then re-emit the block in it. `Format`
    /// replaces the format and keeps the current style; `Embed` replaces the style
    /// and keeps the current format.
    async fn reformat_document(
        &self,
        path: &Path,
        axis: ReformatAxis,
        named: bool,
    ) -> Result<Option<String>> {
        let (_, doc) = self.load(path).await?;
        let Some(mapping) = doc.meta.as_mapping() else {
            return Ok(None); // no metadata block to convert
        };
        let kind = match doc.carrier {
            Some(MetaCarrier::Fenced(kind)) => kind,
            // The whole file *is* the metadata: re-embedding it means creating or
            // deleting a fenced host and moving the body — a move, not a re-fence,
            // and out of scope. An error when named directly; skipped in a sweep.
            Some(MetaCarrier::WholeFile(_)) if named => {
                return Err(Error::Structure(format!(
                    "{}: whole-file (separate) metadata — its format is its file \
                     extension and its shape is its own file; converting it is a move, \
                     not supported by `convert`",
                    path.display()
                )));
            }
            _ => return Ok(None),
        };
        // Resolve the target `(style, format)` from the current pair with the axis's
        // coordinate replaced; a document already at the requested value is a no-op.
        let (style, format) = match axis {
            ReformatAxis::Format(format) => {
                if kind.inner_format() == format {
                    return Ok(None);
                }
                (crate::document::embed_style_of(kind), format)
            }
            ReformatAxis::Embed(style) => {
                if crate::document::embed_style_of(kind) == style {
                    return Ok(None);
                }
                // `separate` is a whole-file sidecar, not a fenced shape — the same
                // move `WholeFile` above is, in the other direction.
                if style == EmbedStyle::Separate {
                    return Err(Error::Structure(format!(
                        "{}: `separate` moves metadata into a sibling file and re-points \
                         its links — a move, not supported by `convert`",
                        path.display()
                    )));
                }
                (style, kind.inner_format())
            }
        };
        let target = match crate::document::embed_carrier(style, format) {
            Some(MetaCarrier::Fenced(target)) => target,
            // The only `(style, format)` with no fenced archetype is
            // `delimited` + fig — the fig dialect has no `---`-style delimiter. A
            // real "impossible as asked" (reached converting *to* fig from a
            // delimited block, or *to* delimited from a fig one), so it errors
            // rather than skipping, aborting any recursive sweep.
            _ => {
                let fmt = crate::config::metadata_format_str(format);
                return Err(Error::Structure(format!(
                    "{}: a {} block cannot carry {fmt} — {fmt} has no delimiter syntax; \
                     use a code_block or HTML embedding",
                    path.display(),
                    style.as_config_str(),
                )));
            }
        };
        Ok(Some(crate::edit::reformat_block(
            &doc.body, mapping, target,
        )?))
    }

    /// Restyle every path link the document at `path` declares — frontmatter
    /// relation entries then body links — returning its new text, or `None` when
    /// nothing changed (so a no-op restyle stages no write). The body is spliced
    /// against `doc.body` (verbatim, MetaEditor-preserved) after the frontmatter
    /// edit, the same two-step `rename` uses.
    async fn restyle_document(
        &self,
        path: &Path,
        style: crate::link::LinkStyle,
    ) -> Result<Option<String>> {
        let (text, doc) = self.load(path).await?;
        let meta_rewritten =
            restyle_frontmatter_links(&text, &doc, self.relations().relations(), path, style)?;
        let final_text = restyle_body_links(&meta_rewritten, &doc.body, path, style);
        Ok((final_text != text).then_some(final_text))
    }

    /// Every document reachable from `root` down the spanning relation, `root`
    /// included — the scope of a `recursive` per-file operation. A missing,
    /// cyclic, or unreadable child simply stops that branch; the walk never
    /// leaves the spanning tree.
    async fn spanning_subtree(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut queue = vec![root.to_path_buf()];
        while let Some(path) = queue.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            let Ok((_, doc)) = self.load(&path).await else {
                continue;
            };
            out.push(path.clone());
            for raw in self.relations().children(&doc.meta) {
                if let Target::Path(child) = self.resolve_link(&path, &Link::parse(&raw)) {
                    queue.push(child);
                }
            }
        }
        Ok(out)
    }

    /// Every document that links to `from` by a path, rewritten to point at `to`
    /// — the inbound half of a move. Reused by `rename`, `separate`, and
    /// `combine`. Id-form links are left untouched (the registry keeps them
    /// resolving); `from`'s own links are excluded (the mover rewrites those
    /// itself). Returns `(source_path, new_text)` pairs.
    async fn collect_inbound_rewrites(
        &self,
        from: &Path,
        to: &Path,
    ) -> Result<Vec<(PathBuf, String)>> {
        let (_spanning, inverse) = self.spanning_pair()?;
        let root = self.spanning_root(from, &inverse).await?;
        let mut sources: BTreeSet<PathBuf> = self
            .census(&root)
            .await?
            .into_iter()
            .filter(|e| {
                matches!(&e.resolution,
                    Resolution::Path(p) | Resolution::CaseMismatch { got: p, .. } if p == from)
            })
            .map(|e| e.source)
            .collect();
        sources.remove(from);
        let mut writes = Vec::new();
        for source in sources {
            if let Some(updated) = self.rewrite_inbound_doc(&source, from, to).await? {
                writes.push((source, updated));
            }
        }
        Ok(writes)
    }

    /// If `from` is a separated node, plan the move of its body file to sit
    /// beside `to`, with its prose wikilinks re-relativized when the directory
    /// changes. `None` for a combined document.
    ///
    /// The body's new name follows the pair's naming convention. A separated
    /// **prose** node shares its body's stem (`notes.yaml` ↔ `notes.md`), so the
    /// body keeps its own extension on the new stem. An **attachment** node
    /// carries the whole payload name plus a metadata extension (`hero.jpg.yaml`
    /// ↔ `hero.jpg`), so the payload name *is* the node's stem — reconstructing it
    /// with the body's extension would double it (`hero.jpg.jpg`).
    async fn plan_body_move(
        &self,
        doc: &Document,
        from: &Path,
        to: &Path,
    ) -> Result<Option<BodyMove>> {
        let Some(body_from) = content_target(doc, from) else {
            return Ok(None);
        };
        let opaque = crate::document::is_opaque_payload(&body_from);
        let (body_to, new_ref) = body_sibling(to, &body_from);
        // An *attachment* payload is opaque bytes (an image, a PDF) — never read
        // it as text, and never rewrite it. The bare `rename` carries the bytes;
        // `text` stays `None`. A prose body is loaded and its wikilinks
        // re-relativized when the directory changes, as before.
        let text = if opaque {
            None
        } else {
            let (raw, _) = self.load(&body_from).await?;
            Some(if from.parent() != to.parent() {
                rerelativize_body_links(&raw, &raw, &body_from, &body_to)
            } else {
                raw
            })
        };
        Ok(Some(BodyMove {
            from: body_from,
            to: body_to,
            new_ref,
            text,
        }))
    }

    /// The spanning relation's name and its inverse — mutations need both.
    pub(crate) fn spanning_pair(&self) -> Result<(String, String)> {
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
        let raw = doc
            .meta
            .get(field)
            .map(Value::link_strings)?
            .into_iter()
            .next()?;
        match self.resolve_link(doc_path, &Link::parse(&raw)) {
            Target::Path(p) => Some(p),
            _ => None,
        }
    }

    /// The index of the entry in `doc`'s `field` sequence whose target
    /// resolves to `wanted` — by relative path or through the registry.
    fn entry_index(
        &self,
        doc: &Document,
        field: &str,
        doc_path: &Path,
        wanted: &Path,
    ) -> Option<usize> {
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
        let Some(carrier) = doc.carrier else {
            return Ok(None); // no metadata block: nothing to rewrite
        };
        let mut editor = MetaEditor::open(text, carrier)?;
        // A scalar field is addressed by key; a sequence entry by key + index.
        if value.as_sequence().is_some() {
            editor.replace_value(
                &[Segment::Key(field), Segment::Index(index)],
                fig::Value::Str(updated.render()),
            )?;
        } else {
            editor.replace_value(&[Segment::Key(field)], fig::Value::Str(updated.render()))?;
        }
        Ok(Some(editor.render()?))
    }

    /// Walk `part_of` (the spanning inverse) up from `from` to the spanning
    /// root — the document nothing contains — so a census can cover `from`'s
    /// whole workspace. A cycle or an unreadable ancestor stops the walk at the
    /// last good document, which still roots a scan over `from`'s neighborhood.
    async fn spanning_root(&self, from: &Path, inverse: &str) -> Result<PathBuf> {
        let mut current = from.to_path_buf();
        let mut seen = BTreeSet::new();
        while seen.insert(current.clone()) {
            let Ok((_, doc)) = self.load(&current).await else {
                break;
            };
            match self.single_target(&doc, inverse, &current) {
                Some(parent) => current = parent,
                None => break,
            }
        }
        Ok(current)
    }

    /// Retarget every path-form reference to `from` in the document at `source`
    /// so it reaches `to`: body wikilinks first (their spans index the current
    /// body), then each frontmatter relation entry (re-parsing between edits).
    /// Returns the updated text, or `None` when nothing in `source` pointed at
    /// `from`. Id-form links are skipped by [`retarget_entry`] and
    /// [`rewrite_body_inbound`] alike.
    async fn rewrite_inbound_doc(
        &self,
        source: &Path,
        from: &Path,
        to: &Path,
    ) -> Result<Option<String>> {
        let (original, doc0) = self.load(source).await?;
        let mut text = rewrite_body_inbound(&original, &doc0.body, source, from, to);
        let mut doc = if text != original {
            Document::parse(source, &text)?
        } else {
            doc0
        };
        for relation in self.relations().relations() {
            if let Some(updated) =
                self.retarget_entry(&text, &doc, &relation.name, source, from, to)?
            {
                text = updated;
                doc = Document::parse(source, &text)?;
            }
        }
        Ok((text != original).then_some(text))
    }
}

/// A planned move of a separated document's body file, computed during `rename`
/// (see [`Workspace::plan_body_move`]) and applied in its write phase.
struct BodyMove {
    /// The body file's current workspace-relative path.
    from: PathBuf,
    /// Where the body file moves to (beside the renamed metadata file).
    to: PathBuf,
    /// The metadata file's new `content` value — the body file's basename.
    new_ref: String,
    /// The prose body's text, wikilinks re-relativized if the directory changed,
    /// to rewrite after the move. `None` for an opaque attachment payload, whose
    /// bytes the bare rename carries untouched.
    text: Option<String>,
}

/// Where a separated node's body file sits beside a node placed at `node_to`,
/// and the `content` value (the body's basename) that points at it. `body_from`
/// is the current body file, whose shape decides the naming convention: an
/// **attachment** payload (opaque bytes) *is* the node's stem and already
/// carries its own extension (`hero.jpg.yaml` ↔ `hero.jpg`), while a separated
/// **prose** body shares the node's stem and keeps its own extension
/// (`notes.yaml` ↔ `notes.md`). Shared by [`Workspace::plan_body_move`] (rename)
/// and [`Workspace::duplicate`].
fn body_sibling(node_to: &Path, body_from: &Path) -> (PathBuf, String) {
    let body_to = if crate::document::is_opaque_payload(body_from) {
        let stem = node_to
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        node_to.with_file_name(stem)
    } else {
        let ext = body_from
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("md");
        node_to.with_extension(ext)
    };
    let new_ref = body_to
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    (body_to, new_ref)
}

/// The workspace-relative path a document's `content` attribute points at (its
/// separated body file), resolved against the document's own directory. `None`
/// for a combined document.
fn content_target(doc: &Document, doc_path: &Path) -> Option<PathBuf> {
    let raw = doc.content_attr()?;
    let dir = doc_path.parent().unwrap_or(Path::new(""));
    Some(link::normalize(dir.join(raw)))
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
    let Some(carrier) = doc.carrier else {
        return Ok(text.to_string()); // no metadata: nothing to re-relativize
    };
    let mut editor = MetaEditor::open(text, carrier)?;
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
            Some(
                target
                    .with_target(link::relative(new_dir, &resolved))
                    .render(),
            )
        };
        match value {
            Value::String(raw) => {
                if let Some(updated) = rewrite(raw) {
                    editor
                        .replace_value(&[Segment::Key(&relation.name)], fig::Value::Str(updated))?;
                }
            }
            Value::Sequence(items) => {
                for (i, item) in items.iter().enumerate() {
                    if let Some(raw) = item.as_str()
                        && let Some(updated) = rewrite(raw)
                    {
                        editor.replace_value(
                            &[Segment::Key(&relation.name), Segment::Index(i)],
                            fig::Value::Str(updated),
                        )?;
                    }
                }
            }
            _ => {}
        }
    }
    editor.render()
}

/// Restyle every path-form frontmatter link `doc` declares into `style`,
/// keeping its resolved destination, label, and wrapper — the frontmatter half
/// of [`convert_link_style`](Workspace::convert_link_style). The sibling of
/// [`rerelativize`], but where that recomputes a *relative* target for a move,
/// this re-spells a stationary target in a chosen [`LinkStyle`]. Id-form,
/// external, and nominal (alias) targets are skipped — `style` describes only
/// how a path is written.
fn restyle_frontmatter_links(
    text: &str,
    doc: &Document,
    relations: &[crate::relation::Relation],
    file: &Path,
    style: crate::link::LinkStyle,
) -> Result<String> {
    let Some(carrier) = doc.carrier else {
        return Ok(text.to_string()); // no metadata: nothing to restyle
    };
    let mut editor = MetaEditor::open(text, carrier)?;
    let restyle = |raw: &str| -> Option<String> {
        let link = Link::parse(raw);
        if link.is_external()
            || link.id_target().is_some()
            || crate::title::is_alias_shaped(&link.target)
        {
            return None;
        }
        let resolved = link::resolve(file, &link.target);
        Some(
            link.with_target(link::path_text(style, file, &resolved))
                .render(),
        )
    };
    for relation in relations {
        let Some(value) = doc.meta.get(&relation.name) else {
            continue;
        };
        match value {
            Value::String(raw) => {
                if let Some(updated) = restyle(raw) {
                    editor
                        .replace_value(&[Segment::Key(&relation.name)], fig::Value::Str(updated))?;
                }
            }
            Value::Sequence(items) => {
                for (i, item) in items.iter().enumerate() {
                    if let Some(raw) = item.as_str()
                        && let Some(updated) = restyle(raw)
                    {
                        editor.replace_value(
                            &[Segment::Key(&relation.name), Segment::Index(i)],
                            fig::Value::Str(updated),
                        )?;
                    }
                }
            }
            _ => {}
        }
    }
    editor.render()
}

/// Restyle the path-form body links in `body` into `style`, splicing the result
/// back into `text` — the body half of
/// [`convert_link_style`](Workspace::convert_link_style), covering `[[…]]` and
/// markdown/djot `[t](a)` links alike. Id-form, external, and alias targets are
/// left alone. Returns `text` unchanged when nothing restyled.
fn restyle_body_links(
    text: &str,
    body: &str,
    file: &Path,
    style: crate::link::LinkStyle,
) -> String {
    if body.is_empty() {
        return text.to_string();
    }
    let mut new_body = String::with_capacity(body.len());
    let mut cursor = 0;
    let mut rewrote = false;
    for bl in link::scan_body_links(file, body) {
        if bl.id_target().is_some()
            || bl.link.is_external()
            || crate::title::is_alias_shaped(&bl.link.target)
        {
            continue;
        }
        let resolved = link::resolve(file, &bl.link.target);
        let retargeted = bl
            .link
            .with_target(link::path_text(style, file, &resolved))
            .render();
        new_body.push_str(&body[cursor..bl.span.start]);
        new_body.push_str(&retargeted);
        cursor = bl.span.end;
        rewrote = true;
    }
    if !rewrote {
        return text.to_string();
    }
    new_body.push_str(&body[cursor..]);
    splice_body(text, body, &new_body)
}

/// Re-relativize the path-form body links in a moved document's body —
/// `[[wikilinks]]` and markdown/djot `[t](a)` links alike — so they still
/// resolve from `to`'s directory, then splice the rewritten body back into
/// `text` (the already-frontmatter-rewritten document). `body` is the moved
/// document's verbatim prose, which MetaEditor preserved byte-for-byte, so it is
/// still a contiguous run of `text`. Id-form (`id:<id>`) and external
/// (`scheme://…`) targets are left alone — neither depends on where the document
/// lives. Each link keeps its own wrapper on rewrite ([`Link::render`]), so a
/// wikilink stays `[[…]]` and a markdown link stays `[label](…)`. Returns `text`
/// unchanged when the body has no rewritable link.
fn rerelativize_body_links(text: &str, body: &str, from: &Path, to: &Path) -> String {
    if body.is_empty() {
        return text.to_string();
    }
    let new_dir = to.parent().unwrap_or(Path::new(""));
    let mut new_body = String::with_capacity(body.len());
    let mut cursor = 0;
    let mut rewrote = false;
    for bl in link::scan_body_links(from, body) {
        // ID-form (stable by construction) and external targets stay put; the
        // text between `cursor` and this span — including any such skipped
        // link — is copied verbatim by the next span's push (or the tail).
        if bl.id_target().is_some() || bl.link.is_external() {
            continue;
        }
        let resolved = link::resolve(from, &bl.link.target);
        let retargeted = bl
            .link
            .with_target(link::relative(new_dir, &resolved))
            .render();
        new_body.push_str(&body[cursor..bl.span.start]);
        new_body.push_str(&retargeted);
        cursor = bl.span.end;
        rewrote = true;
    }
    if !rewrote {
        return text.to_string();
    }
    new_body.push_str(&body[cursor..]);
    splice_body(text, body, &new_body)
}

/// Replace the single verbatim occurrence of `old_body` in `text` with
/// `new_body`. The body sits at one end of the document (a suffix under
/// frontmatter, a prefix under endmatter, or the whole text when there is no
/// metadata block), so those cases are matched directly; the general
/// single-replacement is the fallback.
fn splice_body(text: &str, old_body: &str, new_body: &str) -> String {
    if let Some(head) = text.strip_suffix(old_body) {
        format!("{head}{new_body}")
    } else if let Some(tail) = text.strip_prefix(old_body) {
        format!("{new_body}{tail}")
    } else {
        text.replacen(old_body, new_body, 1)
    }
}

/// Retarget the path-form body links in `source` that resolve to `from` so
/// they reach `to` instead, splicing the result back into `text` — both
/// `[[wikilinks]]` and markdown/djot `[t](a)` links. Id-form and external
/// targets are left untouched. Rewrites right-to-left so each span stays valid
/// as earlier ones are replaced. Returns `text` unchanged when no body link
/// pointed at `from`.
fn rewrite_body_inbound(text: &str, body: &str, source: &Path, from: &Path, to: &Path) -> String {
    if body.is_empty() {
        return text.to_string();
    }
    let source_dir = source.parent().unwrap_or(Path::new(""));
    let mut new_body = body.to_string();
    let mut changed = false;
    for bl in link::scan_body_links(source, body).into_iter().rev() {
        if bl.id_target().is_some() || bl.link.is_external() {
            continue;
        }
        if link::resolve(source, &bl.link.target).as_path() != from {
            continue;
        }
        let retargeted = bl.link.with_target(link::relative(source_dir, to)).render();
        new_body.replace_range(bl.span.clone(), &retargeted);
        changed = true;
    }
    if !changed {
        return text.to_string();
    }
    splice_body(text, body, &new_body)
}

// These engine tests use YAML fixtures throughout, so they run whenever the
// (default) `yaml` feature is on.
#[cfg(all(test, feature = "yaml"))]
mod tests {
    use super::*;
    use crate::exec::block_on;
    use crate::fs::{FailAtWrite, StdFs};
    use crate::identity::Minter;
    use crate::index::FileIndex;
    use crate::link::LinkStyle;

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> String {
        std::fs::read_to_string(dir.join(rel)).unwrap()
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("prov-mutate-{tag}-{}", std::process::id()));
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
    fn scan_ids_rebuilds_the_id_map_from_frontmatter() {
        // Frontmatter-only storage: each document carries its own `id`; a flat
        // scan reconstructs the id→path map with no registry document.
        let dir = tempdir("scan-ids");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\nid: aaaaaaa\n---\nbody\n",
        );
        write(
            &dir,
            "sub/child.md",
            "---\ntitle: Child\nid: bbbbbbb\n---\nbody\n",
        );
        // A document with no `id` is simply absent from the map, not an error.
        write(&dir, "sub/plain.md", "---\ntitle: Plain\n---\nbody\n");

        let mut ids = block_on(ws(&dir).scan_ids()).unwrap();
        ids.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        assert_eq!(
            ids,
            vec![
                (
                    crate::identity::Id("aaaaaaa".into()),
                    PathBuf::from("index.md")
                ),
                (
                    crate::identity::Id("bbbbbbb".into()),
                    PathBuf::from("sub/child.md")
                ),
            ]
        );
    }

    // Exercises inheritance of a `fig`-dialect parent block, so it needs that
    // backend on top of the module-wide `yaml` gate.
    #[cfg(feature = "fig-lang")]
    #[test]
    fn create_links_both_directions_in_the_parents_format() {
        let dir = tempdir("create");
        write(&dir, "index.md", "```fig\ntitle = Root\n```\nbody\n");
        // Plain-relative keeps the authored links bare and deterministic.
        let w = || {
            Workspace::builder(StdFs)
                .root(&dir)
                .link_style(LinkStyle::PlainRelative)
                .build()
        };
        block_on(w().create(Path::new("notes/new.md"), Path::new("index.md"))).unwrap();

        let child = read(&dir, "notes/new.md");
        assert!(
            child.starts_with("```fig\n"),
            "child inherits the parent's archetype: {child}"
        );
        assert!(child.contains("part_of = ../index.md"), "{child}");
        let parent = read(&dir, "index.md");
        // fig ≥ 2.2 renders spliced containers as flow — the round-trippable
        // inline spelling.
        assert!(parent.contains("contents = [notes/new.md]"), "{parent}");
        assert!(parent.ends_with("body\n"), "body untouched: {parent}");
        // The result validates cleanly.
        assert_eq!(block_on(w().check("index.md")).unwrap(), vec![]);
    }

    #[cfg(feature = "fig-lang")]
    #[test]
    fn create_uses_the_workspace_default_embed_format() {
        // The parent is a config file (whole-file metadata), so the child
        // inherits no fenced archetype and falls to the workspace default —
        // here fig, not the built-in YAML.
        let dir = tempdir("embed-default");
        write(&dir, "index.yaml", "title: Root\n");
        let mut w = Workspace::builder(StdFs)
            .root(&dir)
            .default_embed_format(fig::Format::Fig)
            .build();
        block_on(w.create(Path::new("a.md"), Path::new("index.yaml"))).unwrap();
        assert!(
            read(&dir, "a.md").starts_with("```fig"),
            "{}",
            read(&dir, "a.md")
        );
    }

    #[test]
    fn create_authors_id_links_when_configured() {
        // Obsidian-style: both structural links are authored by id, and both
        // ends are registered so the links survive any later move untouched.
        let dir = tempdir("create-id");
        write(&dir, "index.md", "---\ntitle: Root\n---\n");
        let mut w = Workspace::builder(StdFs)
            .root(&dir)
            .identity(Minter::lazy(7))
            .index(FileIndex::new(fig::Format::Yaml))
            .id_links(true)
            .build();
        block_on(w.create(Path::new("a.md"), Path::new("index.md"))).unwrap();

        let parent_id = w
            .index()
            .id_for_path(Path::new("index.md"))
            .expect("parent registered");
        let child_id = w
            .index()
            .id_for_path(Path::new("a.md"))
            .expect("child registered");
        assert!(read(&dir, "a.md").contains(&format!("part_of: id:{parent_id}")));
        assert!(read(&dir, "index.md").contains(&format!("id:{child_id}")));
        // And it still validates — id targets resolve through the registry.
        assert_eq!(block_on(w.check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn create_authors_up_and_down_in_different_relation_styles() {
        use crate::link::{Addressing, ReferenceStyle, Wrapper};
        use crate::relation::{Relation, RelationSet};

        // Down (`contents`) reads like a TOC — an alias wikilink. Up (`part_of`)
        // is durable bookkeeping — a bare markdown id link. Two relations, two
        // styles, one create.
        let alias = ReferenceStyle {
            wrapper: Wrapper::Wikilink,
            addressing: Addressing::Alias,
            label: false,
            path_style: LinkStyle::default(),
        };
        let by_id = ReferenceStyle {
            wrapper: Wrapper::Markdown,
            addressing: Addressing::Id,
            label: false,
            path_style: LinkStyle::default(),
        };
        let relations = RelationSet::new()
            .with(Relation::many("contents").inverse("part_of").style(alias))
            .with(Relation::one("part_of").inverse("contents").style(by_id))
            .spanning("contents");

        let dir = tempdir("create-updown");
        write(&dir, "index.md", "---\ntitle: Root\n---\n");
        let mut w = Workspace::builder(StdFs)
            .root(&dir)
            .relations(relations)
            .identity(Minter::lazy(7))
            .index(FileIndex::new(fig::Format::Yaml))
            .build();
        block_on(w.create(Path::new("a.md"), Path::new("index.md"))).unwrap();

        // Up: `part_of` on the child is a durable id link, and it registered the
        // parent (the id direction is what triggers registration).
        let parent_id = w
            .index()
            .id_for_path(Path::new("index.md"))
            .expect("parent registered");
        assert!(
            read(&dir, "a.md").contains(&format!("part_of: id:{parent_id}")),
            "{}",
            read(&dir, "a.md")
        );

        // Down: `contents` on the parent is a nominal alias wikilink (the child's
        // title), and — because `alias` never links-by-id — the child is *not*
        // registered. That asymmetry is by design.
        assert!(
            read(&dir, "index.md").contains("[[a]]"),
            "{}",
            read(&dir, "index.md")
        );
        assert!(
            w.index().id_for_path(Path::new("a.md")).is_none(),
            "alias down-link must not register the child"
        );
    }

    #[test]
    fn create_makes_a_separated_child_under_a_separated_parent() {
        // A separated parent is a whole-file metadata node with a `content`
        // pointer at its prose body. A new child inherits that shape: a body
        // file plus a sibling metadata node — the node is what the parent links.
        let dir = tempdir("create-separate");
        write(&dir, "index.yaml", "title: Root\ncontent: index.md\n");
        write(&dir, "index.md", "# Root\n");

        block_on(ws(&dir).create(Path::new("notes.md"), Path::new("index.yaml"))).unwrap();

        // The structural node is `notes.yaml`: title, inverse, and a `content`
        // pointer at its (empty) prose body.
        let node = read(&dir, "notes.yaml");
        assert!(node.contains("title: notes"), "{node}");
        assert!(
            node.contains("index.yaml"),
            "inverse link to parent node: {node}"
        );
        assert!(node.contains("content: notes.md"), "{node}");
        assert_eq!(read(&dir, "notes.md"), "", "the body file starts empty");
        // The parent's spanning entry points at the node, never the body file.
        let index = read(&dir, "index.yaml");
        assert!(index.contains("notes.yaml"), "{index}");
        assert!(
            !index.contains("notes.md"),
            "parent links the node, not the body: {index}"
        );
        // The whole (separated) workspace still validates.
        assert_eq!(block_on(ws(&dir).check("index.yaml")).unwrap(), vec![]);
    }

    #[test]
    fn adopt_links_an_existing_document_both_ways_preserving_its_body() {
        // A loose note that predates the workspace: adoption links it under the
        // root in both directions and leaves its prose untouched.
        let dir = tempdir("adopt");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        write(
            &dir,
            "notes/loose.md",
            "---\ntitle: Loose\n---\nOriginal body, kept.\n",
        );
        // The default markdown-root style authors `/index.md`, which resolves from
        // a subdirectory child (a bare canonical path would not).
        let mut w = Workspace::builder(StdFs).root(&dir).build();

        block_on(w.adopt(Path::new("notes/loose.md"), Path::new("index.md"))).unwrap();

        // Down: the root's spanning field gained the child.
        assert!(
            read(&dir, "index.md").contains("notes/loose.md"),
            "{}",
            read(&dir, "index.md")
        );
        // Up: the child declares part_of back to the root (workspace-absolute), and
        // keeps its body.
        let child = read(&dir, "notes/loose.md");
        assert!(child.contains("/index.md"), "{child}");
        assert!(
            child.contains("Original body, kept."),
            "body must be preserved: {child}"
        );
        // The whole workspace validates — no orphan, no missing inverse.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn reparent_moves_a_node_in_the_tree_and_leaves_the_file_alone() {
        // The complement of `rename`: the document's *path* is untouched, only its
        // place in the tree changes. The old parent forgets it, the new one gains
        // it, and its inverse points somewhere new — three documents, one verb.
        let dir = tempdir("reparent");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- '[Jul](/jul.md)'\n- '[Aug](/aug.md)'\n---\n",
        );
        write(
            &dir,
            "jul.md",
            "---\ntitle: Jul\npart_of: '[Home](/index.md)'\ncontents:\n- '[Day](/day.md)'\n---\n",
        );
        write(
            &dir,
            "aug.md",
            "---\ntitle: Aug\npart_of: '[Home](/index.md)'\ncontents:\n---\n",
        );
        write(
            &dir,
            "day.md",
            "---\ntitle: Day\npart_of: '[Jul](/jul.md)'\n---\nProse survives.\n",
        );

        block_on(ws(&dir).reparent(Path::new("day.md"), Path::new("aug.md"))).unwrap();

        assert!(
            !read(&dir, "jul.md").contains("day.md"),
            "old parent forgot it: {}",
            read(&dir, "jul.md")
        );
        assert!(
            read(&dir, "aug.md").contains("day.md"),
            "new parent gained it: {}",
            read(&dir, "aug.md")
        );
        let day = read(&dir, "day.md");
        assert!(day.contains("/aug.md"), "inverse repointed: {day}");
        assert!(!day.contains("/jul.md"), "old inverse gone: {day}");
        assert!(day.contains("Prose survives."), "body untouched: {day}");
        // The file never moved — that is `mv`'s job, not this one's.
        assert!(dir.join("day.md").exists(), "the path is preserved");
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn reparent_is_idempotent_and_adopts_an_unparented_child() {
        // Re-running is a no-op (a script should not have to ask first), and a
        // child with no parent at all is accepted: there is simply nothing to
        // remove, so the effect is exactly `adopt`'s.
        let dir = tempdir("reparent-idem");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        write(&dir, "loose.md", "---\ntitle: Loose\n---\n");

        block_on(ws(&dir).reparent(Path::new("loose.md"), Path::new("index.md"))).unwrap();
        let once = read(&dir, "index.md");
        block_on(ws(&dir).reparent(Path::new("loose.md"), Path::new("index.md"))).unwrap();
        assert_eq!(read(&dir, "index.md"), once, "second run changes nothing");
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn reparent_refuses_to_detach_a_subtree_under_its_own_descendant() {
        // Reparenting a node beneath something it contains would sever both from
        // the tree: they would still claim each other, so nothing looks broken
        // from inside the loop — a spanning walk would just never reach them
        // again. Refusing is the only way that stays visible.
        let dir = tempdir("reparent-cycle");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- '[A](/a.md)'\n---\n",
        );
        write(
            &dir,
            "a.md",
            "---\ntitle: A\npart_of: '[Home](/index.md)'\ncontents:\n- '[B](/b.md)'\n---\n",
        );
        write(&dir, "b.md", "---\ntitle: B\npart_of: '[A](/a.md)'\n---\n");

        let err = block_on(ws(&dir).reparent(Path::new("a.md"), Path::new("b.md"))).unwrap_err();
        assert!(
            err.to_string().contains("detach both from the tree"),
            "{err}"
        );
        // Refused means untouched, not half-done.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn adopt_is_idempotent_and_refuses_a_contested_parent() {
        let dir = tempdir("adopt-idem");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        write(&dir, "other.md", "---\ntitle: Other\n---\n");
        write(&dir, "a.md", "---\ntitle: A\n---\n");
        let mut w = Workspace::builder(StdFs)
            .root(&dir)
            .link_style(LinkStyle::PlainCanonical)
            .build();

        // First adoption links it; a second is a clean no-op (no duplicate entry).
        block_on(w.adopt(Path::new("a.md"), Path::new("index.md"))).unwrap();
        block_on(w.adopt(Path::new("a.md"), Path::new("index.md"))).unwrap();
        assert_eq!(
            read(&dir, "index.md").matches("a.md").count(),
            1,
            "no duplicate spanning entry"
        );

        // a.md now claims index.md; adopting it under a different parent is refused.
        let contested = block_on(w.adopt(Path::new("a.md"), Path::new("other.md")));
        assert!(contested.is_err(), "a contested parent must be refused");
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
    fn rename_rerelativizes_path_wikilinks_and_spares_id_ones() {
        // The identity-free (Diaryx-style) half: a moved document's body
        // wikilinks are maintained by rewriting the path form, while a
        // `[[colophon:id]]` reference is left exactly as written.
        let dir = tempdir("wikilink-rerel");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- mid.md\n---\n",
        );
        write(
            &dir,
            "mid.md",
            "---\npart_of: index.md\n---\nSee [[leaf.md|the leaf]] and [[colophon:ajp7eqb|pinned]].\n",
        );
        write(&dir, "leaf.md", "---\ntitle: Leaf\n---\n");

        block_on(ws(&dir).rename(Path::new("mid.md"), Path::new("sub/mid.md"))).unwrap();

        let mid = read(&dir, "sub/mid.md");
        // Path wikilink re-relativized (label kept) so it still reaches leaf.md.
        assert!(mid.contains("[[../leaf.md|the leaf]]"), "{mid}");
        // ID wikilink untouched — location-independent by construction.
        assert!(mid.contains("[[colophon:ajp7eqb|pinned]]"), "{mid}");
        // Frontmatter maintenance still holds, and the prose survives verbatim.
        assert!(mid.contains("part_of: ../index.md"), "{mid}");
        assert!(mid.ends_with(".\n"), "body preserved: {mid}");
        // Parent's spanning entry followed the move too.
        assert!(
            read(&dir, "index.md").contains("sub/mid.md"),
            "parent retargeted"
        );
    }

    #[test]
    fn rename_rerelativizes_markdown_body_links_and_spares_external_and_code() {
        // Stage 2: real markdown `[label](path)` links in body prose are now
        // maintained on a move, just like wikilinks — while an external URL and a
        // link that is actually code (inside a fence) are left untouched.
        let dir = tempdir("md-body-rerel");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- mid.md\n---\n",
        );
        write(
            &dir,
            "mid.md",
            "---\npart_of: index.md\n---\n\
             See [the leaf](leaf.md) and [home](https://ex.com).\n\n\
             ```\n[fake](leaf.md)\n```\n",
        );
        write(&dir, "leaf.md", "---\ntitle: Leaf\n---\n");

        block_on(ws(&dir).rename(Path::new("mid.md"), Path::new("sub/mid.md"))).unwrap();

        let mid = read(&dir, "sub/mid.md");
        // The inline markdown link was re-relativized, label kept, wrapper kept.
        assert!(mid.contains("[the leaf](../leaf.md)"), "{mid}");
        // The external URL is untouched.
        assert!(mid.contains("[home](https://ex.com)"), "{mid}");
        // The look-alike link inside the code fence must NOT be rewritten.
        assert!(
            mid.contains("[fake](leaf.md)"),
            "code fence left alone: {mid}"
        );
        assert!(
            read(&dir, "index.md").contains("sub/mid.md"),
            "parent retargeted"
        );
    }

    #[test]
    fn check_diagnoses_a_broken_markdown_body_link() {
        // A markdown body link to a missing file is now a broken-link finding —
        // the diagnosis half of body-link ownership. A wikilink to nowhere was
        // already caught; parity for markdown/djot links.
        let dir = tempdir("md-body-check");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- a.md\n---\n",
        );
        write(
            &dir,
            "a.md",
            "---\npart_of: index.md\n---\nSee [gone](nope.md).\n",
        );

        let findings = block_on(ws(&dir).check("index.md")).unwrap();
        assert!(
            findings.iter().any(|f| matches!(f,
                Finding::BrokenLink { doc, site: LinkSite::Body(_), target }
                    if doc == &PathBuf::from("a.md") && target == "nope.md")),
            "expected a broken markdown body link, got {findings:?}"
        );
    }

    #[test]
    fn rename_retargets_inbound_markdown_body_links() {
        // A sibling references the moved doc with a markdown body link; the census
        // finds it and the move retargets it — the inbound direction, for markdown.
        let dir = tempdir("md-body-inbound");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n- b.md\n---\n");
        write(&dir, "a.md", "---\npart_of: index.md\n---\n");
        write(
            &dir,
            "b.md",
            "---\npart_of: index.md\n---\nAlso see [it](a.md) nearby.\n",
        );

        block_on(ws(&dir).rename(Path::new("a.md"), Path::new("sub/a.md"))).unwrap();

        assert!(
            read(&dir, "b.md").contains("[it](sub/a.md)"),
            "inbound md link retargeted: {}",
            read(&dir, "b.md")
        );
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn same_directory_rename_leaves_body_wikilinks_alone() {
        // Outbound links resolve from the document's *directory*; a same-dir
        // rename does not move them, so the body must not churn.
        let dir = tempdir("wikilink-samedir");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- a.md\n---\n",
        );
        write(
            &dir,
            "a.md",
            "---\npart_of: index.md\n---\nlink to [[leaf.md]].\n",
        );
        write(&dir, "leaf.md", "---\ntitle: Leaf\n---\n");

        block_on(ws(&dir).rename(Path::new("a.md"), Path::new("b.md"))).unwrap();
        assert!(
            read(&dir, "b.md").contains("[[leaf.md]]"),
            "unchanged in-place"
        );
    }

    #[test]
    fn rename_retargets_overlay_and_body_inbound_links_anywhere() {
        // A sibling — neither parent nor child of the moved doc — references it
        // two ways: an overlay `links` relation and a body wikilink. Both must
        // follow the move; the census finds them where the old local spanning
        // walk never would. Identity-free: pure Diaryx-style path links.
        let dir = tempdir("inbound");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n- b.md\n---\n");
        write(&dir, "a.md", "---\npart_of: index.md\n---\n");
        write(
            &dir,
            "b.md",
            "---\npart_of: index.md\nlinks:\n- a.md\n---\nAlso see [[a.md]] nearby.\n",
        );

        block_on(ws(&dir).rename(Path::new("a.md"), Path::new("sub/a.md"))).unwrap();

        // Parent's spanning entry followed the move (as the old code did too).
        assert!(
            read(&dir, "index.md").contains("sub/a.md"),
            "parent retargeted"
        );
        let b = read(&dir, "b.md");
        // Overlay `links` inbound from a sibling — newly maintained.
        assert!(b.contains("- sub/a.md"), "overlay links retargeted: {b}");
        // Body wikilink inbound from a sibling — newly maintained.
        assert!(b.contains("[[sub/a.md]]"), "body wikilink retargeted: {b}");
        // The moved doc's own inverse re-relativized from its new location.
        assert!(read(&dir, "sub/a.md").contains("part_of: ../index.md"));
        // The whole workspace still validates.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn delete_refuses_children_then_forces() {
        let dir = tempdir("delete");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n- b.md\n---\n");
        write(
            &dir,
            "a.md",
            "---\npart_of: index.md\ncontents:\n- b.md\n---\n",
        );
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
    fn delete_diagnoses_inbound_references_left_dangling() {
        // A sibling links the doomed document two ways (overlay `links` + a body
        // wikilink). Delete removes the parent's spanning entry silently, but
        // reports the sibling's references it cannot rewrite — there is no new
        // target to send them to.
        let dir = tempdir("delete-inbound");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n- b.md\n---\n");
        write(&dir, "a.md", "---\npart_of: index.md\n---\n");
        write(
            &dir,
            "b.md",
            "---\npart_of: index.md\nlinks:\n- a.md\n---\nSee [[a.md]].\n",
        );

        let danglers = block_on(ws(&dir).delete(Path::new("a.md"), false)).unwrap();
        assert_eq!(danglers.len(), 2, "{danglers:?}");
        assert!(
            danglers.iter().any(|f| matches!(f,
                Finding::BrokenLink { doc, site: LinkSite::Relation(r), target }
                    if doc == &PathBuf::from("b.md") && r == "links" && target == "a.md")),
            "{danglers:?}"
        );
        assert!(
            danglers.iter().any(|f| matches!(f,
                Finding::BrokenLink { doc, site: LinkSite::Body(_), target }
                    if doc == &PathBuf::from("b.md") && target == "a.md")),
            "{danglers:?}"
        );
        // The parent's spanning entry was removed, not reported.
        assert!(
            !read(&dir, "index.md").contains("a.md"),
            "parent entry cleaned"
        );
    }

    #[test]
    fn create_with_title_keeps_the_title_distinct_from_the_slugged_stem() {
        // Title-primary authoring: the caller slugs the title into a readable
        // filename but the document records the original title verbatim (casing
        // and spaces the stem cannot carry), and the parent's entry label follows.
        let dir = tempdir("create-title");
        write(&dir, "index.md", "---\ntitle: Root\n---\n");
        let stem = crate::link::slug("My Great Note");
        assert_eq!(stem, "my-great-note");
        let path = PathBuf::from(format!("{stem}.md"));

        block_on(ws(&dir).create_with_title(&path, Path::new("index.md"), "My Great Note"))
            .unwrap();

        let child = read(&dir, "my-great-note.md");
        assert!(
            child.contains("title: My Great Note"),
            "original title kept: {child}"
        );
        // The parent's spanning-entry label reads as the title, not the stem.
        assert!(
            read(&dir, "index.md").contains("My Great Note"),
            "{}",
            read(&dir, "index.md")
        );
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn duplicate_copies_under_the_same_parent_and_links_both_ways() {
        let dir = tempdir("duplicate");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- a.md\n---\n",
        );
        write(
            &dir,
            "a.md",
            "---\ntitle: A\npart_of: index.md\n---\nA body, copied.\n",
        );

        let copy = block_on(ws(&dir).duplicate(Path::new("a.md"))).unwrap();
        assert_eq!(copy, PathBuf::from("a-copy.md"));

        // The copy carries the source's title and body verbatim, and keeps its
        // inverse link up (same directory → unchanged).
        let copied = read(&dir, "a-copy.md");
        assert!(copied.contains("title: A"), "{copied}");
        assert!(copied.contains("part_of: index.md"), "{copied}");
        assert!(copied.contains("A body, copied."), "{copied}");
        // The parent gained a spanning entry for the copy, keeping the original.
        let index = read(&dir, "index.md");
        assert!(index.contains("- a.md"), "original kept: {index}");
        assert!(index.contains("a-copy.md"), "copy attached: {index}");
        // The whole workspace validates — both directions are sound.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn duplicate_bumps_the_suffix_and_drops_cloned_children() {
        // A container with a child, duplicated: the copy is childless (no double
        // parent), and a second duplicate takes the next free `-copy-N` name.
        let dir = tempdir("duplicate-suffix");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- mid.md\n---\n",
        );
        write(
            &dir,
            "mid.md",
            "---\ntitle: Mid\npart_of: index.md\ncontents:\n- leaf.md\n---\n",
        );
        write(&dir, "leaf.md", "---\ntitle: Leaf\npart_of: mid.md\n---\n");

        let first = block_on(ws(&dir).duplicate(Path::new("mid.md"))).unwrap();
        assert_eq!(first, PathBuf::from("mid-copy.md"));
        // The copy did not clone the child — no contested containment for leaf.md.
        let copy = read(&dir, "mid-copy.md");
        assert!(
            !copy.contains("leaf.md"),
            "children must not be cloned: {copy}"
        );
        assert!(copy.contains("part_of: index.md"), "{copy}");

        // A second duplicate of the same source bumps past the taken name.
        let second = block_on(ws(&dir).duplicate(Path::new("mid.md"))).unwrap();
        assert_eq!(second, PathBuf::from("mid-copy-2.md"));
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn duplicate_does_not_clone_a_registered_id() {
        // Frontmatter-stamped identity must not travel to the copy: an ID names
        // exactly one document. The copy is a distinct path with (here, lazy) no
        // id of its own until something links it.
        let dir = tempdir("duplicate-id");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- a.md\n---\n",
        );
        write(
            &dir,
            "a.md",
            "---\ntitle: A\npart_of: index.md\nid: aaaaaaa\n---\nbody\n",
        );

        let copy = block_on(id_ws(&dir).duplicate(Path::new("a.md"))).unwrap();
        let copied = read(&dir, &copy.to_string_lossy());
        assert!(
            !copied.contains("aaaaaaa"),
            "the source's id must not be cloned: {copied}"
        );
        assert!(
            !copied.contains("\nid:"),
            "the copy carries no stamped id: {copied}"
        );
    }

    #[test]
    fn duplicate_a_separated_node_copies_its_body_and_repoints_content() {
        // A separated node (whole-file metadata + a `content` body pointer): the
        // copy is its own pair, pointing at its own body, not the source's.
        let dir = tempdir("duplicate-separate");
        write(&dir, "index.yaml", "title: Root\ncontents:\n- notes.yaml\n");
        write(
            &dir,
            "notes.yaml",
            "title: Notes\npart_of: index.yaml\ncontent: notes.md\n",
        );
        write(&dir, "notes.md", "Prose body, duplicated.\n");

        let copy = block_on(ws(&dir).duplicate(Path::new("notes.yaml"))).unwrap();
        assert_eq!(copy, PathBuf::from("notes-copy.yaml"));

        // The copy node points at its own body; the body file is a real copy.
        let node = read(&dir, "notes-copy.yaml");
        assert!(
            node.contains("content: notes-copy.md"),
            "repointed content: {node}"
        );
        assert_eq!(read(&dir, "notes-copy.md"), "Prose body, duplicated.\n");
        // Source body untouched, and the workspace validates.
        assert_eq!(read(&dir, "notes.md"), "Prose body, duplicated.\n");
        assert_eq!(block_on(ws(&dir).check("index.yaml")).unwrap(), vec![]);
    }

    #[test]
    fn convert_restyles_one_files_links_leaving_the_rest_alone() {
        // Per-file (DESIGN §8): converting mid.md restyles the links *it*
        // declares — its `part_of` up and a body link — into plain_relative,
        // preserving each destination and label. The root's inbound link to
        // mid.md is untouched (that's the root's to convert), so the workspace
        // sits in a valid mixed style.
        let dir = tempdir("convert-linkstyle");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- '[Mid](/sub/mid.md)'\n---\n",
        );
        write(
            &dir,
            "sub/mid.md",
            "---\ntitle: Mid\npart_of: /index.md\n---\nSee [the leaf](/sub/leaf.md).\n",
        );
        write(
            &dir,
            "sub/leaf.md",
            "---\ntitle: Leaf\npart_of: /sub/mid.md\n---\n",
        );

        let n = block_on(ws(&dir).convert_link_style(
            Path::new("sub/mid.md"),
            LinkStyle::PlainRelative,
            false,
        ))
        .unwrap();
        assert_eq!(n.len(), 1, "only the one file converted");

        let mid = read(&dir, "sub/mid.md");
        // Up-link and body link now relative (destinations preserved, label kept).
        assert!(mid.contains("part_of: ../index.md"), "{mid}");
        assert!(mid.contains("[the leaf](leaf.md)"), "{mid}");
        // The root's inbound link stays in its original root style — not this
        // file's to convert.
        assert!(
            read(&dir, "index.md").contains("[Mid](/sub/mid.md)"),
            "inbound untouched"
        );
        // And the mixed-style workspace still validates.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn convert_recursive_covers_the_spanning_subtree_and_spares_id_and_external() {
        // `-r` converts the file and its descendants. An `id:` link and an
        // external URL are left exactly as written — link_format spells only
        // *path* targets.
        let dir = tempdir("convert-recursive");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- a.md\n---\n",
        );
        write(
            &dir,
            "a.md",
            "---\ntitle: A\npart_of: index.md\ncontents:\n- sub/b.md\n---\n\
             See [ext](https://example.com) and [[id:ajp7eqb|pinned]].\n",
        );
        write(&dir, "sub/b.md", "---\ntitle: B\npart_of: ../a.md\n---\n");

        let n = block_on(ws(&dir).convert_link_style(
            Path::new("index.md"),
            LinkStyle::MarkdownRoot,
            true,
        ))
        .unwrap();
        assert_eq!(n.len(), 3, "root + a + b all converted");

        let a = read(&dir, "a.md");
        // Path links became root-absolute.
        assert!(a.contains("part_of: /index.md"), "{a}");
        assert!(a.contains("- /sub/b.md"), "{a}");
        // External and id targets untouched.
        assert!(a.contains("[ext](https://example.com)"), "{a}");
        assert!(a.contains("[[id:ajp7eqb|pinned]]"), "{a}");
        assert!(
            read(&dir, "sub/b.md").contains("part_of: /a.md"),
            "descendant converted"
        );
        // (No `check` here: `ajp7eqb` is a deliberately fake id, which `check`
        // would flag as malformed regardless of the conversion. The first
        // convert test validates a clean workspace after converting.)
    }

    #[cfg(feature = "json")]
    #[test]
    fn convert_meta_format_reserializes_the_block_keeping_values_and_body() {
        // A delimited YAML block becomes a delimited JSON block (`;;;`): every
        // value and the prose body survive, only the frontmatter language changes,
        // and the workspace still validates.
        let dir = tempdir("convert-meta-json");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- '[Leaf](/leaf.md)'\n---\n# Root\n\nprose\n",
        );
        write(
            &dir,
            "leaf.md",
            "---\ntitle: Leaf\npart_of: /index.md\n---\n",
        );

        let n =
            block_on(ws(&dir).convert_meta_format(Path::new("index.md"), fig::Format::Json, false))
                .unwrap();
        assert_eq!(n.len(), 1, "only the named file converted");

        let out = read(&dir, "index.md");
        assert!(out.starts_with(";;;\n"), "delimited JSON now: {out}");
        assert!(out.contains("\"title\": \"Root\""), "{out}");
        assert!(
            out.contains("[Leaf](/leaf.md)"),
            "link value preserved: {out}"
        );
        assert!(out.ends_with("# Root\n\nprose\n"), "body untouched: {out}");
        // Per-file: the leaf stays YAML, and the mixed-format workspace is clean.
        assert!(read(&dir, "leaf.md").starts_with("---\n"), "leaf untouched");
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    // fig has no `---`-style delimiter, so a *delimited* block cannot become fig,
    // but a *code-block* one can (```` ```fig ````): the embedding shape is kept.
    #[cfg(feature = "fig-lang")]
    #[test]
    fn convert_meta_format_keeps_the_embedding_shape_and_rejects_impossible_pairs() {
        let dir = tempdir("convert-meta-fig");
        // A code-block YAML document converts cleanly to a ```` ```fig ```` block.
        write(&dir, "code.md", "```yaml\ntitle: Root\n```\nbody\n");
        let n =
            block_on(ws(&dir).convert_meta_format(Path::new("code.md"), fig::Format::Fig, false))
                .unwrap();
        assert_eq!(n.len(), 1);
        let code = read(&dir, "code.md");
        assert!(code.starts_with("```fig\n"), "code block kept: {code}");
        assert!(code.contains("title = Root"), "fig dialect: {code}");
        assert!(code.ends_with("body\n"), "body untouched: {code}");

        // A delimited (`---`) block cannot carry fig — a hard error, not a silent skip.
        write(&dir, "delim.md", "---\ntitle: Root\n---\nbody\n");
        let err =
            block_on(ws(&dir).convert_meta_format(Path::new("delim.md"), fig::Format::Fig, false))
                .unwrap_err();
        assert!(
            err.to_string().contains("cannot carry fig"),
            "clear diagnostic: {err}"
        );
    }

    // Sequence layout is the trap here: fig's per-key Embed splice renders a
    // single-element sequence as a broken inline `* item`, so the reconstruction
    // must go through the canonical serializer. Needs fig on top of the yaml gate.
    #[cfg(feature = "fig-lang")]
    #[test]
    fn convert_meta_format_renders_sequences_the_canonical_way() {
        let dir = tempdir("convert-meta-seq");
        // A *single-element* `contents` is the case that exposed the splice bug.
        write(
            &dir,
            "index.md",
            "```yaml\ntitle: Root\ncontents:\n- '[Leaf](/leaf.md)'\n```\n# Root\n",
        );
        write(
            &dir,
            "leaf.md",
            "```yaml\ntitle: Leaf\npart_of: /index.md\n```\n",
        );

        block_on(ws(&dir).convert_meta_format(Path::new("index.md"), fig::Format::Fig, false))
            .unwrap();
        let out = read(&dir, "index.md");
        // The link survives as a real sequence element, not fused into a scalar.
        assert!(
            out.contains("[Leaf](/leaf.md)") && !out.contains("= * ["),
            "sequence stays well-formed: {out}"
        );
        // The proof it is well-formed: the workspace re-parses and validates.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[cfg(feature = "fig-lang")]
    #[test]
    fn convert_meta_embed_reshapes_the_block_and_unblocks_fig() {
        // The motivating flow: a `delimited` block cannot hold fig, but re-embedding
        // it as a `code_block` (language kept) can then be converted to fig.
        let dir = tempdir("convert-meta-embed");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- '[Leaf](/leaf.md)'\n---\n# Root\n",
        );
        write(
            &dir,
            "leaf.md",
            "---\ntitle: Leaf\npart_of: /index.md\n---\n",
        );

        // delimited → code_block keeps the YAML language, only the shape changes.
        let n = block_on(ws(&dir).convert_meta_embed(
            Path::new("index.md"),
            EmbedStyle::CodeBlock,
            false,
        ))
        .unwrap();
        assert_eq!(n.len(), 1);
        let code = read(&dir, "index.md");
        assert!(code.starts_with("```yaml\n"), "now a code block: {code}");
        assert!(code.ends_with("# Root\n"), "body untouched: {code}");
        // …which the format axis can now carry to fig.
        block_on(ws(&dir).convert_meta_format(Path::new("index.md"), fig::Format::Fig, false))
            .unwrap();
        assert!(read(&dir, "index.md").starts_with("```fig\n"));
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);

        // `separate` is a whole-file move, out of scope — a clear refusal.
        let err = block_on(ws(&dir).convert_meta_embed(
            Path::new("leaf.md"),
            EmbedStyle::Separate,
            false,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("separate"), "{err}");
    }

    #[cfg(feature = "json")]
    #[test]
    fn convert_meta_format_recursive_skips_no_ops_and_out_of_scope_documents() {
        // A recursive convert sweeps the spanning subtree. A document already in
        // the target format is a no-op; a whole-file (config) document is not a
        // fenced block and is skipped when merely swept — while naming one directly
        // is an error.
        let dir = tempdir("convert-meta-recursive");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- a.md\n---\n",
        );
        // `a.md` is already JSON, so the sweep leaves it untouched (a no-op).
        write(
            &dir,
            "a.md",
            ";;;\n{\"title\": \"A\", \"part_of\": \"index.md\"}\n;;;\n",
        );

        let n =
            block_on(ws(&dir).convert_meta_format(Path::new("index.md"), fig::Format::Json, true))
                .unwrap();
        assert_eq!(
            n.len(),
            1,
            "only the root actually changed (a.md was already JSON)"
        );
        assert!(read(&dir, "index.md").starts_with(";;;\n"));

        // Naming a whole-file config document directly is refused.
        write(&dir, "conf.yaml", "title: Config\n");
        let err = block_on(ws(&dir).convert_meta_format(
            Path::new("conf.yaml"),
            fig::Format::Json,
            false,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("whole-file"), "{err}");
    }

    #[test]
    fn config_pointer_resolves_and_reads_a_setting() {
        // Workspace policy lives in a config document the root links via the
        // `config` relation — the registry's reachability move, for config.
        let dir = tempdir("config");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\nconfig: prov.yaml\n---\n",
        );
        write(
            &dir,
            "prov.yaml",
            "title: prov config\npart_of: index.md\nlink_format: plain_relative\n",
        );
        let ws = ws(&dir);
        assert_eq!(
            block_on(ws.config_path(Path::new("index.md"))).unwrap(),
            Some(PathBuf::from("prov.yaml"))
        );
        let value = block_on(ws.config_get(Path::new("index.md"), "link_format")).unwrap();
        assert_eq!(
            value.and_then(|v| v.as_str().map(str::to_owned)),
            Some("plain_relative".into())
        );
        // An unset key falls through to None (caller uses its default).
        assert!(
            block_on(ws.config_get(Path::new("index.md"), "missing"))
                .unwrap()
                .is_none()
        );
        // No pointer at all → no config document.
        write(&dir, "bare.md", "---\ntitle: Bare\n---\n");
        assert!(
            block_on(ws.config_path(Path::new("bare.md")))
                .unwrap()
                .is_none()
        );
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
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- a.md\n---\n",
        );
        write(&dir, "a.md", "---\npart_of: index.md\n---\n");

        let mut w = id_ws(&dir);
        // Author a link-by-id: register, then write the id target into index.md.
        let id = block_on(w.register(Path::new("a.md"), Trigger::Link)).unwrap();
        let text = read(&dir, "index.md");
        let carrier = Document::parse("index.md", &text).unwrap().carrier;
        let updated = crate::edit::set_in_text(
            &text,
            carrier,
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
            index_text.contains(&format!("id:{id}")),
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
        let carrier = Document::parse("index.md", &text).unwrap().carrier;
        let updated = crate::edit::set_in_text(
            &text,
            carrier,
            "contents.0",
            fig::Value::Str(link::id_target(&id)),
        )
        .unwrap();
        std::fs::write(dir.join("index.md"), &updated).unwrap();

        block_on(w.delete(Path::new("a.md"), false)).unwrap();
        // Deleting removed the parent's entry too (matched through the registry
        // before the tombstone landed)… so re-add a dangling reference by hand
        // to simulate the out-of-band case.
        let text = read(&dir, "index.md");
        let carrier = Document::parse("index.md", &text).unwrap().carrier;
        let updated = crate::edit::set_in_text(
            &text,
            carrier,
            "contents",
            fig::Value::Str(link::id_target(&id)),
        )
        .unwrap();
        std::fs::write(dir.join("index.md"), &updated).unwrap();

        assert!(w.index().resolve(&id).is_none(), "tombstoned");
        assert!(w.index().is_known(&id), "but never forgotten");
        let findings = block_on(w.check("index.md")).unwrap();
        assert!(
            findings.iter().any(|f| matches!(
                f,
                crate::validate::Finding::DanglingId {
                    tombstoned: true,
                    ..
                }
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
        let id = w
            .index()
            .id_for_path(Path::new("a.md"))
            .expect("registered at birth");
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

    // ---- transactional writes (see `crate::change`) ----
    //
    // The property under test is the crate's whole reason to exist: link
    // maintenance spans documents, so a mutation that half-lands is worse than
    // one that does not land at all. Each of these drives a real operation over a
    // backend that fails one write, and asserts the workspace is byte-for-byte as
    // it was found — not merely "check-clean", which a torn-but-detectable state
    // would also be.

    /// The whole workspace as `(relative path, contents)`, sorted — so a test can
    /// assert nothing anywhere changed, rather than spot-checking the files it
    /// happened to think of.
    fn snapshot(dir: &Path) -> Vec<(String, String)> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap())
                .collect();
            entries.sort_by_key(std::fs::DirEntry::path);
            for entry in entries {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    out.push((
                        path.strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                        std::fs::read_to_string(&path).unwrap_or_default(),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out
    }

    /// A workspace over a backend that fails the `fail_at`th write.
    fn failing_ws(dir: &Path, fail_at: usize) -> Workspace<FailAtWrite> {
        Workspace::builder(FailAtWrite::nth(fail_at))
            .root(dir)
            .build()
    }

    fn linked_tree(tag: &str) -> PathBuf {
        let dir = tempdir(tag);
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- a.md\n- b.md\n---\nbody\n",
        );
        write(
            &dir,
            "a.md",
            "---\ntitle: A\npart_of: index.md\n---\nsee [[b]]\n",
        );
        write(
            &dir,
            "b.md",
            "---\ntitle: B\npart_of: index.md\nlinks:\n- a.md\n---\nbody\n",
        );
        dir
    }

    #[test]
    fn moving_a_separated_node_refuses_to_overwrite_an_occupied_body_path() {
        // `rename` guards its own destination but not its *body's*, and a rename
        // that clobbers is the one thing a change set cannot undo: the overwritten
        // bytes are gone before any rollback could copy them. So the guard has to
        // be a refusal up front, alongside the check on the node's own path.
        let dir = tempdir("body-collision");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- notes.yaml\n---\n",
        );
        write(
            &dir,
            "notes.yaml",
            "title: Notes\npart_of: index.md\ncontent: notes.md\n",
        );
        write(&dir, "notes.md", "the prose\n");
        // An unrelated document already sitting where the body would land.
        write(&dir, "other.md", "PRECIOUS — must not be destroyed\n");

        let err = block_on(ws(&dir).rename(Path::new("notes.yaml"), Path::new("other.yaml")))
            .unwrap_err();
        assert!(
            err.to_string().contains("other.md"),
            "should name the blocker: {err}"
        );
        assert_eq!(
            read(&dir, "other.md"),
            "PRECIOUS — must not be destroyed\n",
            "the move destroyed an unrelated document"
        );
        assert!(
            dir.join("notes.yaml").exists(),
            "and the refused move changed nothing"
        );
    }

    #[test]
    fn a_failed_create_leaves_neither_the_child_nor_the_parents_entry() {
        // `create` writes the child then the parent. Fail the parent's write and
        // the child file must not survive: a document nothing contains is exactly
        // the orphan `check` cannot see (DESIGN §8).
        let dir = tempdir("atomic-create");
        write(&dir, "index.md", "---\ntitle: Root\n---\nbody\n");
        let before = snapshot(&dir);

        let mut w = failing_ws(&dir, 1);
        let err = block_on(w.create(Path::new("a.md"), Path::new("index.md"))).unwrap_err();
        assert!(err.to_string().contains("disk full"), "{err}");
        assert_eq!(
            snapshot(&dir),
            before,
            "a failed create left something behind"
        );
    }

    #[test]
    fn a_failed_rename_leaves_every_inbound_link_pointing_at_the_original() {
        // The op with the most writes and the most to lose: the file moves, then
        // the parent's entry, the sibling's overlay link and the body wikilink all
        // retarget. Fail each write in turn — the workspace must come back whole
        // every time, whichever one it was.
        //
        // The sweep is bounded by a probe run rather than a literal, so it keeps
        // covering every write the day `rename` grows one.
        let probe = tempdir("atomic-rename-probe");
        let _ = std::fs::remove_dir_all(&probe);
        let dir = linked_tree("atomic-rename-probe");
        let mut w = Workspace::builder(FailAtWrite::never()).root(&dir).build();
        block_on(w.rename(Path::new("a.md"), Path::new("sub/a.md"))).unwrap();
        let writes = w.fs().attempted();
        assert!(
            writes >= 3,
            "expected the move, the parent and the sibling: {writes}"
        );

        for fail_at in 0..writes {
            let dir = linked_tree("atomic-rename");
            let before = snapshot(&dir);

            let mut w = failing_ws(&dir, fail_at);
            let err = block_on(w.rename(Path::new("a.md"), Path::new("sub/a.md"))).unwrap_err();
            assert!(err.to_string().contains("disk full"), "{err}");
            assert_eq!(
                snapshot(&dir),
                before,
                "a rename that failed at write {fail_at} of {writes} left the workspace torn"
            );
        }
    }

    #[test]
    fn a_failed_reparent_leaves_the_old_containment_intact() {
        // Three documents change, and the middle window is the dangerous one: the
        // child repointed at its new parent while the old parent still claims it.
        let dir = tempdir("atomic-reparent");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- old.md\n- new.md\n---\n",
        );
        write(
            &dir,
            "old.md",
            "---\ntitle: Old\npart_of: index.md\ncontents:\n- kid.md\n---\n",
        );
        write(&dir, "new.md", "---\ntitle: New\npart_of: index.md\n---\n");
        write(&dir, "kid.md", "---\ntitle: Kid\npart_of: old.md\n---\n");
        let before = snapshot(&dir);

        // Write 0 repoints the kid, 1 adds the new parent's entry, 2 removes the
        // old parent's. Failing the last is the worst case — both the other two
        // have landed, and the kid is contained twice.
        let mut w = failing_ws(&dir, 2);
        let err = block_on(w.reparent(Path::new("kid.md"), Path::new("new.md"))).unwrap_err();
        assert!(err.to_string().contains("disk full"), "{err}");
        assert_eq!(
            snapshot(&dir),
            before,
            "a failed reparent left the kid contained twice"
        );
    }

    #[test]
    fn a_failed_delete_restores_the_document_it_removed() {
        let dir = linked_tree("atomic-delete");
        let before = snapshot(&dir);

        // `delete` removes the file, then rewrites the parent's entry. Failing
        // that write must bring the document back, not leave the parent pointing
        // at a hole.
        let mut w = failing_ws(&dir, 0);
        let err = block_on(w.delete(Path::new("a.md"), true)).unwrap_err();
        assert!(err.to_string().contains("disk full"), "{err}");
        assert_eq!(snapshot(&dir), before, "a failed delete lost a document");
    }

    #[test]
    fn consecutive_ops_keep_each_others_registrations() {
        // The other half of the checkpoint's lifetime, and the one that bites in
        // the ordinary case: a *successful* op must drop its checkpoint even when
        // it staged no registry write of its own — a host-less store (frontmatter
        // storage, or before a registry is bootstrapped), an `InMemoryIndex`, an
        // op that never dirtied the index. Otherwise the checkpoint outlives the
        // op that took it, and the next one cannot tell it from a leak.
        let dir = tempdir("consecutive-ops");
        write(&dir, "index.md", "---\ntitle: Root\n---\n");
        let mut w = Workspace::builder(StdFs)
            .root(&dir)
            .identity(Minter::eager(7))
            .id_links(true)
            // No host: `pending_write` stages nothing, so `commit` has no registry
            // write to report — exactly the case that leaves a checkpoint behind.
            .index(FileIndex::new(fig::Format::Yaml))
            .build();

        block_on(w.create(Path::new("a.md"), Path::new("index.md"))).unwrap();
        let a_id = w
            .index()
            .id_for_path(Path::new("a.md"))
            .expect("a.md registered");
        let root_id = w
            .index()
            .id_for_path(Path::new("index.md"))
            .expect("root registered");

        block_on(w.create(Path::new("b.md"), Path::new("index.md"))).unwrap();

        // The second create must not have unwound the first one's registrations —
        // the root's `contents` now links both by id, and both must resolve.
        assert_eq!(
            w.index().id_for_path(Path::new("a.md")),
            Some(a_id.clone()),
            "the first op's registration was erased by the second"
        );
        assert_eq!(
            w.index().id_for_path(Path::new("index.md")),
            Some(root_id),
            "the root was re-minted a second, different id"
        );
        assert!(
            w.index().id_for_path(Path::new("b.md")).is_some(),
            "b.md registered"
        );

        // The authored links must actually resolve — the user-visible failure.
        let root_text = read(&dir, "index.md");
        assert!(
            root_text.contains(a_id.as_str()),
            "the root links a.md by id: {root_text}"
        );
        assert_eq!(
            w.index().resolve(&a_id),
            Some(PathBuf::from("a.md")),
            "the id the root links must still resolve"
        );
    }

    #[test]
    fn a_registration_made_between_ops_survives_the_next_one() {
        // `change` unwinds an outstanding checkpoint, so it must be certain that a
        // checkpoint outstanding at that moment really is abandoned work. A
        // caller registering an ID *between* two ops — the public seam
        // `Workspace::register` — is not abandoned work, and erasing it would
        // dangle any link the caller authored from the id it was handed.
        let dir = tempdir("register-between-ops");
        write(&dir, "index.md", "---\ntitle: Root\n---\n");
        write(&dir, "a.md", "---\ntitle: A\npart_of: index.md\n---\n");
        let mut w = hosted_registry_ws(&dir, StdFs);

        // An op that succeeds without dirtying the index — the case that used to
        // leave a checkpoint outstanding.
        block_on(w.convert_link_style(Path::new("a.md"), LinkStyle::PlainRelative, false)).unwrap();

        let id = block_on(w.register(Path::new("a.md"), Trigger::Link)).unwrap();
        block_on(w.create(Path::new("c.md"), Path::new("index.md"))).unwrap();

        assert_eq!(
            w.index().resolve(&id),
            Some(PathBuf::from("a.md")),
            "a registration made between ops must not be erased by the next one"
        );
        assert!(
            read(&dir, "registry.yaml").contains("a.md"),
            "and the next op's commit should carry it to disk"
        );
    }

    #[test]
    fn a_dangling_checkpoint_is_unwound_by_the_next_op_not_inherited() {
        // The third window, after "the write failed" and "the rollback failed":
        // an op that returns early *between* `change` and `commit`, by `?` on an
        // edit it was still computing. Its writes never happened, but its
        // registrations did, and no commit ran to unwind them — `create` mints the
        // child's ID before authoring the parent's entry, so the leak would be a
        // registry record naming a document that was never written.
        //
        // Driven through the index protocol rather than a fixture, deliberately.
        // Reaching that `?` from the public API needs a metadata edit the editor
        // rejects *after* the mint, which the ops currently recover from or make
        // unreachable — so a fixture would either not fail at all (and quietly
        // stop testing this) or encode today's exact failure points as if they
        // were the contract. What is being asserted is `change`'s contract: a
        // checkpoint left outstanding is unwound, never inherited.
        let dir = tempdir("dangling-checkpoint");
        write(&dir, "index.md", "---\ntitle: Root\n---\n");
        let mut w = id_ws(&dir);

        // Exactly what an op that bailed after minting would leave behind.
        w.index_mut().checkpoint();
        let ghost = crate::identity::Id("ghostid".into());
        w.index_mut()
            .register(&ghost, Path::new("never-written.md"));
        assert!(w.index().is_dirty(), "the abandoned op dirtied the store");

        // The next op unwinds it rather than staging it into its own registry.
        block_on(w.create(Path::new("real.md"), Path::new("index.md"))).unwrap();
        assert_eq!(
            w.index().resolve(&ghost),
            None,
            "a document that was never created must not survive into the next op"
        );
    }

    // ---- the registry lands with the documents (DESIGN §5) ----

    #[test]
    fn moving_the_registry_document_does_not_resurrect_it_at_its_old_path() {
        // The registry document is a document — reached from the root, movable
        // like any other. But `commit` stages the registry's own write *last*, so
        // unless that write follows the move it lands at the old path — recreating
        // the file the op just renamed away from, with all the records in it, while
        // the file the root now points at has none.
        let dir = tempdir("move-registry");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\nregistry: registry.yaml\n---\n",
        );
        let mut w = hosted_registry_ws(&dir, StdFs);
        // Give the registry document an id of its own, so the move dirties the
        // store and forces the registry write into the same set as the rename.
        let id = block_on(w.register(Path::new("registry.yaml"), Trigger::Link)).unwrap();

        block_on(w.rename(Path::new("registry.yaml"), Path::new("meta/registry.yaml"))).unwrap();

        assert!(
            !dir.join("registry.yaml").exists(),
            "the registry was resurrected at the path it just moved away from"
        );
        let moved = read(&dir, "meta/registry.yaml");
        assert!(
            moved.contains(id.as_str()) && moved.contains("meta/registry.yaml"),
            "the moved registry must hold its records, repointed: {moved}"
        );
    }

    #[test]
    fn an_op_that_rewrites_the_registry_document_is_not_clobbered_by_its_own_records() {
        // The same hazard when a store *does* carry a link (machinery gets no
        // `part_of` by default, but a hand-added one — as here — must still be
        // maintained): moving the *root* re-relativizes the registry's `part_of`,
        // staging a write to the registry document. `commit` then stages its own
        // write to that document, rendered from the text it read at startup, and
        // last-write-wins would silently drop the re-relativized link.
        let dir = tempdir("rewrite-registry");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\nregistry: registry.yaml\ncontents:\n- registry.yaml\n---\n",
        );
        let mut w = hosted_registry_ws(&dir, StdFs);
        // The registry document points back at the root, in a path form a move
        // must recompute.
        write(
            &dir,
            "registry.yaml",
            "title: ID registry\npart_of: index.md\n",
        );
        let text = read(&dir, "registry.yaml");
        w.index_mut().set_host("registry.yaml", &text).unwrap();
        let root_id = block_on(w.register(Path::new("index.md"), Trigger::Link)).unwrap();

        block_on(w.rename(Path::new("index.md"), Path::new("docs/index.md"))).unwrap();

        let registry = read(&dir, "registry.yaml");
        assert!(
            registry.contains("part_of: docs/index.md"),
            "the registry document's own part_of must survive the root's move: {registry}"
        );
        assert!(
            registry.contains(root_id.as_str()),
            "and its records must be there too: {registry}"
        );
    }

    /// A workspace whose registry is a real document on disk, so its write is
    /// staged rather than left to the caller.
    ///
    /// Seeds the host only if it is not already there, so a test can rebuild the
    /// workspace over a directory mid-flight — the way a second CLI run picks up
    /// the registry the first one left — instead of wiping it.
    fn hosted_registry_ws<FS: Storage>(dir: &Path, fs: FS) -> Workspace<FS, Minter, FileIndex> {
        let host = "registry.yaml";
        if !dir.join(host).exists() {
            write(dir, host, "title: ID registry\n");
        }
        let text = std::fs::read_to_string(dir.join(host)).unwrap();
        Workspace::builder(fs)
            .root(dir)
            .identity(Minter::eager(7))
            .index(FileIndex::parse(Path::new(host), &text).unwrap())
            .build()
    }

    #[test]
    fn a_rename_lands_its_registry_update_in_the_same_change_set() {
        // The positive half: after a successful move the registry on disk already
        // names the new path. Nothing else had to write it — no post-hoc save
        // step, which is the window this closes.
        let dir = tempdir("registry-with-docs");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\ncontents:\n- a.md\n---\n",
        );
        write(&dir, "a.md", "---\ntitle: A\npart_of: index.md\n---\n");
        let mut w = hosted_registry_ws(&dir, StdFs);

        let id = block_on(w.register(Path::new("a.md"), Trigger::Link)).unwrap();
        block_on(w.rename(Path::new("a.md"), Path::new("moved.md"))).unwrap();

        let registry = read(&dir, "registry.yaml");
        assert!(
            registry.contains("moved.md") && !registry.contains(" a.md"),
            "the registry on disk should already name the new path: {registry}"
        );
        assert!(
            registry.contains(id.as_str()),
            "the id should be recorded: {registry}"
        );
        assert!(
            !w.index().is_dirty(),
            "a staged registry write leaves the store clean"
        );
    }

    #[test]
    fn a_failed_rename_does_not_leave_the_registry_ahead_of_the_documents() {
        // The tear this exists to prevent, and the one the documents cannot
        // self-heal from: the registry is authoritative, not derived, so an
        // `id → path` that moved while the documents did not would resolve every
        // `colophon:<id>` reference to a file that is not there.
        //
        // Swept across every write the op makes rather than aimed at one, because
        // the interesting failure is the *last* — the registry's own write, with
        // every document already on disk behind it. Fixing a single index here
        // would silently stop testing that the day an op grows a write.
        let seed = |tag: &str| {
            let dir = tempdir(tag);
            write(
                &dir,
                "index.md",
                "---\ntitle: Root\ncontents:\n- a.md\n---\n",
            );
            write(&dir, "a.md", "---\ntitle: A\npart_of: index.md\n---\n");
            dir
        };

        // Probe: how many writes does the move make, registry included?
        let dir = seed("registry-rollback-probe");
        let mut w = hosted_registry_ws(&dir, FailAtWrite::never());
        block_on(w.register(Path::new("a.md"), Trigger::Link)).unwrap();
        let before_move = w.fs().attempted();
        block_on(w.rename(Path::new("a.md"), Path::new("moved.md"))).unwrap();
        let writes = w.fs().attempted() - before_move;
        assert!(
            read(&dir, "registry.yaml").contains("moved.md"),
            "the probe should have staged the registry write — otherwise this \
             test's premise is gone"
        );

        for fail_at in 0..writes {
            let dir = seed("registry-rollback");

            // Register and let it land, so the sweep isolates the *move*.
            let mut w = hosted_registry_ws(&dir, StdFs);
            let id = block_on(w.register(Path::new("a.md"), Trigger::Link)).unwrap();
            block_on(w.create(Path::new("settle.md"), Path::new("index.md"))).unwrap();
            assert!(
                read(&dir, "registry.yaml").contains("a.md"),
                "registry seeded"
            );
            let before = snapshot(&dir);

            // Rebuild over a backend that fails this run's `fail_at`th write,
            // carrying the same on-disk registry — as a second CLI run would.
            let mut w = hosted_registry_ws(&dir, FailAtWrite::nth(fail_at));
            let id_again = block_on(w.register(Path::new("a.md"), Trigger::Link)).unwrap();
            assert_eq!(id, id_again, "the same document keeps its id across runs");

            let err = block_on(w.rename(Path::new("a.md"), Path::new("moved.md"))).unwrap_err();
            assert!(err.to_string().contains("disk full"), "{err}");

            // On disk: nothing moved — least of all the registry.
            assert_eq!(
                snapshot(&dir),
                before,
                "a rename that failed at write {fail_at} of {writes} left the workspace torn"
            );
            // In memory: the store was rolled back too, so a caller holding this
            // workspace does not go on believing the move happened.
            assert_eq!(
                w.index().resolve(&id),
                Some(PathBuf::from("a.md")),
                "the in-memory registry should have rolled back with the writes \
                 (failed at write {fail_at} of {writes})"
            );
        }
    }

    // ---- recycle bin (Part 3) ----

    #[test]
    fn recycle_moves_a_document_into_the_bin_and_records_it() {
        let dir = tempdir("recycle-basic");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- note.md\n---\n",
        );
        let original = "---\ntitle: My Note\npart_of: index.md\n---\nbody text\n";
        write(&dir, "note.md", original);

        let danglers =
            block_on(ws(&dir).recycle(Path::new("note.md"), false, Some("2026-07-16T10:00:00Z")))
                .unwrap();
        assert!(danglers.is_empty(), "{danglers:?}");

        // The document is gone from its path but not destroyed — its bytes are
        // moved verbatim into the bin, under the (unreached) items directory.
        assert!(!dir.join("note.md").exists());
        assert_eq!(read(&dir, "recyclebin/items/note.md"), original);

        // The parent no longer links it, and the root now links the bin.
        let index = read(&dir, "index.md");
        assert!(
            !index.contains("- note.md"),
            "parent entry removed: {index}"
        );
        assert!(index.contains("recycle_bin"), "root links the bin: {index}");

        // The bin index records the deletion: title, origin, and timestamp.
        let bin = read(&dir, "recyclebin/index.yaml");
        assert!(bin.contains("My Note"), "records the title: {bin}");
        assert!(bin.contains("note.md"), "records the origin: {bin}");
        assert!(bin.contains("2026-07-16T10:00:00Z"), "records when: {bin}");

        // And the workspace is still consistent — the binned doc is *not* an orphan.
        let findings = block_on(ws(&dir).check(Path::new("index.md"))).unwrap();
        assert!(
            findings.is_empty(),
            "a recycle should leave check clean: {findings:?}"
        );
    }

    #[test]
    fn recycle_then_restore_is_lossless() {
        // The round-trip is the whole promise: delete and restore return the
        // workspace to byte-identical state, parent link and all.
        let dir = tempdir("recycle-restore");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- note.md\n---\n",
        );
        let original = "---\ntitle: My Note\npart_of: index.md\n---\nbody text\n";
        write(&dir, "note.md", original);

        block_on(ws(&dir).recycle(Path::new("note.md"), false, None)).unwrap();
        assert!(!dir.join("note.md").exists());

        block_on(ws(&dir).restore(Path::new("note.md"), Path::new("index.md"))).unwrap();

        // The document is back, byte-for-byte.
        assert_eq!(read(&dir, "note.md"), original);
        // The parent links it again, and its record is gone from the bin.
        let index = read(&dir, "index.md");
        assert!(
            index.contains("note.md"),
            "parent re-links the restored doc: {index}"
        );
        let bin = read(&dir, "recyclebin/index.yaml");
        assert!(
            !bin.contains("My Note"),
            "the record is cleared on restore: {bin}"
        );
        assert!(
            !dir.join("recyclebin/items/note.md").exists(),
            "the binned bytes moved back"
        );
        // Consistent.
        let findings = block_on(ws(&dir).check(Path::new("index.md"))).unwrap();
        assert!(
            findings.is_empty(),
            "a restore should leave check clean: {findings:?}"
        );
    }

    #[test]
    fn recycle_refuses_a_parent_with_children_unless_forced() {
        // Parity with `delete`: a document that contains others cannot be binned
        // without `force`, since binning it would strand them.
        let dir = tempdir("recycle-children");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n---\n");
        write(
            &dir,
            "a.md",
            "---\ntitle: A\npart_of: index.md\ncontents:\n- b.md\n---\n",
        );
        write(&dir, "b.md", "---\ntitle: B\npart_of: a.md\n---\n");

        let err = block_on(ws(&dir).recycle(Path::new("a.md"), false, None)).unwrap_err();
        assert!(err.to_string().contains("contains 1 document"), "{err}");
        assert!(
            dir.join("a.md").exists(),
            "a refused recycle changes nothing"
        );

        block_on(ws(&dir).recycle(Path::new("a.md"), true, None)).unwrap();
        assert!(!dir.join("a.md").exists());
        assert!(dir.join("recyclebin/items/a.md").exists());
    }

    #[test]
    fn a_second_deletion_appends_to_the_existing_bin() {
        // The bin is bootstrapped once; a later deletion appends to it, and the
        // root's pointer is authored a single time.
        let dir = tempdir("recycle-append");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n- b.md\n---\n");
        write(&dir, "a.md", "---\ntitle: Aye\npart_of: index.md\n---\n");
        write(&dir, "b.md", "---\ntitle: Bee\npart_of: index.md\n---\n");

        block_on(ws(&dir).recycle(Path::new("a.md"), false, None)).unwrap();
        block_on(ws(&dir).recycle(Path::new("b.md"), false, None)).unwrap();

        let bin = read(&dir, "recyclebin/index.yaml");
        assert!(
            bin.contains("Aye") && bin.contains("Bee"),
            "both recorded: {bin}"
        );
        assert!(dir.join("recyclebin/items/a.md").exists());
        assert!(dir.join("recyclebin/items/b.md").exists());

        let index = read(&dir, "index.md");
        assert_eq!(
            index.matches("recycle_bin").count(),
            1,
            "pointer authored once: {index}"
        );

        let findings = block_on(ws(&dir).check(Path::new("index.md"))).unwrap();
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn empty_bin_purges_the_bytes_and_clears_the_index_but_keeps_the_member() {
        let dir = tempdir("recycle-empty");
        write(&dir, "index.md", "---\ncontents:\n- a.md\n---\n");
        write(&dir, "a.md", "---\ntitle: Aye\npart_of: index.md\n---\n");

        block_on(ws(&dir).recycle(Path::new("a.md"), false, None)).unwrap();
        assert!(dir.join("recyclebin/items/a.md").exists());

        let purged = block_on(ws(&dir).empty_bin(Path::new("index.md"))).unwrap();
        assert_eq!(purged, 1);
        assert!(!dir.join("recyclebin/items/a.md").exists(), "bytes purged");

        let bin = read(&dir, "recyclebin/index.yaml");
        assert!(!bin.contains("Aye"), "records cleared: {bin}");
        // The bin member itself survives, still linked and consistent.
        assert!(read(&dir, "index.md").contains("recycle_bin"));
        let findings = block_on(ws(&dir).check(Path::new("index.md"))).unwrap();
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_failed_recycle_leaves_the_workspace_untouched() {
        // The whole move is one journaled ChangeSet, so an I/O failure part-way
        // rolls back to exactly the starting state — nothing half-binned.
        let dir = tempdir("recycle-atomic");
        write(&dir, "index.md", "---\ncontents:\n- note.md\n---\n");
        write(
            &dir,
            "note.md",
            "---\ntitle: Note\npart_of: index.md\n---\nbody\n",
        );
        let before = snapshot(&dir);

        let mut w = Workspace::builder(FailAtWrite::nth(0)).root(&dir).build();
        let err = block_on(w.recycle(Path::new("note.md"), false, None)).unwrap_err();
        assert!(err.to_string().contains("disk full"), "{err}");

        assert_eq!(
            snapshot(&dir),
            before,
            "a failed recycle tore the workspace"
        );
    }
}
