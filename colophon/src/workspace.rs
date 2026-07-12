//! The workspace handle — where the filesystem, relation vocabulary, identity
//! policy, and index store are composed.
//!
//! The type parameters encode the "identity is a bolt-on" design: a
//! `Workspace<FS>` defaults to [`NoIdentity`] + [`NoIndex`] — paths only, with
//! the identity machinery compiled out. Opting in is one builder line that flips
//! a type parameter:
//!
//! ```no_run
//! use colophon::workspace::Workspace;
//! use colophon::relation::RelationSet;
//! # fn demo<FS>(fs: FS) {
//! // Paths only — no ID ever touches a document.
//! let ws = Workspace::builder(fs).root("vault").build();
//! # let _ = ws;
//! # }
//! ```
//!
//! The filesystem-driven `scan`/traverse/mutate engine ports from `diaryx_core`
//! next; the seams are in place so that port has somewhere to land.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::content::ContentFormat;
use crate::document::whole_file_format;
use crate::error::{Error, Result};
use crate::fs::Storage;
use crate::identity::{IdentityPolicy, NoIdentity, Trigger};
use crate::index::{IndexStore, NoIndex};
use crate::link::{self, Addressing, Link, LinkStyle, ReferenceStyle, Wrapper};
use crate::meta::Value;
use crate::relation::RelationSet;
use crate::title::{self, TitleIndex, TitleMatch};

/// A composed workspace: a filesystem, a relation vocabulary, an identity
/// policy, an index store, and the link style it authors in.
#[derive(Debug, Clone)]
pub struct Workspace<FS, Id = NoIdentity, Ix = NoIndex> {
    fs: FS,
    root: PathBuf,
    relations: RelationSet,
    identity: Id,
    index: Ix,
    link_style: LinkStyle,
    id_links: bool,
    reference_style: Option<ReferenceStyle>,
    default_embed_format: fig::Format,
}

impl<FS> Workspace<FS, NoIdentity, NoIndex> {
    /// Start building a paths-only workspace over `fs`. Defaults: root `"."`,
    /// the [`RelationSet::diaryx`] vocabulary, identity off, and the default
    /// [`LinkStyle`] (`MarkdownRoot`, matching diaryx).
    pub fn builder(fs: FS) -> WorkspaceBuilder<FS, NoIdentity, NoIndex> {
        WorkspaceBuilder {
            fs,
            root: PathBuf::from("."),
            relations: RelationSet::diaryx(),
            identity: NoIdentity,
            index: NoIndex,
            link_style: LinkStyle::default(),
            id_links: false,
            reference_style: None,
            default_embed_format: fig::Format::Yaml,
        }
    }
}

impl<FS, Id, Ix> Workspace<FS, Id, Ix> {
    /// The workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The configured relation vocabulary.
    pub fn relations(&self) -> &RelationSet {
        &self.relations
    }

    /// The identity policy.
    pub fn identity(&self) -> &Id {
        &self.identity
    }

    /// The index store.
    pub fn index(&self) -> &Ix {
        &self.index
    }

    /// The link style this workspace authors in (read from the root's
    /// `link_format`, or the default).
    pub fn link_style(&self) -> LinkStyle {
        self.link_style
    }

    /// Whether this workspace authors durable structural links by id
    /// (registering the target) rather than a path — a convenience view over the
    /// effective default [`reference_style`](Self::reference_style).
    pub fn id_links(&self) -> bool {
        self.reference_style().registers()
    }

    /// The workspace-default reference style — the fallback for any relation
    /// without its own `style` override. An explicit `reference_style` builder
    /// value wins; otherwise it is derived from the legacy `link_style`/`id_links`
    /// builder inputs so existing configurations behave exactly as before.
    pub fn reference_style(&self) -> ReferenceStyle {
        self.reference_style.unwrap_or(ReferenceStyle {
            wrapper: Wrapper::Markdown,
            addressing: if self.id_links { Addressing::Id } else { Addressing::Path },
            label: false,
            path_style: self.link_style,
        })
    }

    /// The reference style colophon authors `relation`'s links in: the
    /// relation's own override if it declares one, else the workspace default.
    pub fn reference_style_for(&self, relation: &str) -> ReferenceStyle {
        self.relations.style_for(relation).unwrap_or_else(|| self.reference_style())
    }

