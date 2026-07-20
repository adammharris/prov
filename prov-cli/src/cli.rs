//! The command-line surface: the `clap` argument grammar and the enums that
//! mirror the library's config axes.
//!
//! Every type here is a CLI *spelling* of a library concept — a `--layout` flag
//! that maps to [`prov::Layout`], a `--reference` value that maps to
//! [`prov::Addressing`], and so on — kept in one module so the argument
//! grammar is the CLI's business and the library enums stay free of `clap`. The
//! command *handlers* live elsewhere (`main.rs` and its sibling modules); this is
//! only the shape of what the user types.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use prov::{
    Addressing, ContentFormat, EmbedStyle, Format, IdStorage, Layout, LinkStyle, Notation,
    Registration, RelationStyleConfig, WorkspaceConfig, Wrapper,
};

/// `--layout` — the CLI mirror of [`Layout`], so the flag's spelling is the
/// CLI's business and the library enum stays free of clap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum LayoutArg {
    /// A directory per route segment, each holding an `index` node.
    Nested,
    /// Every synthesized node beside the start document.
    Flat,
}

impl From<LayoutArg> for Layout {
    fn from(arg: LayoutArg) -> Self {
        match arg {
            LayoutArg::Nested => Layout::Nested,
            LayoutArg::Flat => Layout::Flat,
        }
    }
}

/// The filename stem of the registry document the CLI creates on first
/// `prov id` — visible, beside the root, and *linked from the root's own
/// metadata* via the `registry` relation. Its extension is the workspace's
/// metadata format (see [`sidecar_name`]). Where the registry lives is a fact
/// about the workspace, declared in it; the CLI only supplies this default when
/// bootstrapping one. (It can equally be a `.md` file whose frontmatter carries
/// the records — anything the pointer targets.)
pub(crate) const REGISTRY_STEM: &str = "registry";

/// The filename stem of the config document the CLI creates on first
/// `prov config <k> <v>` (or at `init`) — beside the root, linked via the
/// `config` relation (the reachability move the registry uses). Workspace policy
/// lives here rather than bloating the root or hiding in a dotfile.
pub(crate) const CONFIG_STEM: &str = "prov";

/// The whole-file extension for a metadata format: the config and registry
/// sidecars are written in the workspace's *chosen metadata format*, not always
/// YAML — `yaml`/`json`/`figl`. Mirrors [`prov::document::whole_file_format`],
/// which parses them back.
pub(crate) fn sidecar_ext(format: Format) -> &'static str {
    match format {
        #[cfg(feature = "json")]
        Format::Json => "json",
        #[cfg(feature = "toml")]
        Format::Toml => "toml",
        #[cfg(feature = "fig-lang")]
        Format::Fig => "figl",
        _ => "yaml",
    }
}

/// The sidecar filename for `stem` in metadata `format` (e.g. `prov.figl`).
pub(crate) fn sidecar_name(stem: &str, format: Format) -> String {
    format!("{stem}.{}", sidecar_ext(format))
}

