//! Attachments — giving an *arbitrary* file its own workspace-linked metadata.
//!
//! A colophon document carries its structure in embedded frontmatter, but a
//! binary — an image, a PDF, a font — cannot. The fix reuses the **separated**
//! document shape (`EmbedStyle::Separate`): a whole-file metadata *sidecar*
//! joined to a body file by a `content` attribute. An attachment is that same
//! pattern with the body relaxed from prose to bytes — the sidecar
//! `photo.jpg.yaml` holds `title`/`id`/relations and points `content` at
//! `photo.jpg`, which colophon links, moves, and validates but never *reads*.
//!
//! This is the sidecar colophon's philosophy welcomes, not the one it rejects:
//! a co-located, visible, self-describing document any tool can open — the exact
//! opposite of an app-private `.obsidian/`-style folder (`lib.rs`).
//!
//! Three operations:
//! - [`attach`](Workspace::attach) — mint a sidecar for a loose file and link it
//!   under a parent (the attachment analogue of [`create`](Workspace::create)).
//! - [`attachment_for`](Workspace::attachment_for) — the reverse lookup: given a
//!   payload, find its sidecar by the `<file>.<ext>` convention, confirmed by the
//!   authoritative `content` pointer.
//! - [`loose_attachments`](Workspace::loose_attachments) — every opaque file with
//!   no sidecar yet, the work-list an importer walks.
//!
//! Move and delete need no new code: a sidecar is a separated node, so
//! [`rename`](Workspace::rename) already relocates the payload beside it (keeping
//! `content` correct) and [`delete`](Workspace::delete) removes the pair.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use fig::Segment;

use crate::document::{is_opaque_payload, whole_file_extension};
use crate::edit::MetaEditor;
use crate::error::{Error, Result};
use crate::fs::Storage;
use crate::identity::{IdentityPolicy, Trigger};
use crate::index::IndexStore;
use crate::link;
use crate::meta::{Mapping, Value};
use crate::workspace::Workspace;

/// The whole-file metadata extensions a sidecar can use, in reverse-lookup
/// preference order. The `<payload>.<ext>` naming convention (`photo.jpg` →
/// `photo.jpg.yaml`) keeps the full payload name, so `a.png` and `a.txt` get
/// distinct sidecars instead of colliding on `a.yaml`. An extension whose format
/// feature is not compiled simply never matches (its sidecar fails to parse as a
/// whole-file document), so the list is safe to keep static.
const SIDECAR_EXTENSIONS: &[&str] = &["yaml", "yml", "json", "toml", "fig", "figl"];

/// The sidecar path for `payload` in metadata `format`: the payload's full name
/// plus the format's whole-file extension, as a sibling (`sub/a.pdf` →
/// `sub/a.pdf.yaml`), so the sidecar's `content` pointer is just the basename.
fn sidecar_path(payload: &Path, format: fig::Format) -> PathBuf {
    let ext = whole_file_extension(format);
    let name = payload.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    payload.with_file_name(format!("{name}.{ext}"))
}

impl<FS: Storage, Id, Ix: IndexStore> Workspace<FS, Id, Ix> {
    /// The metadata sidecar for the attachment `payload`, or `None` when it has
    /// none. Probes the `<payload>.<ext>` convention for each whole-file metadata
    /// extension and confirms the candidate's `content` actually resolves back to
    /// `payload` — the convention is the fast path, the `content` pointer is
    /// authoritative, so a sidecar under a non-conventional name is still found by
    /// [`loose_attachments`] treating the payload as unattached only when no
    /// pointer claims it. (Here we accept the convention's hits; a bespoke layout
    /// is the caller's to track.)
    pub async fn attachment_for(&self, payload: &Path) -> Result<Option<PathBuf>> {
        let payload = link::normalize(payload);
        let Some(name) = payload.file_name().and_then(|n| n.to_str()) else {
            return Ok(None);
        };
        for ext in SIDECAR_EXTENSIONS {
            let candidate = payload.with_file_name(format!("{name}.{ext}"));
            if !self.fs().try_exists(&self.root().join(&candidate)).await? {
                continue;
            }
            let (_, doc) = self.load(&candidate).await?;
            if let Some(content) = doc.content_attr() {
                let dir = candidate.parent().unwrap_or(Path::new(""));
                if link::normalize(dir.join(content)) == payload {
                    return Ok(Some(candidate));
                }
            }
        }
        Ok(None)
    }