    /// The metadata format a new document gets when it inherits no parent block
    /// — a *default* for authoring, not a workspace constraint (existing
    /// documents keep their own format on write, §7).
    pub fn default_embed_format(&self) -> fig::Format {
        self.default_embed_format
    }

    /// Mutable access to the index store (e.g. to persist it after mutations).
    pub fn index_mut(&mut self) -> &mut Ix {
        &mut self.index
    }
}

/// Whether `path` names a document the title scan should read — one whose
/// extension is a recognized body format (Markdown/Djot/HTML) or a whole-file
/// metadata format (YAML/JSON/…). Non-document files (images, binaries) are
/// skipped so the scan neither reads nor mis-indexes them.
fn is_document_path(path: &Path) -> bool {
    ContentFormat::from_extension(path).is_some() || whole_file_format(path).is_some()
}

/// The resolution of one link target against a workspace: a path, an ID the
/// registry does not currently resolve, or an off-workspace reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A (normalized, workspace-relative) path.
    Path(PathBuf),
    /// An `id:<id>` reference with no live registry entry — unknown,
    /// tombstoned, or the workspace has no registry at all.
    UnresolvedId(crate::identity::Id),
    /// A nominal (alias) reference whose name several documents claim, so it
    /// cannot be resolved to one. The `String` is the name as written.
    AmbiguousAlias(String),
    /// A URL or mail address — never resolved against the workspace and never
    /// rewritten by moves.
    External,
}

impl<FS, Id, Ix: IndexStore> Workspace<FS, Id, Ix> {
    /// Resolve `link` (declared in the document at `doc`) to a workspace target,
    /// without nominal (alias) resolution — path and `id:` targets only. Use
    /// [`resolve_link_with`](Self::resolve_link_with) when a [`TitleIndex`] is
    /// available and `[[My File]]`-style aliases should resolve.
    pub fn resolve_link(&self, doc: &Path, link: &Link) -> Target {
        self.resolve_link_with(doc, link, None)
    }

    /// Resolve `link` to a workspace target. Path targets resolve relative to
    /// `doc`'s directory; an `id:<id>` target resolves through the registry (the
    /// location-independent path that stays valid across moves); an
    /// alias-shaped target (a bare name) resolves through `titles` when one is
    /// supplied — `Unique` to its path, `Ambiguous` to
    /// [`Target::AmbiguousAlias`], and `Unknown` falling through to a path (so a
    /// nominal link to nothing surfaces as a missing/broken path, exactly as
    /// before aliases existed). With `titles` `None`, alias resolution is off
    /// and this is the pure path/id resolver.
    pub fn resolve_link_with(
        &self,
        doc: &Path,
        link: &Link,
        titles: Option<&TitleIndex>,
    ) -> Target {
        if link.is_external() {
            return Target::External;
        }
        if let Some(id) = link.id_target() {
            return match self.index.resolve(&id) {
                Some(path) => Target::Path(link::normalize(path)),
                None => Target::UnresolvedId(id),
            };
        }
        if let Some(titles) = titles
            && title::is_alias_shaped(&link.target)
        {
            match titles.resolve(&link.target) {
                TitleMatch::Unique(path) => return Target::Path(link::normalize(path)),
                TitleMatch::Ambiguous(_) => return Target::AmbiguousAlias(link.target.clone()),
                // Unknown: fall through — a bare name with nothing behind it is
                // treated as a path, so it reads as missing like any dead link.
                TitleMatch::Unknown => {}
            }
        }
        Target::Path(link::resolve(doc, &link.target))
    }
}

impl<FS: Storage, Id, Ix: IndexStore> Workspace<FS, Id, Ix> {
    /// The registry document this workspace's root declares: the first target
    /// of the registry-pointer relation on `root_doc`, resolved. `None` when
    /// the vocabulary has no registry relation or the root does not declare
    /// one — the workspace simply has no (discoverable) registry.
    ///
    /// This is the anti-`.obsidian/` move: where the identity state lives is a
    /// fact *about the workspace*, declared in the root's own metadata like
    /// every other link — reachable, validatable, and tool-agnostic — rather
    /// than an app-private path convention.
    pub async fn registry_path(&self, root_doc: &Path) -> Result<Option<PathBuf>> {
        match self.relations().registry_relation() {
            Some(relation) => self.pointer_target(root_doc, relation).await,
            None => Ok(None),
        }
    }

