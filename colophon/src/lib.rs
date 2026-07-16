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

// At least one embedded-metadata format backend must be compiled in, otherwise
// colophon can neither parse nor serialize any metadata. The format features
// (`yaml`, `json`, `fig-lang`) forward to the matching `fig` parser — see
// `Cargo.toml`.
#[cfg(not(any(feature = "yaml", feature = "json", feature = "fig-lang")))]
compile_error!(
    "colophon needs at least one metadata-format feature enabled: \
     `yaml` (the default), `json`, or `fig-lang`. \
     You have disabled the default feature without selecting a replacement."
);

pub mod attach;
pub mod change;
pub mod config;
pub mod content;
pub mod document;
pub mod edit;
pub mod error;
pub mod exec;
pub mod fs;
pub mod identity;
pub mod index;
pub mod intake;
pub mod journal;
pub mod link;
pub mod meta;
pub mod mutate;
pub mod relation;
pub mod route;
pub mod title;
pub mod tree;
pub mod validate;
pub mod workspace;

pub use change::{ChangeSet, FileOp};
pub use config::{IdStorage, RelationStyleConfig, WorkspaceConfig};
pub use content::ContentFormat;
pub use content::{code_spans, render_html};
pub use document::{Document, EmbedStyle, EmbedType, embed_carrier, is_opaque_payload};
pub use error::{Error, Result};
pub use exec::block_on;
pub use fig::Format;
pub use fs::{Capabilities, Storage, StdFs};
pub use identity::{Id, IdentityPolicy, Minter, Registration, Trigger};
pub use index::{FileIndex, InMemoryIndex, IndexStore, NoIndex};
pub use intake::{Adoption, PlanOutcome, StructurePlan, SynthNode};
pub use journal::{Recovered, recover};
pub use link::{
    Addressing, BodyLink, Link, LinkStyle, ReferenceStyle, Wikilink, Wrapper, format_link,
    path_to_title,
};
pub use meta::{Mapping, Value};
pub use mutate::Created;
pub use relation::{Cardinality, Edge, Relation, RelationSet};
pub use route::{Layout, RoutePlan};
pub use title::{TitleIndex, TitleMatch};
pub use tree::{Node, NodeKind};
pub use validate::{Backlink, CensusEntry, Finding, Fix, LinkSite, Resolution};
pub use workspace::{Target, Workspace};