    /// Every opaque file under the root that has no sidecar yet — the *recursive*
    /// population, the whole tree. A flat filesystem scan (hidden entries
    /// skipped), independent of link resolution, like the title/id/content scans
    /// beside it. Sidecars and prose documents are text colophon reads, so they
    /// are not payloads and never appear here.
    ///
    /// This is the `--recursive` escape hatch for `attach --all`; the bounded
    /// [`loose_attachments_in`](Self::loose_attachments_in) is the safer default.
    pub async fn loose_attachments(&self) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        self.scan_loose(PathBuf::new(), &mut found).await?;
        found.sort();
        Ok(found)
    }

    /// Loose opaque files (no sidecar yet) in the directories the workspace
    /// already reaches from `start` — **reachability-bounded** discovery, the
    /// default for `attach --all`. Unreached directories are never scanned, so
    /// `attach --all` in a project root sweeps only the folders the workspace
    /// occupies, not a vendored subtree or a nested colophon workspace. The
    /// counterpart to the bounded orphan check (DESIGN §8).
    pub async fn loose_attachments_in(&self, start: &Path) -> Result<Vec<PathBuf>> {
        // The reachable set: `start` plus every path a census link resolves to.
        let mut reachable: BTreeSet<PathBuf> = BTreeSet::new();
        reachable.insert(link::normalize(start));
        for entry in self.census(start).await? {
            if let Some(p) = entry.resolution.resolved_path() {
                reachable.insert(p.clone());
            }
        }
        let reached_dirs = Self::reached_dirs(&reachable);
        let mut found = Vec::new();
        for file in self.direct_child_files(&reached_dirs).await? {
            if is_opaque_payload(&file) && self.attachment_for(&file).await?.is_none() {
                found.push(file);
            }
        }
        found.sort();
        Ok(found)
    }

    /// Recursively collect opaque files lacking a sidecar under `rel_dir`. Same
    /// walk shape as the content/id scans; unreadable and hidden entries skipped.
    fn scan_loose<'a>(
        &'a self,
        rel_dir: PathBuf,
        out: &'a mut Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        Box::pin(async move {
            let Ok(entries) = self.fs().read_dir(&self.root().join(&rel_dir)).await else {
                return Ok(());
            };
            for entry in entries {
                let Some(name) = entry.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                    continue;
                };
                if name.starts_with('.') {
                    continue;
                }
                let rel = if rel_dir.as_os_str().is_empty() {
                    PathBuf::from(&name)
                } else {
                    rel_dir.join(&name)
                };
                if entry.file_type().is_dir() {
                    self.scan_loose(rel, out).await?;
                } else if entry.file_type().is_file()
                    && is_opaque_payload(&rel)
                    && self.attachment_for(&rel).await?.is_none()
                {
                    out.push(rel);
                }
            }
            Ok(())
        })
    }
}

impl<FS: Storage, IdP: IdentityPolicy, Ix: IndexStore> Workspace<FS, IdP, Ix> {
    /// Attach the opaque file `payload` as a spanning child of `parent`: mint a
    /// whole-file metadata **sidecar** beside it (`photo.jpg` → `photo.jpg.yaml`)
    /// carrying `title`, the inverse link back to `parent`, a `content` pointer at
    /// the payload, and an `attachment: true` marker; and add the sidecar (never
    /// the payload) to `parent`'s spanning field. If the identity policy registers
    /// on create, the sidecar is assigned a stable ID. Returns the sidecar's path.
    ///
    /// The payload is the structural analogue of a separated document's prose
    /// body, so it is *not* rewritten, read, or required to be text — only its
    /// existence is checked. Refuses a payload colophon can read as a document
    /// (that is [`adopt`](Workspace::adopt)'s job: it can hold its own
    /// frontmatter) and refuses when a sidecar already exists (query
    /// [`attachment_for`](Workspace::attachment_for) first for idempotency).
    pub async fn attach(&mut self, payload: &Path, parent: &Path) -> Result<PathBuf> {
        self.attach_titled(payload, parent, None).await
    }