    /// The workspace-config document this root declares via the config-pointer
    /// relation (§6, the registry's reachability move applied to policy). `None`
    /// when the vocabulary has no config relation or the root declares none —
    /// the workspace simply runs on defaults.
    pub async fn config_path(&self, root_doc: &Path) -> Result<Option<PathBuf>> {
        match self.relations().config_relation() {
            Some(relation) => self.pointer_target(root_doc, relation).await,
            None => Ok(None),
        }
    }

    /// Read a single workspace-config value by `key` from the linked config
    /// document. `None` when there is no config document or it lacks the key —
    /// the caller falls back to its default.
    pub async fn config_get(
        &self,
        root_doc: &Path,
        key: &str,
    ) -> Result<Option<crate::meta::Value>> {
        let Some(config_doc) = self.config_path(root_doc).await? else {
            return Ok(None);
        };
        let (_, doc) = self.load(&config_doc).await?;
        Ok(doc.meta.get(key).cloned())
    }

    /// Build the workspace's [`TitleIndex`] by scanning every document under the
    /// root and registering it under its `title` and its file stem. This is a
    /// **derived cache** (DESIGN §5): rebuilt on demand, never persisted. It is
    /// what makes nominal (`[[My File]]`) references resolvable — a flat
    /// filesystem scan, deliberately independent of link resolution so that
    /// alias links can themselves be *spanning* (`contents: alias`) without a
    /// chicken-and-egg between "walk the tree" and "resolve the walk's links."
    pub async fn title_index(&self) -> Result<TitleIndex> {
        let mut index = TitleIndex::new();
        self.scan_titles(PathBuf::new(), &mut index).await?;
        Ok(index)
    }

    /// Scan every document under the root for a self-stored `id` frontmatter
    /// field, returning the `(id, path)` pairs — the rebuildable id→path map for
    /// the frontmatter-only identity storage mode ([`IdStorage::FrontmatterOnly`]).
    /// Like [`title_index`](Self::title_index) this is a flat filesystem scan,
    /// deliberately independent of link resolution (so it can bootstrap the very
    /// index that id links resolve through, with no chicken-and-egg).
    ///
    /// [`IdStorage::FrontmatterOnly`]: crate::config::IdStorage::FrontmatterOnly
    pub async fn scan_ids(&self) -> Result<Vec<(crate::identity::Id, PathBuf)>> {
        let mut ids = Vec::new();
        self.scan_ids_dir(PathBuf::new(), &mut ids).await?;
        Ok(ids)
    }

    /// Recursively collect self-stored `id` fields under `rel_dir`. Same walk as
    /// [`scan_titles`](Self::scan_titles); unreadable/hidden entries are skipped.
    fn scan_ids_dir<'a>(
        &'a self,
        rel_dir: PathBuf,
        ids: &'a mut Vec<(crate::identity::Id, PathBuf)>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        Box::pin(async move {
            let Ok(entries) = self.fs.read_dir(&self.root.join(&rel_dir)).await else {
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
                    self.scan_ids_dir(rel, ids).await?;
                } else if entry.file_type().is_file()
                    && is_document_path(&rel)
                    && let Ok((_, doc)) = self.load(&rel).await
                    && let Some(id) = doc.meta.get("id").and_then(Value::as_str)
                    && !id.trim().is_empty()
                {
                    ids.push((crate::identity::Id(id.trim().to_string()), rel));
                }
            }
            Ok(())
        })
    }

    /// Recursively index the documents under the workspace-relative `rel_dir`.
    /// Unreadable directories and files are skipped (a title index is a
    /// best-effort cache, not a validation pass); hidden entries (`.`-prefixed)
    /// are ignored.
    fn scan_titles<'a>(
        &'a self,
        rel_dir: PathBuf,
        index: &'a mut TitleIndex,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        Box::pin(async move {
            let Ok(entries) = self.fs.read_dir(&self.root.join(&rel_dir)).await else {
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
                    self.scan_titles(rel, index).await?;
                } else if entry.file_type().is_file() && is_document_path(&rel) {
                    // Always index by stem (name-based resolution, Obsidian-style)…
                    if let Some(stem) = rel.file_stem().and_then(|s| s.to_str()) {
                        index.insert(stem, rel.clone());
                    }
                    // …and by the declared `title` when the document parses.
                    if let Ok((_, doc)) = self.load(&rel).await
                        && let Some(title) = doc.meta.get("title").and_then(Value::as_str)
                    {
                        index.insert(title, rel.clone());
                    }
                }
            }
            Ok(())
        })
    }

    /// Resolve the first target of `relation` declared on `root_doc` to a
    /// workspace path — the shared mechanic behind the registry and config
    /// pointers: a workspace resource named by a well-known relation on the root.
    async fn pointer_target(&self, root_doc: &Path, relation: &str) -> Result<Option<PathBuf>> {
        let root_doc = link::normalize(root_doc);
        let (_, doc) = self.load(&root_doc).await?;
        let Some(raw) = doc
            .meta
            .get(relation)
            .map(crate::meta::Value::link_strings)
            .and_then(|targets| targets.into_iter().next())
        else {
            return Ok(None);
        };
        match self.resolve_link(&root_doc, &Link::parse(&raw)) {
            Target::Path(path) => Ok(Some(path)),
            _ => Ok(None),
        }
    }
}