/// A self-describing plaintext workspace, from the command line.
#[derive(Parser)]
#[command(name = "prov", version, about, long_about = None)]
pub(crate) struct Cli {
    /// Run as if started in this directory — discover the workspace root from
    /// here instead of the current directory (like `git -C`, this goes *before*
    /// the subcommand: `prov -C ~/vault check`). Also settable via the `PROV_ROOT`
    /// environment variable; the flag wins. Lets a script or cron operate on a
    /// vault without `cd`-ing into it. Relative path arguments resolve here too.
    #[arg(short = 'C', long = "root", value_name = "DIR")]
    pub(crate) root: Option<PathBuf>,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Initialize a new workspace here: write a self-describing root document
    /// that the other commands can discover. The starting point — `tree`,
    /// `new`, and `check` all need a root to work from. On a terminal, prompts
    /// for anything not given as a flag; pass `--yes` to take every default.
    Init {
        /// Directory to initialize (default: the current directory). Created if
        /// it does not exist.
        dir: Option<PathBuf>,
        /// Title for the root document (default: the directory's name, titleized).
        #[arg(long)]
        title: Option<String>,
        /// Author to record in the root's metadata (default: none).
        #[arg(long)]
        author: Option<String>,
        /// Config language for the root's metadata: yaml/toml/json/fig
        /// (default: yaml). `fig` is unavailable with `--embed delimited`.
        #[arg(long, value_enum)]
        meta: Option<MetaFormat>,
        /// How that metadata is embedded: delimited, code-block, html-script,
        /// html-code, or separate. Must suit `--content` (default: the first
        /// style that content grammar offers).
        #[arg(long, value_enum)]
        embed: Option<EmbedArg>,
        /// Body-prose grammar; sets the root file's extension (default: markdown).
        #[arg(long, value_enum)]
        content: Option<ContentLang>,
        /// The syntactic wrapper prov authors references in: markdown
        /// (`[Title](target)`) or wikilink (`[[target]]`) (default: markdown).
        /// The first style axis — pick it, then `--reference` picks the target.
        #[arg(long, value_enum)]
        wrapper: Option<WrapperArg>,
        /// What references address their target by: path, id, alias (by title),
        /// or split (readable `contents` down / durable `part_of` up). `id` and
        /// `split` require `--identity` ≠ off; `alias`/`split` are by-title links
        /// with no markdown form, so the interactive menu offers them only under
        /// `--wrapper wikilink` (default: path).
        #[arg(long, value_enum)]
        reference: Option<ReferenceArg>,
        /// How *path* references are formatted — only used when a target is
        /// addressed by path (default: markdown-root).
        #[arg(long, value_enum)]
        link_style: Option<LinkStyleArg>,
        /// When documents earn a stable ID: off (paths only), lazy (on
        /// link-by-id or publish), or eager (at creation) (default: lazy).
        #[arg(long, value_enum)]
        identity: Option<IdentityArg>,
        /// Where IDs live: frontmatter (stamped into each document's `id` field,
        /// with the registry kept as a cache), registry (only in the registry
        /// document), or frontmatter-only (no registry document — self-describing,
        /// but no tombstones) (default: frontmatter).
        #[arg(long, value_enum)]
        id_storage: Option<IdStorageArg>,
        /// Content-checksum coverage for bit-rot detection: payloads (attachment
        /// files only — the default, frictionless), full (also document bodies,
        /// paired with `prov edit`), or off. Verified by `prov check`.
        #[arg(long, value_enum)]
        fixity: Option<FixityArg>,
        /// Delete straight to a hard delete instead of the recoverable recycle bin
        /// (the recycle bin is on by default — the safe archival posture).
        #[arg(long)]
        no_recycle_bin: bool,
        /// Frontmatter field `prov edit` stamps with an RFC 3339 UTC timestamp
        /// on a content change (e.g. `updated`). Omitted → the feature is off.
        #[arg(long, value_name = "FIELD")]
        updated_field: Option<String>,
        /// What to do with content documents already in the directory: `flat`
        /// links each one under the new root; `mirror` folds the folder tree into
        /// the containment tree (each directory becomes a node, synthesizing a
        /// folder index where none exists); `none` leaves them unlinked. Omit to
        /// be asked on a terminal (and to leave them unlinked otherwise).
        #[arg(long, value_enum)]
        adopt: Option<AdoptArg>,
        /// Also give the directory's *non-document* files (images, PDFs, data,
        /// binaries) each a metadata sidecar, linked under the root. Omit to be
        /// asked on a terminal; non-interactive leaves them alone (they stay
        /// invisible to `prov check` until attached).
        #[arg(long)]
        attach: bool,
        /// Accept every default without prompting.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Summarize a document: its metadata, spanning children, and declared links.
    Show {
        /// Path to a document (plaintext with embedded metadata).
        #[arg(value_name = "TARGET")]
        file: String,
    },
    /// List a document's links as `relation<TAB>target`, one per line.
    Links {
        /// Path to a document.
        #[arg(value_name = "TARGET")]
        file: String,
        /// Only show links declared by this relation (e.g. `contents`).
        #[arg(long)]
        relation: Option<String>,
    },
    /// Print a document's metadata block (without fences).
    Meta {
        /// Path to a document.
        #[arg(value_name = "TARGET")]
        file: String,
        /// Output format (default: the format the document already uses).
        #[arg(long, value_enum)]
        format: Option<MetaFormat>,
    },
    /// Print one metadata field by dotted path (e.g. `title`, `contents.0`).
    Get {
        /// Path to a document.
        #[arg(value_name = "TARGET")]
        file: String,
        /// Dotted key path; an all-digit segment indexes a sequence.
        key: String,
    },
    /// Print a document's body (everything outside the metadata block).
    Body {
        /// Path to a document.
        #[arg(value_name = "TARGET")]
        file: String,
    },
    /// Render a document's body to HTML (Markdown/Djot, via `twig`).
    Render {
        /// Path to a document.
        #[arg(value_name = "TARGET")]
        file: String,
    },
    /// Open a document in `$EDITOR` and, on save, recompute its content checksum
    /// (under the `full` fixity tier) so a body edit keeps its fixity true rather
    /// than becoming a `check` finding. The prov-mediated edit path.
    Edit {
        /// The document to edit: a path, a title route (`@Daily/2026/07`), or an
        /// id (`id:fpk38j`).
        #[arg(value_name = "TARGET")]
        file: String,
    },
    /// Set a metadata field (comment- and format-preserving; creates the
    /// block when the document has none).
    Set {
        /// Path to a document.
        #[arg(value_name = "TARGET")]
        file: String,
        /// Dotted key path.
        key: String,
        /// Value; `true`/`false`, integers, floats, and `null` are typed,
        /// everything else is a string.
        value: String,
    },
    /// Remove a metadata field (comment- and format-preserving).
    Unset {
        /// Path to a document.
        #[arg(value_name = "TARGET")]
        file: String,
        /// Dotted key path.
        key: String,
    },
    /// Print the containment tree that unfolds from a root document.
    Tree {
        /// The document to discover from (default: the workspace root).
        #[arg(value_name = "TARGET")]
        root: Option<String>,
    },
    /// Interactively explore the workspace: view a document and follow any of its
    /// links — or its backlinks — moving through the graph from the terminal.
    Explore {
        /// The document to start from (default: the workspace root).
        file: Option<PathBuf>,
    },
    /// Check workspace integrity from a root: broken links, case mismatches,
    /// duplicate containment, missing inverse links, dangling IDs. Exits 1 on
    /// findings.
    Check {
        /// The document to check from (default: the workspace root).
        #[arg(value_name = "TARGET")]
        root: Option<String>,
        /// Interactively repair fixable findings (currently: missing inverse
        /// links). Metadata edits only — body-link findings are left for a
        /// structure-aware pass, so code that looks like a link is never touched.
        #[arg(long)]
        fix: bool,
    },
    /// Create a document as a child of a parent, linking both directions. The
    /// positional is the new document's **title** — prov derives a readable
    /// filename from it (a slug plus the workspace's content extension) in the
    /// parent's directory, and records the title in the document's metadata,
    /// where structure lives. Override the derived filename with `--as` (an exact
    /// path) or just its extension with `--ext`.
    New {
        /// Title of the new document (recorded in its metadata; a readable
        /// filename is slugged from it unless `--as` overrides).
        title: String,
        /// The parent document that gains a spanning link to the new one: a path
        /// (`daily.md`), a title route (`@Daily/2026/07`), or an id
        /// (`id:fpk38j`). A route's missing segments are an error unless `-p`.
        #[arg(long = "in", short = 'i', value_name = "TARGET")]
        in_target: String,
        /// `mkdir -p` for containment — idempotent creation. Creates any missing
        /// route segments (when `--in` is a route), *and* treats an
        /// already-existing leaf (a same-titled child) as a no-op instead of an
        /// error. Safe to re-run — a daily-note cron can call the same command
        /// every day. A path held by a *different*-titled document still errors.
        #[arg(long = "parents", short = 'p', requires = "in_target")]
        parents: bool,
        /// Where `-p` writes the nodes it creates: `nested` (a directory per
        /// segment, `daily/2026/index.md`) or `flat` (all beside the start,
        /// `daily.md`, `2026.md`). File placement only — containment is the links
        /// either way (default: nested).
        #[arg(long, value_enum, default_value_t = LayoutArg::Nested, requires = "parents")]
        layout: LayoutArg,
        /// Print what `--in` resolves to and what `-p` would create, then stop.
        #[arg(long, requires = "in_target")]
        dry_run: bool,
        /// Use this exact workspace path instead of a title-derived name (the
        /// title is still taken from the positional). Wins over `--ext`.
        #[arg(long = "as")]
        as_path: Option<PathBuf>,
        /// Override just the derived filename's extension (e.g. `djot`, `yaml`);
        /// ignored under `--as`. Default: the workspace's content format.
        #[arg(long)]
        ext: Option<String>,
    },
    /// Give an arbitrary file (an image, a PDF, any binary) workspace-linked
    /// metadata: write a sidecar `<file>.yaml` beside it carrying its title,
    /// links, and any ID, and link it as a child of a parent. The file's bytes
    /// are never read or rewritten — only linked, moved, and validated with it.
    Attach {
        /// The file to attach. Anything prov can't read as a document; a
        /// readable document should be created with `new` (it carries its own
        /// metadata) rather than shadowed by a sidecar. Omit with `--all`.
        payload: Option<PathBuf>,
        /// The parent that gains a spanning link to the attachment (default: the
        /// workspace root): a path (`daily.md`), a title route
        /// (`@Daily/2026/07`), or an id (`id:fpk38j`).
        #[arg(long = "in", short = 'i', value_name = "TARGET")]
        in_target: Option<String>,
        /// Create any route segments that don't exist yet — `mkdir -p` for
        /// containment. Only meaningful when `--in` is a route.
        #[arg(long = "parents", short = 'p', requires = "in_target")]
        parents: bool,
        /// Where `-p` writes the nodes it creates. File placement only.
        #[arg(long, value_enum, default_value_t = LayoutArg::Nested, requires = "parents")]
        layout: LayoutArg,
        /// Attach every loose file under the workspace — each opaque file that
        /// has no sidecar yet — instead of a single payload. Bounded to the
        /// directories the workspace already reaches (an unlinked subtree, a
        /// nested workspace, is left alone); pass `--recursive` to sweep the whole
        /// tree. Mutually exclusive with a positional file.
        #[arg(long)]
        all: bool,
        /// With `--all`, descend into every directory, including ones nothing
        /// links to yet — the full recursive sweep rather than the reachability-
        /// bounded default.
        #[arg(long)]
        recursive: bool,
    },
    /// Move/rename a document, maintaining every affected link: every inbound
    /// reference across the workspace (parent entry, children's inverses,
    /// overlay links, body wikilinks) and the document's own relative links.
    ///
    /// Moves the *file* and preserves the document's place in the tree. To change
    /// its place in the tree instead, see `reparent` — or pass `--in`
    /// here to do both at once.
    Mv {
        /// The document to move: a path, a title route (`@Daily/2026/07`), or an
        /// id (`id:fpk38j`).
        #[arg(value_name = "TARGET")]
        from: String,
        /// New path.
        to: PathBuf,
        /// Also reparent under this document — the file moves *and* changes
        /// parent. A path, a title route (`@Daily/2026/08`), or an id.
        #[arg(long = "in", short = 'i', value_name = "TARGET")]
        in_target: Option<String>,
        /// Create missing route segments (when `--in` is a route), like `mkdir -p`.
        #[arg(long = "parents", short = 'p', requires = "in_target")]
        parents: bool,
        /// Where `--parents` writes the nodes it synthesizes. Placement only —
        /// never the graph.
        #[arg(long, value_enum, default_value_t = LayoutArg::Nested, requires = "parents")]
        layout: LayoutArg,
    },
    /// Change a document's parent in the containment tree, leaving the file where
    /// it is.
    ///
    /// The complement of `mv`: `mv` changes a document's path and preserves its
    /// place in the tree; `reparent` changes its place in the tree and preserves
    /// its path. Containment is link-shaped, not directory-shaped, so a node may
    /// live in any directory — moving the file is a separate decision (`mv`, or
    /// `mv --in` to do both).
    ///
    /// The old parent's entry is removed and the new one's added, so the document
    /// is never contained twice. An unparented document is accepted: there is
    /// nothing to remove, so this simply links it in.
    Reparent {
        /// The document to reparent: a path, a title route (`@Daily/2026/07`), or
        /// an id (`id:fpk38j`).
        #[arg(value_name = "TARGET")]
        path: String,
        /// The new parent: a path (`daily.md`), a title route
        /// (`@Daily/2026/08`), or an id (`id:fpk38j`).
        #[arg(long = "in", short = 'i', value_name = "TARGET")]
        in_target: String,
        /// Create missing route segments (when `--in` is a route), like `mkdir -p`.
        #[arg(long = "parents", short = 'p', requires = "in_target")]
        parents: bool,
        /// Where `--parents` writes the nodes it synthesizes. Placement only —
        /// never the graph.
        #[arg(long, value_enum, default_value_t = LayoutArg::Nested, requires = "parents")]
        layout: LayoutArg,
        /// Show what the route resolves to without changing anything.
        #[arg(long, requires = "in_target")]
        dry_run: bool,
    },
    /// Delete a document, removing its parent's spanning entry. Refuses when
    /// the document has children unless --force. By default the document is moved
    /// to the workspace recycle bin (recoverable with `restore`); pass `--purge`
    /// for an immediate hard delete. The default is governed by the `recycle_bin`
    /// config axis (on unless opted out).
    Rm {
        /// The document to delete: a path, a title route (`@Daily/2026/07`), or an
        /// id (`id:fpk38j`).
        #[arg(value_name = "TARGET")]
        path: String,
        /// Delete even when the document still contains children (orphans them).
        #[arg(long)]
        force: bool,
        /// Hard-delete: destroy the document instead of moving it to the recycle
        /// bin. Irreversible.
        #[arg(long)]
        purge: bool,
    },
    /// Restore a document from the recycle bin to the path it was deleted from,
    /// re-linking it under its original parent.
    Restore {
        /// The original path of a binned document (as listed in the recycle bin).
        #[arg(value_name = "PATH")]
        path: String,
    },
    /// Permanently purge every document in the recycle bin. Irreversible; the
    /// only hard delete of binned documents.
    EmptyBin,
    /// Convert a document along a config axis, in place. Four axes are supported.
    /// Two restyle the document's own outbound path links: `notation` (how a
    /// target is wrapped — `markdown` `[Title](target)` or `bare` `target`) and
    /// `path_style` (how the path itself is written — `root` / `relative` /
    /// `canonical`) — only the spelling changes; each link's destination, label,
    /// and wrapper are preserved, and id/external/alias targets are left untouched.
    /// The other two rewrite the metadata block: `metadata.format` re-emits the
    /// frontmatter in a different language (`yaml` / `json` / `toml` / `fig`),
    /// keeping its embedding shape; `metadata.embed` re-emits it in a different
    /// shape (`delimited` / `code_block` / `html_script` / `html_code`), keeping its
    /// language — so a `delimited` block can become a code block that can then hold
    /// fig. Both preserve every value (comments do not survive a block rewrite).
    /// Per file by default (DESIGN §8) — a document's spelling is its own to
    /// declare; `-r` also converts this file's spanning subtree.
    Convert {
        /// The document to convert.
        #[arg(value_name = "TARGET")]
        file: String,
        /// The config axis to convert: `notation`, `path_style`, `metadata.format`,
        /// or `metadata.embed`.
        axis: String,
        /// The target value (e.g. `bare` for `notation`, `relative` for
        /// `path_style`, `json` for `metadata.format`, `code_block` for
        /// `metadata.embed`).
        value: String,
        /// Also convert every document in this file's spanning subtree.
        #[arg(long, short)]
        recursive: bool,
    },
    /// Duplicate a document as a fresh sibling under the same parent, linking the
    /// copy in both directions. The copy takes the next free `-copy` name and
    /// carries the source's title, body, and metadata — but never its stable ID
    /// (identity is per-document) nor its children (a shallow copy, so no child is
    /// left with two parents). A separated node's body file is copied too.
    #[command(alias = "dup")]
    Duplicate {
        /// The document to duplicate: a path, a title route (`@Daily/2026/07`), or
        /// an id (`id:fpk38j`).
        #[arg(value_name = "TARGET")]
        source: String,
    },
    /// Ensure a document has a stable ID and print its `prov:<id>` target.
    /// Registers it in the workspace's registry document (bootstrapping
    /// registry.yaml + the root's `registry` pointer on first use) — link that
    /// target from any document and it survives moves.
    Id {
        /// Path to a document.
        #[arg(value_name = "TARGET")]
        file: String,
    },
    /// Resolve a stable ID (with or without the `prov:` prefix) to its
    /// current path.
    Resolve {
        /// The ID to resolve.
        id: String,
    },
    /// List the documents that link to a document (its backlinks), across the
    /// workspace, as `source<TAB>site<TAB>path|id`.
    Backlinks {
        /// The document whose backlinks to list.
        #[arg(value_name = "TARGET")]
        file: String,
    },
    /// Get or set workspace config (e.g. `link_format`, `identity`). With a
    /// value, writes it to the linked config document — creating and linking
    /// `prov.yaml` from the root on first use. With a key only, prints that
    /// value; with no key, prints the effective config.
    Config {
        /// The config key — dotted for nested axes (e.g. `references.notation`,
        /// `identity`). Omit to print the effective config.
        key: Option<String>,
        /// The value to set. Omit to read.
        value: Option<String>,
        /// Materialize the *full* effective config explicitly into the config
        /// document — every setting written out at its current (or default)
        /// value, so nothing relies on invisible defaults. Fills in the keys you
        /// have not set; existing settings and fields are preserved.
        #[arg(long, conflicts_with_all = ["key", "value", "home"])]
        setup: bool,
        /// Relocate the whole workspace policy to one home, preserving what is
        /// declared (no defaults baked in): `sidecar` moves it into `prov.yaml`
        /// and clears the root's `prov:` block ("unclutter my root"); `root`
        /// inlines it into the root's `prov:` block and removes the sidecar ("one
        /// less file"). Reading always spans both homes regardless of where policy
        /// lives.
        #[arg(long, value_name = "root|sidecar", conflicts_with_all = ["key", "value", "setup"])]
        home: Option<ConfigHome>,
    },
}

/// Which home the `config --home` conversion relocates workspace policy to. The
/// two homes read identically (DESIGN §2, "two homes, one vocabulary"); this only
/// chooses *where the bytes live*.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ConfigHome {
    /// Inline in the root document's `prov:` block; the sidecar is removed.
    Root,
    /// In the dedicated `prov.yaml` config document; the root's block is cleared.
    Sidecar,
}