    /// [`attach`](Self::attach) with an explicit sidecar title (else the payload's
    /// titleized stem). Authoring the title here keeps the parent's spanning-entry
    /// *label* in step with it, exactly as [`create_titled`](Self::create_titled).
    pub(crate) async fn attach_titled(
        &mut self,
        payload: &Path,
        parent: &Path,
        title_override: Option<&str>,
    ) -> Result<PathBuf> {
        let payload = link::normalize(payload);
        let parent = link::normalize(parent);

        if !self.fs().try_exists(&self.root().join(&payload)).await? {
            return Err(Error::Structure(format!("{} does not exist", payload.display())));
        }
        // An attachment shadows *external* content. A file colophon can read is a
        // document that should carry its own metadata — adopt it, don't sidecar it.
        if !is_opaque_payload(&payload) {
            return Err(Error::Structure(format!(
                "{} is a colophon document, not an opaque attachment — use `adopt`",
                payload.display()
            )));
        }

        let (spanning, inverse) = self.spanning_pair()?;
        let format = self.default_embed_format();
        let node = sidecar_path(&payload, format);
        if self.fs().try_exists(&self.root().join(&node)).await? {
            return Err(Error::Structure(format!("{} already exists", node.display())));
        }

        let (parent_text, parent_doc) = self.load(&parent).await?;
        let title = title_override
            .map(str::to_owned)
            .unwrap_or_else(|| link::path_to_title(&payload));
        let parent_title = parent_doc
            .meta
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| link::path_to_title(&parent));

        // The sidecar's inverse link up (the parent exists → an id link registers
        // it by path) and the parent's spanning entry down (the sidecar is not on
        // disk yet → mint its id directly rather than register-by-path).
        let up = self.authored_target(&inverse, &node, &parent, &parent_title, true).await?;
        let down = self.authored_target(&spanning, &parent, &node, &title, false).await?;

        // The sidecar: a whole-file mapping pointing `content` at the payload
        // (a sibling, so just its name) and flagged as an attachment.
        let payload_ref = payload
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let mut map = Mapping::new();
        map.insert("title".into(), Value::String(title));
        map.insert(inverse.clone(), Value::String(up));
        map.insert("content".into(), Value::String(payload_ref));
        map.insert("attachment".into(), Value::Bool(true));
        let node_text = crate::meta::serialize_mapping(&map, format)?;

        // The parent: append the sidecar to its spanning field (creating it if
        // absent — `append` needs an existing sequence).
        let mut parent_editor = MetaEditor::open_or_init(&parent_text, parent_doc.carrier)?;
        let span_path = [Segment::Key(&spanning)];
        if parent_editor.append_value(&span_path, fig::Value::Str(down.clone())).is_err() {
            parent_editor.set_value(&span_path, fig::Value::Seq(vec![fig::Value::Str(down)]))?;
        }
        let parent_out = parent_editor.render()?;

        if let Some(dir) = self.root().join(&node).parent() {
            self.fs().create_dir_all(dir).await?;
        }
        self.fs().write(&self.root().join(&node), node_text.as_bytes()).await?;
        self.fs().write(&self.root().join(&parent), parent_out.as_bytes()).await?;

        // Identity hook — eager policies assign an ID from birth (idempotent: an
        // id-linked sidecar was already registered above).
        if self.identity().registration().fires_on(Trigger::Create)
            && self.index().id_for_path(&node).is_none()
        {
            let id = self.mint_unique(&node);
            self.index_mut().register(&id, &node);
        }
        Ok(node)
    }
}

#[cfg(all(test, feature = "yaml"))]
mod tests {
    use super::*;
    use crate::exec::block_on;
    use crate::fs::StdFs;

