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

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fs::Storage;
use crate::identity::{IdentityPolicy, NoIdentity, Trigger};
use crate::index::{IndexStore, NoIndex};
use crate::link::{self, Link};
use crate::relation::RelationSet;

/// A composed workspace: a filesystem, a relation vocabulary, an identity
/// policy, and an index store.
#[derive(Debug, Clone)]
pub struct Workspace<FS, Id = NoIdentity, Ix = NoIndex> {
    fs: FS,
    root: PathBuf,
    relations: RelationSet,
    identity: Id,
    index: Ix,
}

impl<FS> Workspace<FS, NoIdentity, NoIndex> {
    /// Start building a paths-only workspace over `fs`. Defaults: root `"."`,
    /// the [`RelationSet::diaryx`] vocabulary, identity off.
    pub fn builder(fs: FS) -> WorkspaceBuilder<FS, NoIdentity, NoIndex> {
        WorkspaceBuilder {
            fs,
            root: PathBuf::from("."),
            relations: RelationSet::diaryx(),
            identity: NoIdentity,
            index: NoIndex,
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

    /// Mutable access to the index store (e.g. to persist it after mutations).
    pub fn index_mut(&mut self) -> &mut Ix {
        &mut self.index
    }
}

/// The resolution of one link target against a workspace: a path, an ID the
/// registry does not currently resolve, or an off-workspace reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A (normalized, workspace-relative) path.
    Path(PathBuf),
    /// A `colophon:<id>` reference with no live registry entry — unknown,
    /// tombstoned, or the workspace has no registry at all.
    UnresolvedId(crate::identity::Id),
    /// A URL or mail address — never resolved against the workspace and never
    /// rewritten by moves.
    External,
}

impl<FS, Id, Ix: IndexStore> Workspace<FS, Id, Ix> {
    /// Resolve `link` (declared in the document at `doc`) to a workspace
    /// target. Path targets resolve relative to `doc`'s directory; a
    /// `colophon:<id>` target resolves through the registry — the
    /// location-independent path that stays valid across moves.
    pub fn resolve_link(&self, doc: &Path, link: &Link) -> Target {
        if link.is_external() {
            return Target::External;
        }
        if let Some(id) = link.id_target() {
            return match self.index.resolve(&id) {
                Some(path) => Target::Path(link::normalize(path)),
                None => Target::UnresolvedId(id),
            };
        }
        Target::Path(link::resolve(doc, &link.target))
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

    /// Attach an identity policy, turning identity on.
    pub fn identity<Id2>(self, identity: Id2) -> WorkspaceBuilder<FS, Id2, Ix> {
        WorkspaceBuilder {
            fs: self.fs,
            root: self.root,
            relations: self.relations,
            identity,
            index: self.index,
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
