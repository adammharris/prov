//! Index — where stable IDs and (later) the materialized graph live.
//!
//! The [`IndexStore`] is the single artifact that fuses two natures (DESIGN
//! §5): the **authoritative** id↔path registry — not rebuildable from the
//! documents — and (to come) the **derived** resolution cache and adjacency
//! index, which are. Keeping the store behind a trait is deliberate: a sidecar
//! file, an in-memory map, or a sync-backed store are all valid homes.
//!
//! ## Tombstones — IDs are forever
//!
//! DESIGN's open question #1 ("does the registry ever need to survive without
//! its documents?") is answered **yes, minimally**: deleting a document leaves
//! a *tombstone* — the ID stops resolving but is never forgotten, so it can
//! never be reminted to mean something else. A dangling `colophon:` reference
//! then stays *diagnosable* (validation can say "that document was deleted")
//! instead of becoming a silent re-resolution hazard.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::identity::Id;
use crate::meta::{self, Mapping, Value};

/// Somewhere IDs (and eventually derived graph data) are persisted and queried.
pub trait IndexStore {
    /// Record that `id` names the document at `path`.
    fn register(&mut self, id: &Id, path: &Path);

    /// Resolve an ID to its current path. `None` for unknown *and* tombstoned
    /// IDs — use [`is_known`](IndexStore::is_known) to tell them apart.
    fn resolve(&self, id: &Id) -> Option<PathBuf>;

    /// The ID currently assigned to `path`, if any.
    fn id_for_path(&self, path: &Path) -> Option<Id>;

    /// Update the path an ID points at (e.g. after a move/rename).
    fn set_path(&mut self, id: &Id, new_path: &Path);

    /// Retire an ID (e.g. after a delete). A store with tombstones keeps the
    /// ID on record so it is never reissued; a plain store may forget it.
    fn unregister(&mut self, id: &Id);

    /// Whether `id` has *ever* been issued — live or tombstoned. This is the
    /// mint-with-rejection predicate: a fresh ID must be `!is_known`.
    fn is_known(&self, id: &Id) -> bool {
        self.resolve(id).is_some()
    }
}

/// No index — identity-off workspaces. Registers nothing, resolves nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoIndex;

impl IndexStore for NoIndex {
    fn register(&mut self, _id: &Id, _path: &Path) {}
    fn resolve(&self, _id: &Id) -> Option<PathBuf> {
        None
    }
    fn id_for_path(&self, _path: &Path) -> Option<Id> {
        None
    }
    fn set_path(&mut self, _id: &Id, _new_path: &Path) {}
    fn unregister(&mut self, _id: &Id) {}
}

/// A simple in-memory registry — for tests and ephemeral workspaces. No
/// tombstones: an unregistered ID is forgotten entirely.
#[derive(Debug, Clone, Default)]
pub struct InMemoryIndex {
    forward: HashMap<Id, PathBuf>,
    reverse: HashMap<PathBuf, Id>,
}

impl InMemoryIndex {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of registered IDs.
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }
}

impl IndexStore for InMemoryIndex {
    fn register(&mut self, id: &Id, path: &Path) {
        self.forward.insert(id.clone(), path.to_path_buf());
        self.reverse.insert(path.to_path_buf(), id.clone());
    }

    fn resolve(&self, id: &Id) -> Option<PathBuf> {
        self.forward.get(id).cloned()
    }

    fn id_for_path(&self, path: &Path) -> Option<Id> {
        self.reverse.get(path).cloned()
    }

    fn set_path(&mut self, id: &Id, new_path: &Path) {
        if let Some(old) = self.forward.insert(id.clone(), new_path.to_path_buf()) {
            self.reverse.remove(&old);
        }
        self.reverse.insert(new_path.to_path_buf(), id.clone());
    }

    fn unregister(&mut self, id: &Id) {
        if let Some(path) = self.forward.remove(id) {
            self.reverse.remove(&path);
        }
    }
}