    fn write(dir: &Path, rel: &str, bytes: &[u8]) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> String {
        std::fs::read_to_string(dir.join(rel)).unwrap()
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("colophon-attach-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ws(dir: &Path) -> Workspace<StdFs> {
        Workspace::builder(StdFs).root(dir).build()
    }

    #[test]
    fn attach_gives_a_binary_a_linked_metadata_sidecar() {
        let dir = tempdir("basic");
        write(&dir, "index.md", b"---\ntitle: Home\n---\n");
        // A binary payload: bytes colophon must never try to read as text.
        write(&dir, "photo.jpg", &[0xff, 0xd8, 0xff, 0xe0, 0x00]);

        let node =
            block_on(ws(&dir).attach(Path::new("photo.jpg"), Path::new("index.md"))).unwrap();
        // The sidecar keeps the full payload name (no `a.jpg`/`a.png` collision).
        assert_eq!(node, PathBuf::from("photo.jpg.yaml"));

        let sidecar = read(&dir, "photo.jpg.yaml");
        assert!(sidecar.contains("title: Photo"), "{sidecar}");
        assert!(sidecar.contains("content: photo.jpg"), "points at the payload: {sidecar}");
        assert!(sidecar.contains("attachment: true"), "flagged as an attachment: {sidecar}");
        assert!(sidecar.contains("index.md"), "inverse link up to the parent: {sidecar}");

        // The parent links the sidecar (the node), never the raw payload.
        let index = read(&dir, "index.md");
        assert!(index.contains("photo.jpg.yaml"), "{index}");
        assert!(!index.contains("[photo.jpg]") && !index.contains("(photo.jpg)"), "{index}");

        // The payload is untouched, and the whole workspace validates — the
        // `content` pointer resolves, and the opaque payload is neither read nor
        // treated as an orphan.
        assert_eq!(std::fs::read(dir.join("photo.jpg")).unwrap(), [0xff, 0xd8, 0xff, 0xe0, 0x00]);
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn attachment_for_finds_the_sidecar_and_refuses_a_document_payload() {
        let dir = tempdir("lookup");
        write(&dir, "index.md", b"---\ntitle: Home\n---\n");
        write(&dir, "assets/logo.png", &[0x89, 0x50, 0x4e, 0x47]);

        assert!(block_on(ws(&dir).attachment_for(Path::new("assets/logo.png"))).unwrap().is_none());
        block_on(ws(&dir).attach(Path::new("assets/logo.png"), Path::new("index.md"))).unwrap();
        assert_eq!(
            block_on(ws(&dir).attachment_for(Path::new("assets/logo.png"))).unwrap(),
            Some(PathBuf::from("assets/logo.png.yaml"))
        );

        // A readable document is not an attachment — adopt it instead.
        write(&dir, "note.md", b"---\ntitle: Note\n---\nbody\n");
        let err = block_on(ws(&dir).attach(Path::new("note.md"), Path::new("index.md"))).unwrap_err();
        assert!(err.to_string().contains("not an opaque attachment"), "{err}");
    }

    #[test]
    fn loose_attachments_lists_only_unsidecarred_binaries() {
        let dir = tempdir("loose");
        write(&dir, "index.md", b"---\ntitle: Home\n---\n");
        write(&dir, "a.pdf", b"%PDF-1.7\n");
        write(&dir, "sub/b.png", &[0x89, 0x50]);
        // A prose document is not a payload; it should never appear.
        write(&dir, "sub/note.md", b"---\ntitle: Note\n---\n");

        let mut loose = block_on(ws(&dir).loose_attachments()).unwrap();
        loose.sort();
        assert_eq!(loose, vec![PathBuf::from("a.pdf"), PathBuf::from("sub/b.png")]);

        // Attaching one drops it from the loose set (its sidecar now claims it).
        block_on(ws(&dir).attach(Path::new("a.pdf"), Path::new("index.md"))).unwrap();
        assert_eq!(
            block_on(ws(&dir).loose_attachments()).unwrap(),
            vec![PathBuf::from("sub/b.png")]
        );
    }

    #[test]
    fn renaming_a_sidecar_moves_its_payload_and_keeps_content_correct() {
        // A sidecar is a separated node, so the existing move machinery relocates
        // the payload beside it and repoints `content` — no attachment-specific code.
        let dir = tempdir("rename");
        write(&dir, "index.md", b"---\ntitle: Home\n---\n");
        write(&dir, "photo.jpg", &[0xff, 0xd8]);
        block_on(ws(&dir).attach(Path::new("photo.jpg"), Path::new("index.md"))).unwrap();

        block_on(ws(&dir).rename(Path::new("photo.jpg.yaml"), Path::new("media/hero.jpg.yaml")))
            .unwrap();

        assert!(dir.join("media/hero.jpg").exists(), "payload moved beside the sidecar");
        assert!(!dir.join("photo.jpg").exists(), "old payload gone");
        assert!(read(&dir, "media/hero.jpg.yaml").contains("content: hero.jpg"), "content repointed");
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }
}