/// CLI spelling of the metadata formats prov compiles in. Variants track the
/// crate's format features: YAML is always available; JSON and the native fig
/// dialect appear only when their features are enabled, so `--format` never
/// offers a format whose parser is not in the binary.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum MetaFormat {
    Yaml,
    #[cfg(feature = "toml")]
    Toml,
    #[cfg(feature = "json")]
    Json,
    #[cfg(feature = "fig-lang")]
    Fig,
}

impl MetaFormat {
    /// The lowercase spelling for the `init` summary line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            MetaFormat::Yaml => "yaml",
            #[cfg(feature = "toml")]
            MetaFormat::Toml => "toml",
            #[cfg(feature = "json")]
            MetaFormat::Json => "json",
            #[cfg(feature = "fig-lang")]
            MetaFormat::Fig => "fig",
        }
    }
}

/// CLI spelling of the body-prose grammars `twig` parses. Unlike the metadata
/// formats these are always available (twig is a required dependency), so no
/// variant is feature-gated.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ContentLang {
    Markdown,
    Djot,
    Html,
}

impl ContentLang {
    /// The root document's file extension for this grammar.
    pub(crate) fn ext(self) -> &'static str {
        match self {
            ContentLang::Markdown => "md",
            ContentLang::Djot => "dj",
            ContentLang::Html => "html",
        }
    }

    /// A title heading in this grammar — the seed body of the root document.
    pub(crate) fn heading(self, title: &str) -> String {
        match self {
            // Markdown and Djot share ATX heading syntax.
            ContentLang::Markdown | ContentLang::Djot => format!("# {title}\n"),
            ContentLang::Html => format!("<h1>{title}</h1>\n"),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            ContentLang::Markdown => "markdown",
            ContentLang::Djot => "djot",
            ContentLang::Html => "html",
        }
    }

    /// The embed styles `init` offers for this grammar, in menu order (the first
    /// is the default). Markdown gets delimiters, a fenced block, or a separate
    /// sidecar; Djot drops delimiters (it has no idiomatic frontmatter, and a
    /// leading `---`/`+++` is body syntax) and offers a fenced block or separate;
    /// HTML offers the two data-island shapes; every grammar can keep metadata
    /// in a sibling file.
    pub(crate) fn embed_styles(self) -> &'static [EmbedStyle] {
        match self {
            ContentLang::Markdown => &[
                EmbedStyle::Delimited,
                EmbedStyle::CodeBlock,
                EmbedStyle::Separate,
            ],
            ContentLang::Djot => &[EmbedStyle::CodeBlock, EmbedStyle::Separate],
            ContentLang::Html => &[
                EmbedStyle::HtmlScript,
                EmbedStyle::HtmlCode,
                EmbedStyle::Separate,
            ],
        }
    }

    /// Whether `style` is a sensible embed for this content grammar — the
    /// validity check the `--embed` flag is held to (the interactive menu only
    /// ever offers valid styles).
    pub(crate) fn allows_embed(self, style: EmbedStyle) -> bool {
        self.embed_styles().contains(&style)
    }
}