impl<FS: Storage, Id: IdentityPolicy, Ix: IndexStore> Workspace<FS, Id, Ix> {
    /// Ensure the document at `path` has a registered stable ID, minting one if
    /// needed, and return it. Idempotent: an already-registered document
    /// returns its existing ID regardless of `event`.
    ///
    /// A fresh registration only happens when the identity policy's trigger
    /// set fires on `event` (DESIGN §4's registration lifecycle) — an inactive
    /// trigger is an error, so callers cannot silently grow the authoritative
    /// set beyond what the policy allows.
    pub async fn register(&mut self, path: &Path, event: Trigger) -> Result<crate::identity::Id> {
        let path = link::normalize(path);
        if let Some(id) = self.index.id_for_path(&path) {
            return Ok(id);
        }
        if !self.identity.registration().fires_on(event) {
            return Err(Error::Structure(format!(
                "identity policy does not register on {event:?}"
            )));
        }
        if !self.fs.try_exists(&self.root.join(&path)).await? {
            return Err(Error::Structure(format!("{} does not exist", path.display())));
        }
        let id = self.mint_unique(&path);
        self.index.register(&id, &path);
        Ok(id)
    }

    /// Mint until the ID is unknown to the index — including tombstones, so a
    /// deleted document's ID is never reissued to mean something else.
    pub(crate) fn mint_unique(&mut self, path: &Path) -> crate::identity::Id {
        loop {
            let id = self.identity.mint(path);
            if !self.index.is_known(&id) {
                return id;
            }
        }
    }

    /// The scalar colophon writes for a durable link declared by `relation` from
    /// the document at `from` to `to` (titled `title`). The style is
    /// [`reference_style_for`](Self::reference_style_for)`(relation)`, so links
    /// going "down" (e.g. `contents`) and "up" (e.g. `part_of`) can differ. An
    /// `id`-addressing style registers `to` first (the link-by-id trigger) so the
    /// link survives a move untouched; if identity does not register on a link,
    /// [`format_reference`](link::format_reference) degrades it to a path.
    ///
    /// `target_exists` says whether `to` is already on disk: `true` registers it
    /// through the existence-checked [`register`](Self::register); `false` (a
    /// document being created in the same operation) mints and registers directly.
    /// The single seam through which create, rename repair, and autofix author a
    /// link.
    pub(crate) async fn authored_target(
        &mut self,
        relation: &str,
        from: &Path,
        to: &Path,
        title: &str,
        target_exists: bool,
    ) -> Result<String> {
        let style = self.reference_style_for(relation);
        let id = if style.registers() && self.identity.registration().fires_on(Trigger::Link) {
            Some(if target_exists {
                self.register(to, Trigger::Link).await?
            } else {
                self.register_for_authoring(to)
            })
        } else {
            None
        };
        Ok(link::format_reference(style, from, to, id.as_ref(), title))
    }

    /// Ensure `path` has an ID for the purpose of authoring a link *to* a
    /// document this same operation is creating — so the on-disk existence check
    /// in [`register`](Self::register) does not yet hold. Idempotent: returns any
    /// existing ID, else mints and registers one.
    pub(crate) fn register_for_authoring(&mut self, path: &Path) -> crate::identity::Id {
        let path = link::normalize(path);
        if let Some(id) = self.index.id_for_path(&path) {
            return id;
        }
        let id = self.mint_unique(&path);
        self.index.register(&id, &path);
        id
    }
}