/// The persistent registry: a snapshot with tombstones, (de)serialized through
/// `fig` so the on-disk format is any the workspace compiles in.
///
/// The rendered shape is designed for clean diffs (DESIGN §5): a single
/// `registry` section (leaving room for a sibling `derived` section later),
/// one record per line, sorted by ID. A live record is `id: path`; a tombstone
/// is `id: null`.
///
/// This type is pure — text in ([`FileIndex::from_str`]), text out
/// ([`FileIndex::render`]) — so any storage backend can host it; the caller
/// owns the I/O and can consult [`is_dirty`](FileIndex::is_dirty) to skip
/// no-op writes.
#[derive(Debug, Clone)]
pub struct FileIndex {
    live: InMemoryIndex,
    tombstones: BTreeSet<Id>,
    format: fig::Format,
    dirty: bool,
}

impl FileIndex {
    /// An empty registry that renders to `format`.
    pub fn new(format: fig::Format) -> Self {
        Self {
            live: InMemoryIndex::new(),
            tombstones: BTreeSet::new(),
            format,
            dirty: false,
        }
    }

    /// Parse a registry from its serialized `text` (in `format`). An empty
    /// text is an empty registry.
    pub fn from_str(text: &str, format: fig::Format) -> Result<Self> {
        let mut index = Self::new(format);
        let top = meta::parse_mapping(text, format)?;
        if let Some(registry) = top.get("registry").and_then(Value::as_mapping) {
            for (id, value) in registry {
                let id = Id(id.clone());
                match value {
                    Value::Null => {
                        index.tombstones.insert(id);
                    }
                    Value::String(path) => {
                        index.live.register(&id, Path::new(path));
                    }
                    _ => {
                        return Err(crate::error::Error::Structure(format!(
                            "registry entry `{id}` must be a path or null (tombstone)"
                        )));
                    }
                }
            }
        }
        Ok(index)
    }

    /// Serialize the registry: sorted, one record per line, tombstones as null.
    pub fn render(&self) -> Result<String> {
        let mut records: std::collections::BTreeMap<&Id, Value> = self
            .tombstones
            .iter()
            .map(|id| (id, Value::Null))
            .collect();
        for (id, path) in &self.live.forward {
            records.insert(id, Value::String(path.to_string_lossy().into_owned()));
        }
        let mut registry = Mapping::new();
        for (id, value) in records {
            registry.insert(id.0.clone(), value);
        }
        let mut top = Mapping::new();
        top.insert("registry".into(), Value::Mapping(registry));
        meta::serialize_mapping(&top, self.format)
    }

    /// Whether the registry changed since it was parsed/created (i.e. needs a
    /// write). Cleared by [`mark_clean`](FileIndex::mark_clean).
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Mark the registry as persisted.
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    /// The number of live (resolving) IDs.
    pub fn len(&self) -> usize {
        self.live.len()
    }

    /// Whether the registry has no live IDs.
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Whether `id` is retired: known but no longer resolving.
    pub fn is_tombstoned(&self, id: &Id) -> bool {
        self.tombstones.contains(id)
    }

    /// Iterate live records as `(id, path)`, sorted by ID.
    pub fn iter(&self) -> impl Iterator<Item = (&Id, &PathBuf)> {
        let mut live: Vec<_> = self.live.forward.iter().collect();
        live.sort_by(|a, b| a.0.cmp(b.0));
        live.into_iter()
    }
}

impl IndexStore for FileIndex {
    fn register(&mut self, id: &Id, path: &Path) {
        self.live.register(id, path);
        self.dirty = true;
    }

    fn resolve(&self, id: &Id) -> Option<PathBuf> {
        self.live.resolve(id)
    }

    fn id_for_path(&self, path: &Path) -> Option<Id> {
        self.live.id_for_path(path)
    }

    fn set_path(&mut self, id: &Id, new_path: &Path) {
        self.live.set_path(id, new_path);
        self.dirty = true;
    }