/// A menu label + hint for an embed style — the `init` "Embed type" prompt and
/// the summary line's spelling.
pub(crate) fn embed_labels(style: EmbedStyle) -> (&'static str, &'static str) {
    match style {
        EmbedStyle::Delimited => ("Character delimiters", "--- yaml · +++ toml · ;;; json"),
        EmbedStyle::CodeBlock => ("Typed code block", "```yaml · ```toml · ```fig"),
        EmbedStyle::HtmlScript => ("Script tag", "<script type=\"application/…\">"),
        EmbedStyle::HtmlCode => ("Code tag", "<pre><code class=\"language-…\">"),
        EmbedStyle::Separate => ("Separate", "metadata in a sibling file"),
    }
}

/// The config languages `init` offers for `embed`, compiled-in only. YAML is
/// always present; TOML/JSON/fig follow their crate features. The fig dialect
/// has no character-delimiter form, so it is dropped for [`EmbedStyle::Delimited`].
pub(crate) fn config_languages(embed: EmbedStyle) -> Vec<(MetaFormat, &'static str)> {
    let _ = embed; // read below only under the `fig-lang` feature
    let mut opts = vec![(MetaFormat::Yaml, "YAML")];
    #[cfg(feature = "toml")]
    opts.push((MetaFormat::Toml, "TOML"));
    #[cfg(feature = "json")]
    opts.push((MetaFormat::Json, "JSON"));
    #[cfg(feature = "fig-lang")]
    if embed != EmbedStyle::Delimited {
        opts.push((MetaFormat::Fig, "fig"));
    }
    opts
}