impl<FS: Storage, Id, Ix> Workspace<FS, Id, Ix> {
    /// The underlying filesystem.
    pub fn fs(&self) -> &FS {
        &self.fs
    }

    // TODO(port): scan/traverse/mutate from diaryx_core::workspace land here,
    // driving `fs` and maintaining `index` when `identity` triggers fire.
}

/// Builder for [`Workspace`]. Setting an identity policy or index store returns
/// a builder with a new type parameter, so the composed [`Workspace`] carries
/// exactly the layers requested — and none it does not.
#[derive(Debug, Clone)]
pub struct WorkspaceBuilder<FS, Id, Ix> {
    fs: FS,
    root: PathBuf,
    relations: RelationSet,
    identity: Id,
    index: Ix,
    link_style: LinkStyle,
    id_links: bool,
    reference_style: Option<ReferenceStyle>,
    default_embed_format: fig::Format,
}

impl<FS, Id, Ix> WorkspaceBuilder<FS, Id, Ix> {
    /// Set the workspace root.
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = root.into();
        self
    }

    /// Set the relation vocabulary.
    pub fn relations(mut self, relations: RelationSet) -> Self {
        self.relations = relations;
        self
    }

    /// Set the link style this workspace authors in (typically read from the
    /// root's `link_format`).
    pub fn link_style(mut self, link_style: LinkStyle) -> Self {
        self.link_style = link_style;
        self
    }

    /// Author durable structural links by id (Obsidian-style) rather than paths.
    /// A convenience over [`reference_style`](Self::reference_style); effective
    /// only when identity registers on a link.
    pub fn id_links(mut self, id_links: bool) -> Self {
        self.id_links = id_links;
        self
    }

    /// Set the workspace-default reference style — the fallback for relations
    /// without their own override. Supersedes the `link_style`/`id_links`
    /// convenience inputs when set.
    pub fn reference_style(mut self, style: ReferenceStyle) -> Self {
        self.reference_style = Some(style);
        self
    }

    /// Set the metadata format new documents get when they inherit no parent
    /// block (a default, not a constraint).
    pub fn default_embed_format(mut self, format: fig::Format) -> Self {
        self.default_embed_format = format;
        self
    }

    /// Attach an identity policy, turning identity on.
    pub fn identity<Id2>(self, identity: Id2) -> WorkspaceBuilder<FS, Id2, Ix> {
        WorkspaceBuilder {
            fs: self.fs,
            root: self.root,
            relations: self.relations,
            identity,
            index: self.index,
            link_style: self.link_style,
            id_links: self.id_links,
            reference_style: self.reference_style,
            default_embed_format: self.default_embed_format,
        }
    }

    /// Attach an index store (where IDs are persisted).
    pub fn index<Ix2>(self, index: Ix2) -> WorkspaceBuilder<FS, Id, Ix2> {
        WorkspaceBuilder {
            fs: self.fs,
            root: self.root,
            relations: self.relations,
            identity: self.identity,
            index,
            link_style: self.link_style,
            id_links: self.id_links,
            reference_style: self.reference_style,
            default_embed_format: self.default_embed_format,
        }
    }

    /// Finish building.
    pub fn build(self) -> Workspace<FS, Id, Ix> {
        Workspace {
            fs: self.fs,
            root: self.root,
            relations: self.relations,
            identity: self.identity,
            index: self.index,
            link_style: self.link_style,
            id_links: self.id_links,
            reference_style: self.reference_style,
            default_embed_format: self.default_embed_format,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{IdentityPolicy, Minter};
    use crate::index::InMemoryIndex;

    // A stand-in filesystem — the seam is exercised without a real backend.
    #[derive(Clone)]
    struct DummyFs;

    #[test]
    fn paths_only_by_default() {
        let ws = Workspace::builder(DummyFs).root("vault").build();
        assert_eq!(ws.root(), Path::new("vault"));
        assert_eq!(ws.relations().spanning_relation(), Some("contents"));
        // Identity off: the default policy fires no triggers.
        assert!(!ws.identity().registration().is_active());
    }

    #[test]
    fn identity_opts_in_via_one_builder_line() {
        let ws = Workspace::builder(DummyFs)
            .root("vault")
            .identity(Minter::lazy(1))
            .index(InMemoryIndex::new())
            .build();
        assert!(ws.identity().registration().on_link);
        assert!(ws.index().is_empty());
    }
}
