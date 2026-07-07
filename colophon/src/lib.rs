//! # colophon
//!
//! A *self-describing plaintext workspace*: a set of documents whose structure
//! lives in the documents' own embedded metadata (frontmatter), not in the
//! filesystem layout or an app-private sidecar folder.
//!
//! The name is the point. A *colophon* is the note in which a book describes its
//! own making — the type, the paper, the press. A colophon workspace is one you
//! can hand to any tool and it explains itself: follow the links in the metadata
//! and the whole structure unfolds, with a distinguished root that describes the
//! whole.
//!
//! ## The shape of the abstraction
//!
//! - **Documents** are plaintext files with an embedded metadata block
//!   ([`document::Document`]).
//! - **Relations** are named links declared in that metadata
//!   ([`relation::RelationSet`]). *Which* fields are links is configurable
//!   (`contents`/`part_of`, `links`, or your own vocabulary); the mechanism is
//!   not. Exactly one relation may be marked **spanning** — the single-parent
//!   tree that gives the workspace its self-describing discovery spine. Every
//!   other relation may be many-to-many, so the tree is a backbone, never a
//!   ceiling.
//! - **Identity** is a strictly-additive layer ([`identity`], [`index`]). The
//!   graph, traversal, and (eventually) mutation operate on *paths* and never
//!   require an ID. Turn identity off and it compiles out; turn it on and IDs
//!   are minted only when something durably refers to a document.
//!
//! ## Status
//!
//! Early extraction from `diaryx_core`. The pure layers — embedded-metadata
//! parsing ([`meta`]), document splitting, and relation extraction — are real
//! and tested. The filesystem-driven scan/traversal/mutation engine ports next;
//! its seams ([`workspace::Workspace`], [`identity::IdentityPolicy`],
//! [`index::IndexStore`]) are staked out here so nothing diaryx-specific leaks
//! into the eventual public API.

pub mod document;
pub mod edit;
pub mod error;
pub mod exec;
pub mod fs;
pub mod identity;
pub mod index;
pub mod link;
pub mod meta;
pub mod mutate;
pub mod relation;
pub mod tree;
pub mod validate;
pub mod workspace;

pub use document::{Document, EmbedType};
pub use error::{Error, Result};
pub use exec::block_on;
pub use fig::Format;
pub use fs::{Storage, StdFs};
pub use identity::{Id, IdentityPolicy, Minter, Registration, Trigger};
pub use index::{FileIndex, InMemoryIndex, IndexStore, NoIndex};
pub use link::Link;
pub use meta::{Mapping, Value};
pub use relation::{Cardinality, Edge, Relation, RelationSet};
pub use tree::{Node, NodeKind};
pub use validate::Finding;
pub use workspace::{Target, Workspace};