impl From<ContentLang> for ContentFormat {
    fn from(c: ContentLang) -> Self {
        match c {
            ContentLang::Markdown => ContentFormat::Markdown,
            ContentLang::Djot => ContentFormat::Djot,
            ContentLang::Html => ContentFormat::Html,
        }
    }
}

/// CLI spelling of the metadata *embed type* ([`prov::EmbedStyle`]) — how
/// the metadata is carried in (or beside) the document, one level above the
/// config language. Which styles make sense depends on the content grammar (see
/// [`ContentLang::embed_styles`]); the `--embed` flag accepts any and is
/// validated against the chosen content.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum EmbedArg {
    /// Character-delimited frontmatter (`---`/`+++`/`;;;`). Markdown only.
    Delimited,
    /// A typed fenced code block (```` ```yaml ````, ```` ```fig ````, …).
    CodeBlock,
    /// An HTML `<script type="application/…">` data island. HTML only.
    HtmlScript,
    /// An HTML `<pre><code class="language-…">` block. HTML only.
    HtmlCode,
    /// Metadata in a sibling whole-file document, linked by `content`.
    Separate,
}

impl From<EmbedArg> for EmbedStyle {
    fn from(e: EmbedArg) -> Self {
        match e {
            EmbedArg::Delimited => EmbedStyle::Delimited,
            EmbedArg::CodeBlock => EmbedStyle::CodeBlock,
            EmbedArg::HtmlScript => EmbedStyle::HtmlScript,
            EmbedArg::HtmlCode => EmbedStyle::HtmlCode,
            EmbedArg::Separate => EmbedStyle::Separate,
        }
    }
}