    /// Retire to a tombstone: the ID stops resolving but stays known forever.
    fn unregister(&mut self, id: &Id) {
        self.live.unregister(id);
        self.tombstones.insert(id.clone());
        self.dirty = true;
    }

    fn is_known(&self, id: &Id) -> bool {
        self.live.resolve(id).is_some() || self.tombstones.contains(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_and_resolves_both_directions() {
        let mut ix = InMemoryIndex::new();
        let id = Id("ajp7eq".into());
        ix.register(&id, Path::new("notes/a.md"));
        assert_eq!(ix.resolve(&id), Some(PathBuf::from("notes/a.md")));
        assert_eq!(ix.id_for_path(Path::new("notes/a.md")), Some(id.clone()));
        assert_eq!(ix.len(), 1);
    }

    #[test]
    fn move_updates_path_and_clears_stale_reverse() {
        let mut ix = InMemoryIndex::new();
        let id = Id("ajp7eq".into());
        ix.register(&id, Path::new("a.md"));
        ix.set_path(&id, Path::new("moved/a.md"));
        assert_eq!(ix.resolve(&id), Some(PathBuf::from("moved/a.md")));
        assert_eq!(ix.id_for_path(Path::new("a.md")), None);
        assert_eq!(ix.id_for_path(Path::new("moved/a.md")), Some(id));
    }

    #[test]
    fn unregister_removes_both_directions() {
        let mut ix = InMemoryIndex::new();
        let id = Id("x".into());
        ix.register(&id, Path::new("a.md"));
        ix.unregister(&id);
        assert!(ix.is_empty());
        assert_eq!(ix.id_for_path(Path::new("a.md")), None);
    }

    #[test]
    fn file_index_round_trips_sorted_with_tombstones() {
        let mut ix = FileIndex::new(fig::Format::Yaml);
        ix.register(&Id("zzzzzzz".into()), Path::new("z.md"));
        ix.register(&Id("bcdfghj".into()), Path::new("notes/a.md"));
        ix.register(&Id("mmmmmmm".into()), Path::new("gone.md"));
        ix.unregister(&Id("mmmmmmm".into()));

        let text = ix.render().unwrap();
        // Sorted, one record per line, tombstone as null.
        let b = text.find("bcdfghj").unwrap();
        let m = text.find("mmmmmmm").unwrap();
        let z = text.find("zzzzzzz").unwrap();
        assert!(b < m && m < z, "{text}");
        assert!(text.contains("mmmmmmm: null"), "{text}");

        let back = FileIndex::from_str(&text, fig::Format::Yaml).unwrap();
        assert_eq!(back.resolve(&Id("bcdfghj".into())), Some(PathBuf::from("notes/a.md")));
        assert_eq!(back.resolve(&Id("mmmmmmm".into())), None);
        assert!(back.is_known(&Id("mmmmmmm".into())), "tombstone survives the round-trip");
        assert!(back.is_tombstoned(&Id("mmmmmmm".into())));
        assert!(!back.is_dirty());
    }

    #[test]
    fn tombstoned_ids_are_never_free_for_reminting() {
        let mut ix = FileIndex::new(fig::Format::Yaml);
        let id = Id("bcdfghj".into());
        ix.register(&id, Path::new("a.md"));
        ix.unregister(&id);
        assert_eq!(ix.resolve(&id), None, "does not resolve");
        assert!(ix.is_known(&id), "but is still known — never reminted");
    }

    #[test]
    fn dirty_tracks_mutations() {
        let mut ix = FileIndex::new(fig::Format::Yaml);
        assert!(!ix.is_dirty());
        ix.register(&Id("x".into()), Path::new("a.md"));
        assert!(ix.is_dirty());
        ix.mark_clean();
        assert!(!ix.is_dirty());
    }

    #[test]
    fn empty_text_is_an_empty_registry() {
        let ix = FileIndex::from_str("", fig::Format::Yaml).unwrap();
        assert!(ix.is_empty());
    }
}
