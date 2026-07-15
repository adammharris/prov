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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use fig::Segment;

use crate::document::{Document, MetaCarrier, whole_file_format};
use crate::edit::MetaEditor;
use crate::error::{Error, Result};
use crate::fs::Storage;
use crate::identity::{IdentityPolicy, Trigger};
use crate::index::IndexStore;
use crate::link::{self, Link};
use crate::meta::Value;
use crate::validate::{Finding, LinkSite, Resolution};
use crate::workspace::{Target, Workspace};

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
    /// the title-primary authoring entry point (`colophon new "My Great Note"`):
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
                return Err(Error::Structure(format!(
                    "{} already exists",
                    existing.display()
                )));
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

        if let Some(dir) = self.root().join(&node).parent() {
            self.fs().create_dir_all(dir).await?;
        }
        self.fs()
            .write(&self.root().join(&node), new_text.as_bytes())
            .await?;
        // A separated child's prose file starts empty (like a combined child's
        // body, which is just the synthesized block with nothing after it).
        if let Some(body_path) = &body {
            self.fs().write(&self.root().join(body_path), b"").await?;
        }
        self.fs()
            .write(&self.root().join(&parent), parent_out.as_bytes())
            .await?;

        // Identity hook — eager policies assign an ID from birth (idempotent: an
        // id-linked child was already registered above).
        if self.identity().registration().fires_on(Trigger::Create)
            && self.index().id_for_path(&node).is_none()
        {
            let id = self.mint_unique(&node);
            self.index_mut().register(&id, &node);
        }
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
                return Err(Error::Structure(format!(
                    "{} does not exist",
                    existing.display()
                )));
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
            self.fs()
                .write(&self.root().join(&child), updated.as_bytes())
                .await?;
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
            self.fs()
                .write(
                    &self.root().join(&parent),
                    parent_editor.render()?.as_bytes(),
                )
                .await?;
        }
        Ok(())
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
    /// ## Failure leaves a *detectable* state, not an atomic one
    ///
    /// Three documents change, and colophon has no journal — so a crash between
    /// writes cannot be prevented, only made loud. The writes are therefore
    /// ordered so that every window is a finding `check` already reports:
    /// repointing the child first leaves the old parent claiming a child that
    /// does not claim it back ([`Finding::MissingInverse`]); adding the new entry
    /// before removing the old leaves the child contained twice
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
                return Err(Error::Structure(format!(
                    "{} does not exist",
                    existing.display()
                )));
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
        self.fs()
            .write(&self.root().join(&child), updated.as_bytes())
            .await?;

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
            self.fs()
                .write(&self.root().join(&parent), editor.render()?.as_bytes())
                .await?;
        }

        // 3. The old parent's entry, removed last (see the ordering note above).
        // Re-read it here: when the old parent *is* the new parent's neighbour in
        // some earlier write, the text on disk is what must be edited, not the
        // copy loaded before step 2.
        if let Some(old) = &old_parent
            && old != &parent
        {
            let (old_text, old_doc) = self.load(old).await?;
            if let (Some(index), Some(carrier)) = (
                self.entry_index(&old_doc, &spanning, old, &child),
                old_doc.carrier,
            ) {
                let mut editor = MetaEditor::open(&old_text, carrier)?;
                editor.remove_item(&[Segment::Key(&spanning)], index)?;
                self.fs()
                    .write(&self.root().join(old), editor.render()?.as_bytes())
                    .await?;
            }
        }
        Ok(())
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
            return Err(Error::Structure(format!(
                "{} does not exist",
                from.display()
            )));
        }
        if self.fs().try_exists(&self.root().join(&to)).await? {
            return Err(Error::Structure(format!("{} already exists", to.display())));
        }
        let (from_text, from_doc) = self.load(&from).await?;

        // 1. Inbound references: every document that links *to* `from` by a
        //    path, retargeted to `to` (parent's spanning entry, children's
        //    inverses, overlay `links`, body wikilinks). Id-form links resolve
        //    through the registry and are never rewritten.
        let inbound_writes = self.collect_inbound_rewrites(&from, &to).await?;

        // A separated document's prose lives in a sibling body file; move it
        // alongside (and keep the `content` pointer correct) so the pair travels
        // together.
        let body_move = self.plan_body_move(&from_doc, &from, &to).await?;

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

        // All edits computed; now write.
        if let Some(dir) = self.root().join(&to).parent() {
            self.fs().create_dir_all(dir).await?;
        }
        self.fs()
            .rename(&self.root().join(&from), &self.root().join(&to))
            .await?;
        self.fs()
            .write(&self.root().join(&to), self_text.as_bytes())
            .await?;
        if let Some(mv) = &body_move {
            self.fs()
                .rename(&self.root().join(&mv.from), &self.root().join(&mv.to))
                .await?;
            // A prose body is rewritten with its re-relativized text; an opaque
            // payload (`text` is `None`) is left exactly as the rename moved it.
            if let Some(text) = &mv.text {
                self.fs()
                    .write(&self.root().join(&mv.to), text.as_bytes())
                    .await?;
            }
        }
        for (source, text) in inbound_writes {
            self.fs()
                .write(&self.root().join(&source), text.as_bytes())
                .await?;
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

        self.fs().remove_file(&self.root().join(&path)).await?;
        if let Some(body) = body_file
            && self.fs().try_exists(&self.root().join(&body)).await?
        {
            self.fs().remove_file(&self.root().join(&body)).await?;
        }
        if let Some((parent, text)) = parent_write {
            self.fs()
                .write(&self.root().join(&parent), text.as_bytes())
                .await?;
        }

        // Identity hook — retire the ID (a tombstoning store keeps it known
        // forever, so it is never minted again to mean something else).
        if let Some(id) = self.index().id_for_path(&path) {
            self.index_mut().unregister(&id);
        }
        Ok(danglers)
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
            return Err(Error::Structure(format!(
                "{} does not exist",
                path.display()
            )));
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
            return Err(Error::Structure(format!(
                "{} already exists",
                meta_path.display()
            )));
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

        // Inbound links now point at the metadata file (the structural node).
        let inbound = self.collect_inbound_rewrites(&path, &meta_path).await?;

        self.fs()
            .write(&self.root().join(&meta_path), meta_text.as_bytes())
            .await?;
        self.fs()
            .write(&self.root().join(&path), body_text.as_bytes())
            .await?;
        for (source, text) in inbound {
            self.fs()
                .write(&self.root().join(&source), text.as_bytes())
                .await?;
        }
        if let Some(id) = self.index().id_for_path(&path) {
            self.index_mut().set_path(&id, &meta_path);
        }
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

        // Inbound links point back at the (now combined) content file.
        let inbound = self.collect_inbound_rewrites(&path, &content).await?;

        self.fs()
            .write(&self.root().join(&content), combined.as_bytes())
            .await?;
        self.fs().remove_file(&self.root().join(&path)).await?;
        for (source, text) in inbound {
            self.fs()
                .write(&self.root().join(&source), text.as_bytes())
                .await?;
        }
        if let Some(id) = self.index().id_for_path(&path) {
            self.index_mut().set_path(&id, &content);
        }
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
    ///   `source`); colophon's link-shaped containment — unlike diaryx's
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
            return Err(Error::Structure(format!(
                "{} does not exist",
                source.display()
            )));
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

        // All edits computed; now write. Node first, then its body copy (opaque
        // payload bytes carried verbatim), then the parent's updated entry.
        if let Some(dir) = self.root().join(&dest).parent() {
            self.fs().create_dir_all(dir).await?;
        }
        self.fs()
            .write(&self.root().join(&dest), copy_text.as_bytes())
            .await?;
        if let (Some(body_from), Some((body_to, _))) = (&body_from, &body_dest) {
            let bytes = self.fs().read(&self.root().join(body_from)).await?;
            self.fs().write(&self.root().join(body_to), &bytes).await?;
        }
        if let Some((parent, text)) = parent_write {
            self.fs()
                .write(&self.root().join(&parent), text.as_bytes())
                .await?;
        }

        // Identity hook — an eager policy assigns the copy an ID from birth
        // (idempotent: the down-link above already registered it under `id_links`).
        if self.identity().registration().fires_on(Trigger::Create)
            && self.index().id_for_path(&dest).is_none()
        {
            let id = self.mint_unique(&dest);
            self.index_mut().register(&id, &dest);
        }
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
    /// to every document in `file`'s spanning subtree. Returns the number of
    /// documents actually rewritten.
    pub async fn convert_link_style(
        &mut self,
        file: &Path,
        style: crate::link::LinkStyle,
        recursive: bool,
    ) -> Result<usize> {
        let file = link::normalize(file);
        if !self.fs().try_exists(&self.root().join(&file)).await? {
            return Err(Error::Structure(format!(
                "{} does not exist",
                file.display()
            )));
        }
        let targets = if recursive {
            self.spanning_subtree(&file).await?
        } else {
            vec![file]
        };
        let mut changed = 0;
        for path in targets {
            if self.restyle_document(&path, style).await? {
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Restyle every path link the document at `path` declares — frontmatter
    /// relation entries then body links — writing the file only when something
    /// changed. Returns whether it did. The body is spliced against `doc.body`
    /// (verbatim, MetaEditor-preserved) after the frontmatter edit, the same
    /// two-step `rename` uses.
    async fn restyle_document(&self, path: &Path, style: crate::link::LinkStyle) -> Result<bool> {
        let (text, doc) = self.load(path).await?;
        let meta_rewritten =
            restyle_frontmatter_links(&text, &doc, self.relations().relations(), path, style)?;
        let final_text = restyle_body_links(&meta_rewritten, &doc.body, path, style);
        if final_text != text {
            self.fs()
                .write(&self.root().join(path), final_text.as_bytes())
                .await?;
            Ok(true)
        } else {
            Ok(false)
        }
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
    use crate::fs::StdFs;
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
        let dir =
            std::env::temp_dir().join(format!("colophon-mutate-{tag}-{}", std::process::id()));
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
        assert_eq!(n, 1, "only the one file converted");

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
        assert_eq!(n, 3, "root + a + b all converted");

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

    #[test]
    fn config_pointer_resolves_and_reads_a_setting() {
        // Workspace policy lives in a config document the root links via the
        // `config` relation — the registry's reachability move, for config.
        let dir = tempdir("config");
        write(
            &dir,
            "index.md",
            "---\ntitle: Root\nconfig: colophon.yaml\n---\n",
        );
        write(
            &dir,
            "colophon.yaml",
            "title: colophon config\npart_of: index.md\nlink_format: plain_relative\n",
        );
        let ws = ws(&dir);
        assert_eq!(
            block_on(ws.config_path(Path::new("index.md"))).unwrap(),
            Some(PathBuf::from("colophon.yaml"))
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
}