/// CLI spelling of the workspace link styles ([`prov::LinkStyle`]).
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum LinkStyleArg {
    MarkdownRoot,
    MarkdownRelative,
    PlainRelative,
    PlainCanonical,
}

impl From<LinkStyleArg> for LinkStyle {
    fn from(l: LinkStyleArg) -> Self {
        match l {
            LinkStyleArg::MarkdownRoot => LinkStyle::MarkdownRoot,
            LinkStyleArg::MarkdownRelative => LinkStyle::MarkdownRelative,
            LinkStyleArg::PlainRelative => LinkStyle::PlainRelative,
            LinkStyleArg::PlainCanonical => LinkStyle::PlainCanonical,
        }
    }
}

/// When a document earns a stable ID — the `identity` config key, one of the
/// two independent identity axes `init` asks about. `Off` is paths-only; `Lazy`
/// mints on a durable reference (link-by-id or publish); `Eager` mints every
/// document at creation. The spellings match the config value ([`registration_from_str`]).
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum IdentityArg {
    /// Paths only — no document ever earns a stable ID. `none` is accepted as a
    /// synonym, matching the canonical `identity: none` config spelling.
    #[value(alias = "none")]
    Off,
    Lazy,
    Eager,
}

impl IdentityArg {
    /// The registration trigger set this identity policy selects.
    pub(crate) fn registration(self) -> Registration {
        match self {
            IdentityArg::Off => Registration::OFF,
            IdentityArg::Lazy => Registration::LAZY,
            IdentityArg::Eager => Registration::EAGER,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            IdentityArg::Off => "off",
            IdentityArg::Lazy => "lazy",
            IdentityArg::Eager => "eager",
        }
    }
}

/// Where a document's stable ID is stored — the `id_storage` config key
/// ([`IdStorage`]). `Registry` is the current default (IDs only in the registry);
/// `Frontmatter` also stamps each document's own `id` field (a portable,
/// self-describing shadow, registry kept as a cache); `FrontmatterOnly` drops the
/// registry entirely (self-describing, but no tombstones). `init` offers the
/// first two; the third is deliberately flag-only.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum IdStorageArg {
    Registry,
    Frontmatter,
    FrontmatterOnly,
}

impl From<IdStorageArg> for IdStorage {
    fn from(s: IdStorageArg) -> Self {
        match s {
            IdStorageArg::Registry => IdStorage::Registry,
            IdStorageArg::Frontmatter => IdStorage::Frontmatter,
            IdStorageArg::FrontmatterOnly => IdStorage::FrontmatterOnly,
        }
    }
}

impl IdStorageArg {
    /// The lowercase spelling for the `init` summary line.
    pub(crate) fn label(self) -> &'static str {
        IdStorage::from(self).as_config_str()
    }
}

/// How far content-checksum (fixity) coverage extends — the `fixity` config key
/// ([`prov::Fixity`]). `Payloads` (the default) checksums attachment payloads
/// only — frictionless, since a payload is never edited; `Full` also checksums
/// document bodies (pair with `prov edit`); `Off` records nothing.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum FixityArg {
    Off,
    Payloads,
    Full,
}

impl From<FixityArg> for prov::Fixity {
    fn from(f: FixityArg) -> Self {
        match f {
            FixityArg::Off => prov::Fixity::Off,
            FixityArg::Payloads => prov::Fixity::Payloads,
            FixityArg::Full => prov::Fixity::Full,
        }
    }
}

impl FixityArg {
    /// The lowercase spelling for the `init` summary line.
    pub(crate) fn label(self) -> &'static str {
        prov::Fixity::from(self).as_config_str()
    }
}

/// What `init` does with content documents already present in the directory
/// (`docs/init-adoption.md`). `Flat` (Phase 1) links each loose file directly
/// under the new root; `Mirror` (Phase 2) folds the directory tree into the
/// containment tree — every directory becomes a node, synthesizing a folder-note
/// index where none exists; `None_` initializes but leaves them unlinked.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum AdoptArg {
    Flat,
    #[value(name = "none")]
    None_,
    Mirror,
}

/// The syntactic wrapper `init` authors references in — the *first* style axis
/// (`--wrapper`), chosen before the addressing (see `docs/reference-styles.md`,
/// "pick the wrapper first, then the substyle").
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum WrapperArg {
    /// The diaryx/CommonMark form: `[Title](target)` (or bare).
    Markdown,
    /// The Obsidian form: `[[target]]` / `[[target|Title]]`.
    Wikilink,
}

impl From<WrapperArg> for Wrapper {
    fn from(w: WrapperArg) -> Self {
        match w {
            WrapperArg::Markdown => Wrapper::Markdown,
            WrapperArg::Wikilink => Wrapper::Wikilink,
        }
    }
}

/// What the references `init` authors address their target *by* — the *second*
/// style axis (`--reference`), the addressing. `Path` is readable but rewritten
/// on move; `Id` is durable and registers its target (so it needs identity);
/// `Alias` is by title (readable, never move-safe, never registers); `Split`
/// sets *different* addressing for the two spanning directions (the diaryx up≠down
/// shape). The wrapper is chosen separately ([`WrapperArg`]).
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ReferenceArg {
    /// By path — rewritten when a file moves. Rendering follows `--link-style`.
    Path,
    /// By durable `id:<id>` handle — survives moves untouched, registers the target.
    Id,
    /// By the target's title — readable, but not move/rename-safe (implies wikilink).
    Alias,
    /// Readable *down*, durable *up*: `contents` by alias, `part_of` by id.
    Split,
}

impl ReferenceArg {
    /// Whether this addressing registers targets (link-by-id), so it needs
    /// identity to mint IDs. `Path` and `Alias` never register.
    pub(crate) fn needs_identity(self) -> bool {
        matches!(self, ReferenceArg::Id | ReferenceArg::Split)
    }

    /// Whether a by-path reference is (possibly) authored, so `init` asks the
    /// path-format question. Only `Path` addresses by path.
    pub(crate) fn uses_path(self) -> bool {
        self == ReferenceArg::Path
    }

    /// The lowercase spelling for the `init` summary line and `--reference` flag.
    pub(crate) fn label(self) -> &'static str {
        match self {
            ReferenceArg::Path => "path",
            ReferenceArg::Id => "id",
            ReferenceArg::Alias => "alias",
            ReferenceArg::Split => "split (alias down, id up)",
        }
    }

    /// The `--reference` flag value (kebab-case), for diagnostics.
    pub(crate) fn flag(self) -> &'static str {
        match self {
            ReferenceArg::Split => "split",
            other => other.label(),
        }
    }

    /// Write the addressing axis and per-relation overrides this reference choice
    /// encodes onto `config`. The workspace `notation`/`path_style` are already set
    /// (from the wrapper + path-format prompts); this only touches `target`,
    /// `label`, and the split relations — except `alias`, which forces wikilink.
    pub(crate) fn write_onto(self, config: &mut WorkspaceConfig) {
        // Author id links *labeled* — `[Title](id:…)` for markdown, `[[id:…|Title]]`
        // for wikilink — so a durable reference stays readable, and clickable with
        // graceful degradation (an `id:` scheme link resolves in tools that know it,
        // and says "unsupported scheme" in those that don't), rather than an opaque
        // bare id. The label is a maintained cache of the target's title.
        let id_label = true;
        match self {
            // Path addressing is the default; notation/path_style already carry it.
            ReferenceArg::Path => {}
            ReferenceArg::Id => {
                config.reference_target = Addressing::Id;
                config.reference_label = id_label;
            }
            ReferenceArg::Alias => {
                // Alias has no markdown/bare spelling; it always renders wikilink.
                config.notation = Notation::Wikilink;
                config.reference_target = Addressing::Alias;
            }
            // Durable id by default (overlay relations like `links` stay
            // move-stable), then the two spanning directions diverge: a readable
            // alias going down, an id link going up in the workspace notation.
            ReferenceArg::Split => {
                config.reference_target = Addressing::Id;
                config.reference_label = id_label;
                config.relation_styles.insert(
                    "contents".into(),
                    RelationStyleConfig {
                        notation: Some(Notation::Wikilink),
                        path_style: None,
                        target: Some(Addressing::Alias),
                        label: None,
                    },
                );
                config.relation_styles.insert(
                    "part_of".into(),
                    RelationStyleConfig {
                        notation: None, // inherit the workspace notation
                        path_style: None,
                        target: Some(Addressing::Id),
                        label: Some(id_label),
                    },
                );
            }
        }
    }
}

impl From<MetaFormat> for Format {
    fn from(f: MetaFormat) -> Format {
        match f {
            MetaFormat::Yaml => Format::Yaml,
            #[cfg(feature = "toml")]
            MetaFormat::Toml => Format::Toml,
            #[cfg(feature = "json")]
            MetaFormat::Json => Format::Json,
            #[cfg(feature = "fig-lang")]
            MetaFormat::Fig => Format::Fig,
        }
    }
}
