//! `colophon` — command-line companion for the colophon library.
//!
//! A thin adapter: parse arguments, call into the library, render the result.
//! All logic lives in `colophon`; this crate is I/O and presentation only.
//!
//! Single-document commands (`show`, `links`, `meta`, `get`, `body`, `set`,
//! `unset`) operate on the pure layers. Workspace commands (`tree`, `check`,
//! `new`, `mv`, `rm`) drive the library's [`colophon::StdFs`]-backed engine,
//! rooted at the current directory, through the dependency-free
//! [`colophon::block_on`] executor.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use colophon::tree::{Node, NodeKind};
use colophon::document::MetaCarrier;
use colophon::{
    Addressing, Adoption, ContentFormat, Document, EmbedStyle, FileIndex, Format, Id, IdStorage,
    IndexStore, LinkStyle, Mapping, Minter, Registration, RelationStyleConfig, RelationSet, StdFs,
    StructurePlan, SynthNode, Target, Trigger, Value, Workspace, WorkspaceConfig, Wrapper, block_on,
    edit, link, meta,
};

/// The filename stem of the registry document the CLI creates on first
/// `colophon id` — visible, beside the root, and *linked from the root's own
/// metadata* via the `registry` relation. Its extension is the workspace's
/// metadata format (see [`sidecar_name`]). Where the registry lives is a fact
/// about the workspace, declared in it; the CLI only supplies this default when
/// bootstrapping one. (It can equally be a `.md` file whose frontmatter carries
/// the records — anything the pointer targets.)
const REGISTRY_STEM: &str = "registry";

/// The filename stem of the config document the CLI creates on first
/// `colophon config <k> <v>` (or at `init`) — beside the root, linked via the
/// `config` relation (the reachability move the registry uses). Workspace policy
/// lives here rather than bloating the root or hiding in a dotfile.
const CONFIG_STEM: &str = "colophon";

/// The whole-file extension for a metadata format: the config and registry
/// sidecars are written in the workspace's *chosen metadata format*, not always
/// YAML — `yaml`/`json`/`figl`. Mirrors [`colophon::document::whole_file_format`],
/// which parses them back.
fn sidecar_ext(format: Format) -> &'static str {
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

/// The sidecar filename for `stem` in metadata `format` (e.g. `colophon.figl`).
fn sidecar_name(stem: &str, format: Format) -> String {
    format!("{stem}.{}", sidecar_ext(format))
}

/// A self-describing plaintext workspace, from the command line.
#[derive(Parser)]
#[command(name = "colophon", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
        /// The syntactic wrapper colophon authors references in: markdown
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
        /// invisible to `colophon check` until attached).
        #[arg(long)]
        attach: bool,
        /// Accept every default without prompting.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Summarize a document: its metadata, spanning children, and declared links.
    Show {
        /// Path to a document (plaintext with embedded metadata).
        file: PathBuf,
    },
    /// List a document's links as `relation<TAB>target`, one per line.
    Links {
        /// Path to a document.
        file: PathBuf,
        /// Only show links declared by this relation (e.g. `contents`).
        #[arg(long)]
        relation: Option<String>,
    },
    /// Print a document's metadata block (without fences).
    Meta {
        /// Path to a document.
        file: PathBuf,
        /// Output format (default: the format the document already uses).
        #[arg(long, value_enum)]
        format: Option<MetaFormat>,
    },
    /// Print one metadata field by dotted path (e.g. `title`, `contents.0`).
    Get {
        /// Path to a document.
        file: PathBuf,
        /// Dotted key path; an all-digit segment indexes a sequence.
        key: String,
    },
    /// Print a document's body (everything outside the metadata block).
    Body {
        /// Path to a document.
        file: PathBuf,
    },
    /// Render a document's body to HTML (Markdown/Djot, via `twig`).
    Render {
        /// Path to a document.
        file: PathBuf,
    },
    /// Set a metadata field (comment- and format-preserving; creates the
    /// block when the document has none).
    Set {
        /// Path to a document.
        file: PathBuf,
        /// Dotted key path.
        key: String,
        /// Value; `true`/`false`, integers, floats, and `null` are typed,
        /// everything else is a string.
        value: String,
    },
    /// Remove a metadata field (comment- and format-preserving).
    Unset {
        /// Path to a document.
        file: PathBuf,
        /// Dotted key path.
        key: String,
    },
    /// Print the containment tree that unfolds from a root document.
    Tree {
        /// The document to discover from (default: the workspace root).
        root: Option<PathBuf>,
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
        root: Option<PathBuf>,
        /// Interactively repair fixable findings (currently: missing inverse
        /// links). Metadata edits only — body-link findings are left for a
        /// structure-aware pass, so code that looks like a link is never touched.
        #[arg(long)]
        fix: bool,
    },
    /// Create a document as a child of a parent, linking both directions.
    New {
        /// Path of the document to create.
        path: PathBuf,
        /// The parent document (gains a spanning link to the new file).
        #[arg(long, short)]
        parent: PathBuf,
    },
    /// Give an arbitrary file (an image, a PDF, any binary) workspace-linked
    /// metadata: write a sidecar `<file>.yaml` beside it carrying its title,
    /// links, and any ID, and link it as a child of a parent. The file's bytes
    /// are never read or rewritten — only linked, moved, and validated with it.
    Attach {
        /// The file to attach. Anything colophon can't read as a document; a
        /// readable document should be created with `new` (it carries its own
        /// metadata) rather than shadowed by a sidecar. Omit with `--all`.
        payload: Option<PathBuf>,
        /// The parent document that gains a spanning link to the attachment
        /// (default: the workspace root).
        #[arg(long, short)]
        parent: Option<PathBuf>,
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
    Mv {
        /// Current path.
        from: PathBuf,
        /// New path.
        to: PathBuf,
    },
    /// Delete a document, removing its parent's spanning entry. Refuses when
    /// the document has children unless --force.
    Rm {
        /// Path of the document to delete.
        path: PathBuf,
        /// Delete even when the document still contains children (orphans them).
        #[arg(long)]
        force: bool,
    },
    /// Ensure a document has a stable ID and print its `colophon:<id>` target.
    /// Registers it in the workspace's registry document (bootstrapping
    /// registry.yaml + the root's `registry` pointer on first use) — link that
    /// target from any document and it survives moves.
    Id {
        /// Path to a document.
        file: PathBuf,
    },
    /// Resolve a stable ID (with or without the `colophon:` prefix) to its
    /// current path.
    Resolve {
        /// The ID to resolve.
        id: String,
    },
    /// List the documents that link to a document (its backlinks), across the
    /// workspace, as `source<TAB>site<TAB>path|id`.
    Backlinks {
        /// The document whose backlinks to list.
        file: PathBuf,
    },
    /// Get or set workspace config (e.g. `link_format`, `identity`). With a
    /// value, writes it to the linked config document — creating and linking
    /// `colophon.yaml` from the root on first use. With a key only, prints that
    /// value; with no key, prints the effective config.
    Config {
        /// The config key (e.g. `link_format`, `identity`). Omit to print all.
        key: Option<String>,
        /// The value to set. Omit to read.
        value: Option<String>,
    },
}

/// CLI spelling of the metadata formats colophon compiles in. Variants track the
/// crate's format features: YAML is always available; JSON and the native fig
/// dialect appear only when their features are enabled, so `--format` never
/// offers a format whose parser is not in the binary.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MetaFormat {
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
    fn label(self) -> &'static str {
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
enum ContentLang {
    Markdown,
    Djot,
    Html,
}

impl ContentLang {
    /// The root document's file extension for this grammar.
    fn ext(self) -> &'static str {
        match self {
            ContentLang::Markdown => "md",
            ContentLang::Djot => "dj",
            ContentLang::Html => "html",
        }
    }

    /// A title heading in this grammar — the seed body of the root document.
    fn heading(self, title: &str) -> String {
        match self {
            // Markdown and Djot share ATX heading syntax.
            ContentLang::Markdown | ContentLang::Djot => format!("# {title}\n"),
            ContentLang::Html => format!("<h1>{title}</h1>\n"),
        }
    }

    fn label(self) -> &'static str {
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
    fn embed_styles(self) -> &'static [EmbedStyle] {
        match self {
            ContentLang::Markdown => {
                &[EmbedStyle::Delimited, EmbedStyle::CodeBlock, EmbedStyle::Separate]
            }
            ContentLang::Djot => &[EmbedStyle::CodeBlock, EmbedStyle::Separate],
            ContentLang::Html => {
                &[EmbedStyle::HtmlScript, EmbedStyle::HtmlCode, EmbedStyle::Separate]
            }
        }
    }

    /// Whether `style` is a sensible embed for this content grammar — the
    /// validity check the `--embed` flag is held to (the interactive menu only
    /// ever offers valid styles).
    fn allows_embed(self, style: EmbedStyle) -> bool {
        self.embed_styles().contains(&style)
    }
}

/// A menu label + hint for an embed style — the `init` "Embed type" prompt and
/// the summary line's spelling.
fn embed_labels(style: EmbedStyle) -> (&'static str, &'static str) {
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
fn config_languages(embed: EmbedStyle) -> Vec<(MetaFormat, &'static str)> {
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

/// CLI spelling of the metadata *embed type* ([`colophon::EmbedStyle`]) — how
/// the metadata is carried in (or beside) the document, one level above the
/// config language. Which styles make sense depends on the content grammar (see
/// [`ContentLang::embed_styles`]); the `--embed` flag accepts any and is
/// validated against the chosen content.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EmbedArg {
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

/// CLI spelling of the workspace link styles ([`colophon::LinkStyle`]).
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LinkStyleArg {
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
enum IdentityArg {
    Off,
    Lazy,
    Eager,
}

impl IdentityArg {
    /// The registration trigger set this identity policy selects.
    fn registration(self) -> Registration {
        match self {
            IdentityArg::Off => Registration::OFF,
            IdentityArg::Lazy => Registration::LAZY,
            IdentityArg::Eager => Registration::EAGER,
        }
    }

    fn label(self) -> &'static str {
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
enum IdStorageArg {
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
    fn label(self) -> &'static str {
        IdStorage::from(self).as_config_str()
    }
}

/// What `init` does with content documents already present in the directory
/// (`docs/init-adoption.md`). `Flat` (Phase 1) links each loose file directly
/// under the new root; `Mirror` (Phase 2) folds the directory tree into the
/// containment tree — every directory becomes a node, synthesizing a folder-note
/// index where none exists; `None_` initializes but leaves them unlinked.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AdoptArg {
    Flat,
    #[value(name = "none")]
    None_,
    Mirror,
}

/// The syntactic wrapper `init` authors references in — the *first* style axis
/// (`--wrapper`), chosen before the addressing (see `docs/reference-styles.md`,
/// "pick the wrapper first, then the substyle").
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum WrapperArg {
    /// The diaryx/CommonMark form: `[Title](target)` (or bare).
    Markdown,
    /// The Obsidian form: `[[target]]` / `[[target|Title]]`.
    Wikilink,
}

impl WrapperArg {
    /// The lowercase spelling for the `init` summary line.
    fn label(self) -> &'static str {
        match self {
            WrapperArg::Markdown => "markdown",
            WrapperArg::Wikilink => "wikilink",
        }
    }
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
enum ReferenceArg {
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
    fn needs_identity(self) -> bool {
        matches!(self, ReferenceArg::Id | ReferenceArg::Split)
    }

    /// Whether a by-path reference is (possibly) authored, so `init` asks the
    /// path-format question. Only `Path` addresses by path.
    fn uses_path(self) -> bool {
        self == ReferenceArg::Path
    }

    /// The lowercase spelling for the `init` summary line and `--reference` flag.
    fn label(self) -> &'static str {
        match self {
            ReferenceArg::Path => "path",
            ReferenceArg::Id => "id",
            ReferenceArg::Alias => "alias",
            ReferenceArg::Split => "split (alias down, id up)",
        }
    }

    /// The `--reference` flag value (kebab-case), for diagnostics.
    fn flag(self) -> &'static str {
        match self {
            ReferenceArg::Split => "split",
            other => other.label(),
        }
    }

    /// Write the workspace-default reference axes and per-relation overrides this
    /// (wrapper, addressing) pair encodes onto `config`. Leaving an axis `None`
    /// preserves the pre-existing derive, so markdown + `Path` writes no new keys
    /// (identical to the pre-reference-style behavior).
    fn write_onto(self, wrapper: Wrapper, config: &mut WorkspaceConfig) {
        // Record the wrapper only when it departs from the markdown default, so a
        // plain markdown workspace keeps a minimal config.
        let wrapper_key = (wrapper == Wrapper::Wikilink).then_some(Wrapper::Wikilink);
        // Author id links *labeled* — `[Title](id:…)` for markdown, `[[id:…|Title]]`
        // for wikilink — so a durable reference stays readable, and clickable with
        // graceful degradation (an `id:` scheme link resolves in tools that know it,
        // and says "unsupported scheme" in those that don't), rather than an opaque
        // bare id. The label is a maintained cache of the target's title.
        let id_label = Some(true);
        match self {
            ReferenceArg::Path => {
                config.reference_wrapper = wrapper_key;
            }
            ReferenceArg::Id => {
                config.reference_wrapper = wrapper_key;
                config.reference_target = Some(Addressing::Id);
                config.reference_label = id_label;
            }
            ReferenceArg::Alias => {
                // Alias has no markdown spelling; it always normalizes to wikilink.
                config.reference_wrapper = Some(Wrapper::Wikilink);
                config.reference_target = Some(Addressing::Alias);
            }
            // Durable id by default (overlay relations like `links` stay
            // move-stable), then the two spanning directions diverge: a readable
            // alias going down, an id link going up in the chosen wrapper.
            ReferenceArg::Split => {
                config.reference_wrapper = wrapper_key;
                config.reference_target = Some(Addressing::Id);
                config.relation_styles.insert(
                    "contents".into(),
                    RelationStyleConfig {
                        wrapper: Some(Wrapper::Wikilink),
                        target: Some(Addressing::Alias),
                        label: None,
                    },
                );
                config.relation_styles.insert(
                    "part_of".into(),
                    RelationStyleConfig {
                        wrapper: Some(wrapper),
                        target: Some(Addressing::Id),
                        label: id_label,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Show { file } => cmd_show(&file),
        Command::Links { file, relation } => cmd_links(&file, relation.as_deref()),
        Command::Meta { file, format } => cmd_meta(&file, format),
        Command::Get { file, key } => cmd_get(&file, &key),
        Command::Body { file } => cmd_body(&file),
        Command::Render { file } => cmd_render(&file),
        Command::Init {
            dir,
            title,
            author,
            meta,
            embed,
            content,
            wrapper,
            reference,
            link_style,
            identity,
            id_storage,
            adopt,
            attach,
            yes,
        } => cmd_init(
            dir.as_deref(),
            title,
            author,
            meta,
            embed,
            content,
            wrapper,
            reference,
            link_style,
            identity,
            id_storage,
            adopt,
            attach,
            yes,
        ),
        Command::Set { file, key, value } => cmd_set(&file, &key, &value),
        Command::Unset { file, key } => cmd_unset(&file, &key),
        Command::Tree { root } => cmd_tree(root.as_deref()),
        Command::Explore { file } => cmd_explore(file.as_deref()),
        Command::Check { root, fix } => cmd_check(root.as_deref(), fix),
        Command::New { path, parent } => cmd_new(&path, &parent),
        Command::Attach { payload, parent, all, recursive } => {
            cmd_attach(payload.as_deref(), parent.as_deref(), all, recursive)
        }
        Command::Mv { from, to } => cmd_mv(&from, &to),
        Command::Rm { path, force } => cmd_rm(&path, force),
        Command::Id { file } => cmd_id(&file),
        Command::Resolve { id } => cmd_resolve(&id),
        Command::Backlinks { file } => cmd_backlinks(&file),
        Command::Config { key, value } => cmd_config(key.as_deref(), value.as_deref()),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("colophon: {err}");
            ExitCode::FAILURE
        }
    }
}

type CmdResult = Result<ExitCode, Box<dyn std::error::Error>>;

/// The relation vocabulary. For now the diaryx preset; configurable vocabularies
/// (and a `--relations` flag) come later.
fn relation_set() -> RelationSet {
    RelationSet::diaryx()
}

/// The discovered workspace context: where the root is, which document is the
/// root, and where the root says the registry lives.
struct Ctx {
    /// Absolute path of the workspace root directory.
    root_dir: PathBuf,
    /// The root document, relative to `root_dir`.
    root_doc: PathBuf,
    /// The registry document the root declares (relative to `root_dir`), if any.
    registry: Option<PathBuf>,
    /// The effective workspace config (root frontmatter overlaid by the linked
    /// config document, over defaults).
    config: WorkspaceConfig,
}

type AnyError = Box<dyn std::error::Error>;

/// Find the workspace root by walking up from the current directory: in each
/// directory, a candidate root is a document (any content grammar — see
/// [`ROOT_EXTS`]) with metadata and no `part_of` (nothing contains it). A file
/// stemmed `index`, then `readme`, wins ties.
fn find_root() -> Result<Ctx, AnyError> {
    let cwd = std::env::current_dir()?;
    for dir in cwd.ancestors() {
        let mut candidates: Vec<String> = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_content_ext = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ROOT_EXTS.contains(&e.to_ascii_lowercase().as_str()));
            // A *separated* root's node is a whole-file metadata document
            // (`index.yaml`, …) rather than a content file. Accept those too, but
            // only under the conventional `index`/`readme` stem — otherwise any
            // stray `.json`/`.yaml`/`.toml` config file in the directory (no
            // `part_of`, a mapping at its root) would masquerade as a root.
            let is_meta_ext = colophon::document::whole_file_format(&path).is_some();
            if !is_content_ext && !is_meta_ext {
                continue;
            }
            if is_meta_ext && !is_content_ext {
                let stem_ok = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.eq_ignore_ascii_case("index") || s.eq_ignore_ascii_case("readme"));
                if !stem_ok {
                    continue;
                }
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(doc) = Document::parse(&path, &text) else { continue };
            if doc.has_meta() && doc.meta.get("part_of").is_none() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    candidates.push(name.to_string());
                }
            }
        }
        // Prefer a file stemmed `index`, then `readme` (any extension); failing
        // that, a lone candidate. Two-plus unnamed candidates are ambiguous.
        let stem_is = |name: &str, want: &str| {
            Path::new(name)
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case(want))
        };
        let chosen = candidates
            .iter()
            .find(|n| stem_is(n, "index"))
            .or_else(|| candidates.iter().find(|n| stem_is(n, "readme")))
            .cloned()
            .or_else(|| (candidates.len() == 1).then(|| candidates[0].clone()));
        match chosen {
            Some(root_doc) => {
                let root_dir = dir.to_path_buf();
                let root_doc = PathBuf::from(root_doc);
                // Ask the root where its registry lives (the pointer relation).
                let probe: Workspace<StdFs> = Workspace::builder(StdFs).root(&root_dir).build();
                let registry = block_on(probe.registry_path(&root_doc))?;
                // Build the effective config: defaults, overlaid by the root
                // frontmatter (diaryx compat, e.g. `link_format`), overlaid by
                // the linked config document (which wins).
                let mut config = WorkspaceConfig::default();
                if let Some(text) = std::fs::read_to_string(root_dir.join(&root_doc)).ok()
                    && let Ok(doc) = Document::parse(&root_doc, &text)
                {
                    config.apply(&doc.meta);
                }
                if let Ok(Some(config_doc)) = block_on(probe.config_path(&root_doc))
                    && let Some(text) = std::fs::read_to_string(root_dir.join(&config_doc)).ok()
                    && let Ok(doc) = Document::parse(&config_doc, &text)
                {
                    config.apply(&doc.meta);
                }
                return Ok(Ctx { root_dir, root_doc, registry, config });
            }
            None if candidates.len() > 1 => {
                return Err(format!(
                    "ambiguous workspace root in {}: {} (rename one, or add part_of)",
                    dir.display(),
                    candidates.join(", ")
                )
                .into());
            }
            None => continue,
        }
    }
    Err("no workspace root found: no ancestor directory has a .md document \
with metadata and no part_of"
        .into())
}

/// The workspace the multi-document commands drive: rooted at the discovered
/// root, a lazy identity policy, and the registry the root declares (an empty
/// in-memory one when the root declares none — see `ensure_registry`).
fn workspace(ctx: &Ctx) -> Result<Workspace<StdFs, Minter, FileIndex>, AnyError> {
    let index = if ctx.config.id_storage == IdStorage::FrontmatterOnly {
        // No registry document: rebuild the id→path map by scanning each file's
        // self-stored `id` field — a flat scan, independent of link resolution.
        let probe: Workspace<StdFs> = Workspace::builder(StdFs).root(&ctx.root_dir).build();
        let mut index = FileIndex::new(ctx.config.default_embed_format);
        for (id, path) in block_on(probe.scan_ids())? {
            index.register(&id, &path);
        }
        // A scanned index reflects on-disk state, so it starts clean.
        index.mark_clean();
        index
    } else {
        match &ctx.registry {
            Some(rel) => {
                let full = ctx.root_dir.join(rel);
                let text = match std::fs::read_to_string(&full) {
                    Ok(text) => text,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(e.into()),
                };
                FileIndex::parse(rel, &text)?
            }
            // No registry declared yet: an empty in-memory one in the workspace's
            // metadata format, so a later bootstrap writes that format.
            None => FileIndex::new(ctx.config.default_embed_format),
        }
    };
    // The relation vocabulary picks up any per-relation `style` overrides the
    // config document declares (up≠down), overlaid on the diaryx default set.
    let relations = RelationSet::diaryx().with_styles(&ctx.config.resolved_relation_styles());
    Ok(Workspace::builder(StdFs)
        .root(&ctx.root_dir)
        .relations(relations)
        .identity(Minter::with(ctx.config.identity, entropy_seed()))
        .index(index)
        .link_style(ctx.config.link_format)
        .id_links(ctx.config.id_links)
        .reference_style(ctx.config.reference_style())
        .default_embed_format(ctx.config.default_embed_format)
        .build())
}

/// Make sure the workspace *declares* a registry, bootstrapping one when it
/// does not: create `registry.<ext>` (in the workspace's metadata format) beside
/// the root (self-described with a title and a part_of back to the root) and add
/// the `registry` pointer to the root's metadata — comment-preservingly, like
/// any other edit.
fn ensure_registry(ctx: &mut Ctx) -> Result<(), AnyError> {
    // Frontmatter-only storage keeps no registry document — IDs live solely in
    // each file's `id` field, so there is nothing to bootstrap or point at.
    if !ctx.config.id_storage.keeps_registry() {
        return Ok(());
    }
    if ctx.registry.is_some() {
        return Ok(());
    }
    let format = ctx.config.default_embed_format;
    let registry_rel = PathBuf::from(sidecar_name(REGISTRY_STEM, format));
    let registry_full = ctx.root_dir.join(&registry_rel);
    if !registry_full.exists() {
        let mut seed = colophon::Mapping::new();
        seed.insert("title".into(), Value::String("ID registry".into()));
        seed.insert(
            "part_of".into(),
            Value::String(ctx.root_doc.to_string_lossy().into_owned()),
        );
        std::fs::write(&registry_full, meta::serialize_mapping(&seed, format)?)?;
    }
    let registry_name = registry_rel.to_string_lossy().into_owned();
    let root_full = ctx.root_dir.join(&ctx.root_doc);
    let text = std::fs::read_to_string(&root_full)?;
    let doc = Document::parse(&ctx.root_doc, &text)?;
    let updated = edit::set_in_text(&text, doc.carrier, "registry", edit::infer_scalar(&registry_name))?;
    std::fs::write(&root_full, updated)?;
    eprintln!("initialized {} (linked from {})", registry_rel.display(), ctx.root_doc.display());
    ctx.registry = Some(registry_rel);
    Ok(())
}

/// Persist the registry when a command changed it, to wherever the root says
/// it lives.
fn save_index(ctx: &Ctx, ws: &mut Workspace<StdFs, Minter, FileIndex>) -> Result<(), AnyError> {
    if !ws.index().is_dirty() {
        return Ok(());
    }
    let Some(rel) = &ctx.registry else {
        return Err("the registry changed but no registry document is declared".into());
    };
    let rendered = ws.index_mut().render()?;
    std::fs::write(ctx.root_dir.join(rel), rendered)?;
    ws.index_mut().mark_clean();
    Ok(())
}

/// Persist a mutation's identity changes according to the workspace's
/// [`IdStorage`] mode: stamp each live ID into its document's `id` frontmatter
/// (frontmatter / frontmatter-only), and write the registry snapshot (registry /
/// frontmatter). Frontmatter-only keeps no registry, so the in-memory index —
/// rebuilt next run by scanning — is simply marked clean.
fn persist(ctx: &Ctx, ws: &mut Workspace<StdFs, Minter, FileIndex>) -> Result<(), AnyError> {
    if ctx.config.id_storage.stamps_frontmatter() {
        stamp_ids(ctx, ws)?;
    }
    if ctx.config.id_storage.keeps_registry() {
        save_index(ctx, ws)?;
    } else {
        // No registry document to write; the id→path map is derived from the
        // frontmatter we just stamped, so discard the dirtiness.
        ws.index_mut().mark_clean();
    }
    Ok(())
}

/// Stamp every live ID into its document's `id` frontmatter field, so the ID
/// travels with the file (DESIGN §5's self-describing shadow). Idempotent: a
/// document already carrying the right ID is left untouched, so this both
/// back-fills a workspace that just switched to frontmatter storage and records
/// freshly-minted IDs. A tombstoned ID has no live path and is skipped.
fn stamp_ids(ctx: &Ctx, ws: &mut Workspace<StdFs, Minter, FileIndex>) -> Result<(), AnyError> {
    let pairs: Vec<(Id, PathBuf)> =
        ws.index().iter().map(|(id, path)| (id.clone(), path.clone())).collect();
    for (id, rel) in pairs {
        let full = ctx.root_dir.join(&rel);
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        let Ok(doc) = Document::parse(&rel, &text) else {
            continue;
        };
        // Already carries this exact ID — nothing to write.
        if doc.meta.get("id").and_then(Value::as_str) == Some(id.0.as_str()) {
            continue;
        }
        let updated = edit::set_in_text(&text, doc.carrier, "id", edit::infer_scalar(&id.0))?;
        std::fs::write(&full, updated)?;
    }
    Ok(())
}

/// Re-anchor a (cwd-relative) CLI path to the discovered workspace root.
fn ws_rel(ctx: &Ctx, path: &Path) -> Result<PathBuf, AnyError> {
    let abs = link::normalize(std::env::current_dir()?.join(path));
    abs.strip_prefix(&ctx.root_dir)
        .map(Path::to_path_buf)
        .map_err(|_| {
            format!(
                "{} is outside the workspace root {}",
                path.display(),
                ctx.root_dir.display()
            )
            .into()
        })
}

/// A seed for the minter from OS-seeded hasher state — dependency-free
/// randomness. (Uniqueness is enforced by rejection against the registry;
/// the seed only needs to differ between runs.)
fn entropy_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::hash::RandomState::new().build_hasher().finish()
}

fn load(file: &Path) -> Result<(String, Document), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(file)?;
    let doc = Document::parse(file, &text)?;
    Ok((text, doc))
}

/// The body-grammar root extensions `init` will not overwrite (every content
/// grammar's `index.*`), mirroring the set `find_root` treats as root candidates.
const ROOT_EXTS: &[&str] = &["md", "markdown", "dj", "djot", "html", "htm"];

/// The whole-file metadata extensions a *separated* root's node can use — the
/// other half of the already-initialized guard, since a separate workspace's
/// root is an `index.<meta-ext>` document, not an `index.<content-ext>` one.
const META_EXTS: &[&str] = &["yaml", "yml", "json", "toml", "figl", "fig"];

/// What `init` found in the target directory — the classification that decides
/// how it proceeds (see `docs/init-adoption.md`). Computed before the interview,
/// over the *content* documents (`ROOT_EXTS`) present, plus the top-level markers
/// that signal an already-initialized workspace.
enum DirState {
    /// Empty, or only files colophon doesn't treat as content documents (images,
    /// code, data). `init` proceeds exactly as on a fresh directory.
    Greenfield,
    /// Content documents are present but none declares a containment link — a
    /// loose folder of notes. `init` can proceed, leaving them unlinked (a future
    /// `adopt` pulls them in); `docs` are their workspace-relative paths.
    LooseContent { docs: Vec<PathBuf> },
    /// A top-level document declares `contents` — an existing colophon/diaryx tree
    /// rooted here. `init` must not mint a competing root; `root` is the detected
    /// top-level root candidate, if unambiguous.
    Structured { root: Option<PathBuf> },
    /// A colophon root or config document is already present — this is an
    /// initialized workspace. `marker` is the file that gave it away.
    Initialized { marker: PathBuf },
}

/// A content document found while classifying a directory: its path (relative to
/// the init directory) and the two frontmatter facts `init` branches on.
struct FoundDoc {
    rel: PathBuf,
    /// Declares a containment link (`contents`/`part_of`) — part of a tree.
    structural: bool,
    /// Has metadata and no `part_of` — a candidate workspace root.
    root_candidate: bool,
}

/// The directory's *own* top-level documents, parsed for the structural /
/// root-candidate facts — the non-recursive counterpart to [`scan_docs`]. A
/// colophon root is a top-level document, so "is this already a workspace?" is a
/// top-level question: scanning the whole tree lets a vendored or nested markdown
/// tree deeper in the repo masquerade as the root.
fn top_level_docs(dir: &Path) -> Vec<FoundDoc> {
    dir_listing(dir, Path::new(""))
        .docs
        .into_iter()
        .map(|rel| {
            let (structural, root_candidate) = std::fs::read_to_string(dir.join(&rel))
                .ok()
                .and_then(|t| Document::parse(&rel, &t).ok())
                .filter(Document::has_meta)
                .map(|doc| {
                    let has_part_of = doc.meta.get("part_of").is_some();
                    (has_part_of || doc.meta.get("contents").is_some(), !has_part_of)
                })
                .unwrap_or((false, false));
            FoundDoc { rel, structural, root_candidate }
        })
        .collect()
}

/// Classify `dir` for `init`. Whether it is already a workspace is decided by the
/// **top-level** documents (an `index.*`/`colophon.*` marker, or a top-level
/// document that declares containment) — never by a recursive sweep, so a
/// vendored or nested tree deeper in the repo cannot be mistaken for the root or
/// inflate the count. Otherwise the loose content is gathered (recursively, for a
/// `mirror` import) to decide loose-vs-greenfield. The second return is every
/// loose *non-document* file (image, PDF, binary, source code) `init` can offer
/// to attach. Empty for an already-a-workspace directory (`init` aborts).
fn classify_dir(dir: &Path) -> (DirState, Vec<PathBuf>) {
    // An existing root (`index.<content|meta-ext>`) or config sidecar
    // (`colophon.<meta-ext>`) at the top level means this is already a workspace.
    for ext in ROOT_EXTS.iter().chain(META_EXTS) {
        let marker = dir.join(format!("index.{ext}"));
        if marker.exists() {
            return (DirState::Initialized { marker }, Vec::new());
        }
    }
    for ext in META_EXTS {
        let marker = dir.join(format!("{CONFIG_STEM}.{ext}"));
        if marker.exists() {
            return (DirState::Initialized { marker }, Vec::new());
        }
    }

    // A top-level document that declares containment is a tree root here (e.g. a
    // README-rooted vault with no index/colophon marker) — this is already a
    // workspace, rooted at the top level, whatever nested trees the repo carries.
    let top = top_level_docs(dir);
    if top.iter().any(|d| d.structural) {
        let root = pick_root_candidate(&top);
        return (DirState::Structured { root }, Vec::new());
    }

    // Not an existing workspace. Gather loose content to offer for adoption —
    // recursively (a folder of notes to mirror), keeping only the *unattached*
    // documents: one already declaring containment belongs to some other tree
    // (vendored, nested) and is not loose content of this directory.
    let mut docs = Vec::new();
    let mut others = Vec::new();
    scan_docs(dir, Path::new(""), &mut docs, &mut others);
    let loose: Vec<PathBuf> = docs.into_iter().filter(|d| !d.structural).map(|d| d.rel).collect();
    let state = if loose.is_empty() {
        DirState::Greenfield
    } else {
        DirState::LooseContent { docs: loose }
    };
    (state, others)
}

/// Recursively collect, under `dir` (rooted at workspace-relative `rel`), the
/// content documents (into `docs`, reading each one's frontmatter for the
/// structural / root-candidate facts) and the loose *opaque* files (into
/// `others` — anything [`colophon::is_opaque_payload`] treats as bytes). Hidden
/// entries (`.`-prefixed) are skipped, mirroring the library's scans; unreadable
/// or unparsable content files count as plain (non-structural) content.
fn scan_docs(dir: &Path, rel: &Path, docs: &mut Vec<FoundDoc>, others: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let child = entry.path();
        let child_rel = rel.join(name);
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            scan_docs(&child, &child_rel, docs, others);
            continue;
        }
        let is_content = child
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| ROOT_EXTS.contains(&e.to_ascii_lowercase().as_str()));
        if !is_content {
            // A non-document file colophon cannot read (image, PDF, data, code)
            // is an attachment candidate; a whole-file metadata file (`.yaml`,
            // `.json`) is neither content nor opaque, so it is simply ignored.
            if colophon::is_opaque_payload(&child_rel) {
                others.push(child_rel);
            }
            continue;
        }
        // Diaryx vocabulary: `contents` (down) / `part_of` (up). Matches
        // `relation_set()`; a configurable vocabulary would read the names here.
        let (structural, root_candidate) = std::fs::read_to_string(&child)
            .ok()
            .and_then(|t| Document::parse(&child_rel, &t).ok())
            .filter(Document::has_meta)
            .map(|doc| {
                let has_part_of = doc.meta.get("part_of").is_some();
                let structural = has_part_of || doc.meta.get("contents").is_some();
                (structural, !has_part_of)
            })
            .unwrap_or((false, false));
        docs.push(FoundDoc { rel: child_rel, structural, root_candidate });
    }
}

/// Pick the workspace root from a set of found documents: a `readme` stem wins
/// (an `index` would have been caught as already-initialized), else a lone
/// root-candidate. Two-plus candidates are ambiguous — `None`, and `init` won't
/// guess.
fn pick_root_candidate(docs: &[FoundDoc]) -> Option<PathBuf> {
    let candidates: Vec<&PathBuf> = docs.iter().filter(|d| d.root_candidate).map(|d| &d.rel).collect();
    let stem_is = |p: &Path, want: &str| {
        p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case(want))
    };
    candidates
        .iter()
        .find(|p| stem_is(p, "index"))
        .or_else(|| candidates.iter().find(|p| stem_is(p, "readme")))
        .map(|p| (*p).clone())
        .or_else(|| (candidates.len() == 1).then(|| candidates[0].clone()))
}

/// Ask which top-level document should become the workspace root, or offer to
/// synthesize a fresh index. Returns the chosen document (relative to the init
/// directory), or `None` to create a new index. Only offered interactively when
/// loose top-level documents exist.
fn prompt_root_choice(docs: &[PathBuf]) -> Result<Option<PathBuf>, AnyError> {
    let mut sel = cliclack::select("Which file should be the workspace root?");
    for d in docs {
        let name = d.file_name().unwrap_or_default().to_string_lossy().into_owned();
        sel = sel.item(d.clone(), name, "adopt this document as the root");
    }
    // The empty path is the "create a new index" sentinel.
    sel = sel.item(PathBuf::new(), "Create a new index", "synthesize a fresh root document");
    let choice = sel.interact()?;
    Ok((!choice.as_os_str().is_empty()).then_some(choice))
}

/// The title an existing document declares (its `title` frontmatter), or a title
/// derived from its filename — used when an existing document is adopted as the
/// root, so the config link and summary read naturally without a title prompt.
fn existing_doc_title(root_dir: &Path, rel: &Path) -> String {
    std::fs::read_to_string(root_dir.join(rel))
        .ok()
        .and_then(|t| Document::parse(rel, &t).ok())
        .and_then(|d| d.meta.get("title").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| link::path_to_title(rel))
}

/// The direct children of one directory, categorized for the interactive intake
/// walk: plaintext `docs`, opaque `others` (attachment candidates), and `subdirs`
/// — all workspace-relative, sorted, hidden entries skipped. Non-recursive: the
/// walk descends only where the user opts in.
struct DirListing {
    docs: Vec<PathBuf>,
    others: Vec<PathBuf>,
    subdirs: Vec<PathBuf>,
}

fn dir_listing(root_dir: &Path, rel_dir: &Path) -> DirListing {
    let mut docs = Vec::new();
    let mut others = Vec::new();
    let mut subdirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root_dir.join(rel_dir)) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let rel = rel_dir.join(name);
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                subdirs.push(rel);
            } else if ContentFormat::from_extension(&rel).is_some() {
                docs.push(rel);
            } else if colophon::is_opaque_payload(&rel) {
                // A whole-file metadata file (`.yaml` config/registry) is neither
                // a document to adopt nor an opaque payload — it is skipped.
                others.push(rel);
            }
        }
    }
    docs.sort();
    others.sort();
    subdirs.sort();
    DirListing { docs, others, subdirs }
}

/// The node document a directory already has — an `index`- or `readme`-stemmed
/// plaintext file directly in it — or `None` (a folder-note must be synthesized).
/// Directory scope of the root discovery in `find_root`/`existing_node`.
fn existing_dir_node(root_dir: &Path, rel_dir: &Path) -> Option<PathBuf> {
    let listing = dir_listing(root_dir, rel_dir);
    let stem_is = |p: &Path, want: &str| {
        p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case(want))
    };
    listing
        .docs
        .iter()
        .find(|p| stem_is(p, "index"))
        .or_else(|| listing.docs.iter().find(|p| stem_is(p, "readme")))
        .cloned()
}

/// The synthesized folder-note name for a directory, in content grammar `ext`.
/// The single place the folder-index convention lives — a future `default index
/// name` config (README.md / `<dir>.md` / custom) slots in here.
fn folder_note_name(rel_dir: &Path, ext: &str) -> PathBuf {
    rel_dir.join(format!("index.{ext}"))
}

/// Interactively walk one directory, accumulating a [`StructurePlan`] (documents
/// to adopt, folder-notes to synthesize) and a list of attachments
/// `(payload, parent)` — the recursive core of the guided `init` intake. `node`
/// is the document this directory's contents hang under (the root, or the
/// directory's own node). For each directory: pick which documents to link, which
/// non-document files to give metadata, and which subdirectories to descend into
/// (each getting its existing index/readme as node, or a synthesized folder-note).
/// Nothing is written here — the plan is applied afterward.
fn intake_walk(
    root_dir: &Path,
    rel_dir: &Path,
    node: &Path,
    ext: &str,
    plan: &mut StructurePlan,
    attachments: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), AnyError> {
    let listing = dir_listing(root_dir, rel_dir);
    let here = if rel_dir.as_os_str().is_empty() {
        ".".to_string()
    } else {
        rel_dir.display().to_string()
    };

    // 1. Documents in this directory (excluding the node itself) → adopt under it.
    let docs: Vec<PathBuf> = listing.docs.iter().filter(|d| d.as_path() != node).cloned().collect();
    if !docs.is_empty() {
        let items: Vec<(PathBuf, String, String)> = docs
            .iter()
            .map(|d| (d.clone(), d.file_name().unwrap_or_default().to_string_lossy().into_owned(), String::new()))
            .collect();
        let picked = cliclack::multiselect(format!("Documents in {here} to link under {}:", node.display()))
            .items(&items)
            .initial_values(docs.clone())
            .required(false)
            .interact()?;
        for child in picked {
            plan.adoptions.push(Adoption { child, parent: node.to_path_buf() });
        }
    }

    // 2. Non-document files → write a metadata sidecar for each chosen one.
    if !listing.others.is_empty() {
        let items: Vec<(PathBuf, String, String)> = listing
            .others
            .iter()
            .map(|f| (f.clone(), f.file_name().unwrap_or_default().to_string_lossy().into_owned(), String::new()))
            .collect();
        let picked = cliclack::multiselect(format!("Non-document files in {here} to give metadata:"))
            .items(&items)
            .required(false)
            .interact()?;
        for payload in picked {
            attachments.push((payload, node.to_path_buf()));
        }
    }

    // 3. Subdirectories → descend into the chosen ones, giving each a node.
    if !listing.subdirs.is_empty() {
        let items: Vec<(PathBuf, String, String)> = listing
            .subdirs
            .iter()
            .map(|d| (d.clone(), format!("{}/", d.file_name().unwrap_or_default().to_string_lossy()), String::new()))
            .collect();
        let picked = cliclack::multiselect(format!("Subdirectories of {here} to include:"))
            .items(&items)
            .initial_values(listing.subdirs.clone())
            .required(false)
            .interact()?;
        for sub in picked {
            let child_node = match existing_dir_node(root_dir, &sub) {
                // An existing index/readme becomes the node — adopt it under here.
                Some(node_rel) => {
                    plan.adoptions.push(Adoption { child: node_rel.clone(), parent: node.to_path_buf() });
                    node_rel
                }
                // No node yet — synthesize a folder-note titled after the folder.
                None => {
                    let path = folder_note_name(&sub, ext);
                    plan.synthesized.push(SynthNode {
                        path: path.clone(),
                        parent: node.to_path_buf(),
                        title: link::path_to_title(&sub),
                    });
                    path
                }
            };
            intake_walk(root_dir, &sub, &child_node, ext, plan, attachments)?;
        }
    }
    Ok(())
}

/// Initialize a workspace: write a self-describing root the other commands can
/// discover. Each field comes from its flag if given; otherwise, on a terminal
/// (and without `--yes`), the user is prompted, and in every other case the
/// default applies (title = directory name, no author, Markdown content, that
/// content's first embed style, YAML metadata).
///
/// The prompts flow *content → embed type → config language*: the content
/// grammar decides which embed styles are on offer (Djot has no delimiter form;
/// HTML uses data islands), and the embed style decides which languages fit
/// (`fig` has no character-delimiter form). A `separate` embed writes the
/// metadata as a sibling whole-file node beside a plain body file; every other
/// style writes a single combined document whose block the carrier-aware editor
/// synthesizes, so the file is a normal document from the start.
#[allow(clippy::too_many_arguments)]
fn cmd_init(
    dir: Option<&Path>,
    title: Option<String>,
    author: Option<String>,
    meta: Option<MetaFormat>,
    embed: Option<EmbedArg>,
    content: Option<ContentLang>,
    wrapper: Option<WrapperArg>,
    reference: Option<ReferenceArg>,
    link_style: Option<LinkStyleArg>,
    identity: Option<IdentityArg>,
    id_storage: Option<IdStorageArg>,
    adopt: Option<AdoptArg>,
    attach: bool,
    yes: bool,
) -> CmdResult {
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir()?,
    };
    std::fs::create_dir_all(&dir)?;
    // Canonicalize (now that the directory exists) for a stable absolute name —
    // both for the default title and the confirmation line.
    let dir = dir.canonicalize()?;

    // Prompt only on a real terminal and only when `--yes` wasn't passed.
    let interactive = !yes && std::io::stdin().is_terminal();
    // The guided, recursive per-directory intake walk replaces the one-shot
    // flat/mirror menu when a terminal is present and no adoption flag forces a
    // non-interactive choice: the user picks documents, files, and subdirectories
    // to include, directory by directory.
    let use_walk = interactive && adopt.is_none() && !attach;

    // Inspect the directory before prompting, and refuse or warn as its contents
    // warrant (docs/init-adoption.md): never overwrite an initialized workspace,
    // never mint a root that competes with an existing tree, and never silently
    // orphan a folder of notes. When loose content is present, decide whether to
    // adopt it — and how (`flat` links each file directly under the new root;
    // `mirror` reproduces the folder tree as containment). The flag wins, else the
    // terminal is asked. `None` here means "leave them unlinked".
    let mut loose_docs: Vec<PathBuf> = Vec::new();
    let mut adopt_mode: Option<AdoptArg> = None;
    let (dir_state, loose_others) = classify_dir(&dir);
    match dir_state {
        DirState::Greenfield => {}
        DirState::Initialized { marker } => {
            return Err(format!(
                "{} already exists — this looks like an initialized workspace",
                marker.display()
            )
            .into());
        }
        // An existing containment tree: minting `index.md` here would create a
        // second root. Adopting a tree (attaching config, linking loose files)
        // is not built yet, so refuse with the path that is — `colophon config`
        // attaches policy to the existing root.
        DirState::Structured { root } => {
            let root_note = root
                .as_ref()
                .map(|r| format!(" (root: {})", r.display()))
                .unwrap_or_default();
            return Err(format!(
                "this directory already holds a colophon workspace{root_note}. \
                 `init` would mint a competing root — to attach colophon configuration \
                 to the existing tree, run `colophon config <key> <value>` from here."
            )
            .into());
        }
        // Loose notes with no tree: safe to initialize over. Adopt them (link each
        // under the new root) or leave them unlinked — the flag decides, else the
        // terminal is asked, else (non-interactive) they are left unlinked with a
        // note, and `--yes` without a decision refuses rather than guess.
        // Walk mode handles loose content per-directory below; here we only run
        // the one-shot flat/mirror menu (or flags) for the non-walk case.
        DirState::LooseContent { .. } if use_walk => {}
        DirState::LooseContent { docs } => {
            let n = docs.len();
            // Whether the loose files span subdirectories — if so, a `mirror`
            // import (folder-as-node) is on the table; a single flat directory has
            // nothing to mirror, so only `flat` is offered.
            let nested = docs.iter().any(|d| d.parent().is_some_and(|p| !p.as_os_str().is_empty()));
            match adopt {
                Some(AdoptArg::Flat) => adopt_mode = Some(AdoptArg::Flat),
                Some(AdoptArg::Mirror) => adopt_mode = Some(AdoptArg::Mirror),
                Some(AdoptArg::None_) => {}
                None if interactive => {
                    // Mention the non-document files too, so the picture is whole
                    // (they get their own attach question after this one).
                    let others_note = match loose_others.len() {
                        0 => String::new(),
                        m => format!(" (plus {m} non-document file(s))"),
                    };
                    let mut menu = cliclack::select(format!(
                        "{n} existing document(s){others_note} here aren't part of a colophon workspace — what should init do?"
                    ));
                    if nested {
                        menu = menu.item(
                            "mirror",
                            "Import the folder tree",
                            "mirror each directory as a node (synthesizing folder indexes)",
                        );
                    }
                    menu = menu
                        .item("flat", "Adopt flat", "link every file directly under the new root")
                        .item("leave", "Leave unlinked", "initialize anyway; colophon check will list them")
                        .item("cancel", "Cancel", "write nothing");
                    match menu.interact()? {
                        "mirror" => adopt_mode = Some(AdoptArg::Mirror),
                        "flat" => adopt_mode = Some(AdoptArg::Flat),
                        "leave" => {}
                        _ => {
                            println!("cancelled — nothing written");
                            return Ok(ExitCode::SUCCESS);
                        }
                    }
                }
                None if yes => {
                    let mirror_hint = if nested {
                        " `--adopt mirror` mirrors the folder tree;"
                    } else {
                        ""
                    };
                    return Err(format!(
                        "{n} existing document(s) here aren't linked into a workspace;\
                        {mirror_hint} pass `--adopt flat` to link them under the root, or \
                         `--adopt none` to initialize and leave them unlinked."
                    )
                    .into());
                }
                None => {
                    eprintln!(
                        "colophon: note — {n} existing document(s) here will not be linked \
                         into the workspace (colophon check will list them; `--adopt flat` links them)."
                    );
                }
            }
            loose_docs = docs;
        }
    }

    // Non-document files (images, PDFs, data, code) can each get a workspace
    // metadata sidecar — a decision separate from the document structure above,
    // and deliberately conservative: attaching is opt-in because an unattached
    // opaque file is invisible to the (reachability-bounded) `check`, so there is
    // nothing to force. The `--attach` flag opts in; a terminal is asked (default
    // *leave*); `--yes` without the flag leaves them alone.
    let mut attach_others = false;
    if !loose_others.is_empty() && !use_walk {
        let m = loose_others.len();
        if attach {
            attach_others = true;
        } else if interactive {
            let choice = cliclack::select(format!(
                "{m} non-document file(s) here (images, PDFs, data, code) — give them workspace metadata?"
            ))
            .item("leave", "Leave unlinked", "invisible to colophon until you attach them")
            .item("attach", "Attach each", "write a metadata sidecar beside each, linked under the root")
            .interact()?;
            attach_others = choice == "attach";
        }
    }

    let default_title = link::path_to_title(&dir);
    // Two prompts are conditional but still count toward "will we prompt?", so
    // the intro/outro stay paired with at least one question: the references
    // prompt is skipped when identity is off (path is forced), and the path-format
    // prompt appears only when a by-path reference is (possibly) authored.
    let reference_prompt_possible = reference.is_none() && identity != Some(IdentityArg::Off);
    let path_format_possible =
        link_style.is_none() && matches!(reference, None | Some(ReferenceArg::Path));
    let id_storage_prompt_possible = id_storage.is_none() && identity != Some(IdentityArg::Off);
    let prompting = interactive
        && (use_walk
            || title.is_none()
            || author.is_none()
            || content.is_none()
            || embed.is_none()
            || meta.is_none()
            || wrapper.is_none()
            || identity.is_none()
            || reference_prompt_possible
            || path_format_possible
            || id_storage_prompt_possible);
    if prompting {
        cliclack::intro("colophon init")?;
    }

    // Root selection (the walk's first step): adopt one of the directory's own
    // top-level documents as the root, or synthesize a fresh index. Offered only
    // when loose documents are present to choose from.
    let root_pick: Option<PathBuf> = if use_walk {
        let top_docs = dir_listing(&dir, Path::new("")).docs;
        if top_docs.is_empty() { None } else { prompt_root_choice(&top_docs)? }
    } else {
        None
    };

    // Each field: flag wins; else prompt when interactive; else the default. An
    // adopted existing root carries its own title, so that prompt is skipped.
    let title = if let Some(root_doc) = &root_pick {
        existing_doc_title(&dir, root_doc)
    } else {
        match title {
            Some(t) if !t.is_empty() => t,
            _ if interactive => cliclack::input("Title")
                .default_input(&default_title)
                .placeholder(&default_title)
                .interact::<String>()?,
            _ => default_title,
        }
    };
    let author = match author {
        Some(a) => (!a.trim().is_empty()).then(|| a.trim().to_string()),
        None if interactive => {
            let entered: String = cliclack::input("Author")
                .required(false)
                .placeholder("optional — leave blank to omit")
                .interact()?;
            (!entered.trim().is_empty()).then(|| entered.trim().to_string())
        }
        None => None,
    };
    // Content grammar first — it gates the embed styles offered next.
    let content = match content {
        Some(c) => c,
        None if interactive => cliclack::select("Content format")
            .initial_value(ContentLang::Markdown)
            .item(ContentLang::Markdown, "Markdown", ".md")
            .item(ContentLang::Djot, "Djot", ".dj")
            .item(ContentLang::Html, "HTML", ".html")
            .interact()?,
        None => ContentLang::Markdown,
    };
    // Embed type — depends on the content grammar.
    let embed: EmbedStyle = match embed {
        Some(e) => {
            let style = EmbedStyle::from(e);
            if !content.allows_embed(style) {
                return Err(format!(
                    "the `{}` embed type does not fit {} content (offered: {})",
                    style.as_config_str(),
                    content.label(),
                    content
                        .embed_styles()
                        .iter()
                        .map(|s| s.as_config_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            style
        }
        None if interactive => prompt_embed_style(content)?,
        None => content.embed_styles()[0],
    };
    // Config language — depends on the embed type (fig has no delimiter form).
    let meta: MetaFormat = match meta {
        Some(m) => {
            if colophon::embed_carrier(embed, m.into()).is_none() {
                return Err(format!(
                    "`{}` metadata cannot be embedded as `{}` (try a code block or separate)",
                    m.label(),
                    embed.as_config_str()
                )
                .into());
            }
            m
        }
        None if interactive => prompt_config_language(embed)?,
        None => MetaFormat::Yaml,
    };
    // The reference style, in two axes (docs/reference-styles.md): pick the
    // wrapper first — the syntactic form every reference is written in.
    let wrapper = match wrapper {
        Some(w) => w,
        None if interactive => prompt_wrapper()?,
        None => WrapperArg::Markdown,
    };
    // Identity gates the addressing axis (id/split register). First: *when* a
    // document earns a stable ID. Default lazy, matching `WorkspaceConfig::default()`.
    let identity = match identity {
        Some(i) => i,
        None if interactive => cliclack::select("Identity")
            .initial_value(IdentityArg::Lazy)
            .item(IdentityArg::Lazy, "On demand", "an ID is minted when a document is linked by ID or published")
            .item(IdentityArg::Off, "None", "documents are addressed by path only")
            .item(IdentityArg::Eager, "From creation", "every document gets an ID when it is created")
            .interact()?,
        None => IdentityArg::Lazy,
    };
    // Second: the addressing — what a reference points at. `id`/`split` register
    // their targets, so they need identity; a `--reference id/split` against
    // `--identity off` is a surfaced contradiction rather than silently ignored.
    // With identity off the interactive menu simply omits those options.
    let reference = match reference {
        Some(r) => {
            if r.needs_identity() && identity == IdentityArg::Off {
                return Err(format!(
                    "`--reference {}` needs identity to mint IDs (try `--identity lazy`)",
                    r.flag()
                )
                .into());
            }
            r
        }
        None if interactive => prompt_reference(identity, wrapper)?,
        None => ReferenceArg::Path,
    };
    // Third: the path format — how a by-path reference renders (root / relative /
    // plain). Only meaningful, and only asked, when the addressing is by path.
    let link_style = match link_style {
        Some(l) => l,
        None if interactive && reference.uses_path() => prompt_path_format(wrapper)?,
        None => LinkStyleArg::MarkdownRoot,
    };
    // Where IDs are stored — only meaningful once something mints them, so the
    // prompt is skipped (and forced to `registry`) when identity is off. The
    // interactive menu offers registry/frontmatter; `frontmatter-only` is
    // deliberately flag-only (it forfeits tombstones).
    let id_storage = if identity == IdentityArg::Off {
        IdStorageArg::Registry
    } else {
        match id_storage {
            Some(s) => s,
            None if interactive => prompt_id_storage()?,
            None => IdStorageArg::Frontmatter,
        }
    };

    // Assemble the workspace preferences these choices encode. The (wrapper,
    // reference) pair writes the default `reference_*` axes and any per-relation
    // overrides (the up≠down split) onto the config.
    let mut ws_config = WorkspaceConfig {
        link_format: link_style.into(),
        identity: identity.registration(),
        id_links: false,
        reference_wrapper: None,
        reference_target: None,
        reference_label: None,
        relation_styles: std::collections::BTreeMap::new(),
        id_storage: id_storage.into(),
        default_embed_format: meta.into(),
        embed_style: embed,
        content_format: content.into(),
    };
    reference.write_onto(wrapper.into(), &mut ws_config);

    let meta_format: Format = meta.into();
    let config_name = sidecar_name(CONFIG_STEM, meta_format);
    let content_name = format!("index.{}", content.ext());
    // The carrier the root's metadata lives in — a fenced block in the content
    // file, or (for `separate`) a whole-file sibling node. Validated already, so
    // this never fails here.
    let carrier = colophon::embed_carrier(embed, meta_format).ok_or_else(|| {
        format!("`{}` metadata cannot be embedded as `{}`", meta.label(), embed.as_config_str())
    })?;

    // Write the root, and learn which file is the structural root document (the
    // node the config's `part_of` points back at, and the `next:` hint names).
    // An adopted existing document becomes the root as-is (its config pointer is
    // added after the config document is written, below); otherwise a fresh root
    // is synthesized in the chosen carrier.
    let root_name = if let Some(root_doc) = &root_pick {
        root_doc.to_string_lossy().into_owned()
    } else {
        match carrier {
        // Separate: a plain body file (heading only) plus a whole-file metadata
        // node that points at it via `content` and carries the same title/author/
        // config pointer a combined root would embed.
        MetaCarrier::WholeFile(format) => {
            let node_name = format!("index.{}", colophon::document::whole_file_extension(format));
            std::fs::write(dir.join(&content_name), content.heading(&title))?;
            let mut node = Mapping::new();
            node.insert("title".into(), Value::String(title.clone()));
            if let Some(author) = &author {
                node.insert("author".into(), Value::String(author.clone()));
            }
            node.insert("content".into(), Value::String(content_name.clone()));
            node.insert("config".into(), Value::String(config_name.clone()));
            std::fs::write(dir.join(&node_name), meta::serialize_mapping(&node, format)?)?;
            node_name
        }
        // Combined: one document, its block synthesized around the body (leading
        // blank line = the conventional gap after a closing fence).
        MetaCarrier::Fenced(_) => {
            let body = format!("\n{}", content.heading(&title));
            let mut editor = edit::MetaEditor::open_or_init(&body, Some(carrier))?;
            editor.set_value(&edit::key_path("title"), edit::infer_scalar(&title))?;
            if let Some(author) = &author {
                editor.set_value(&edit::key_path("author"), edit::infer_scalar(author))?;
            }
            editor.set_value(&edit::key_path("config"), edit::infer_scalar(&config_name))?;
            std::fs::write(dir.join(&content_name), editor.render()?)?;
            content_name.clone()
        }
        }
    };

    // Write the config document beside the root, in the chosen metadata format:
    // self-describing (title + `part_of` back to the root, in the chosen link
    // style) plus the recorded preferences. A whole-file config document (DESIGN
    // §6/§7), the same shape as the registry.
    let config_rel = PathBuf::from(&config_name);
    let part_of = link::format_link(ws_config.link_format, &config_rel, Path::new(&root_name), &title);
    let mut config_map = Mapping::new();
    config_map.insert("title".into(), Value::String("colophon config".into()));
    config_map.insert("part_of".into(), Value::String(part_of));
    for (key, value) in ws_config.to_mapping() {
        config_map.insert(key, value);
    }
    std::fs::write(dir.join(&config_rel), meta::serialize_mapping(&config_map, meta_format)?)?;

    // An adopted existing root did not get a `config` pointer during synthesis
    // (it was not synthesized) — add it now, comment- and format-preservingly,
    // so the workspace is discoverable from its own root like any other.
    if root_pick.is_some() {
        let root_full = dir.join(&root_name);
        let text = std::fs::read_to_string(&root_full)?;
        let doc = Document::parse(Path::new(&root_name), &text)?;
        let updated = edit::set_in_text(&text, doc.carrier, "config", edit::infer_scalar(&config_name))?;
        std::fs::write(&root_full, updated)?;
    }

    // Adoption of pre-existing loose content (docs/init-adoption.md). `flat`
    // (Phase 1) links each document directly under the freshly-written root;
    // `mirror` (Phase 2) folds the directory tree into the containment tree,
    // synthesizing a folder-note index for each bare directory. Both run over the
    // workspace we just wrote, so a registry is bootstrapped first when the links
    // will mint IDs (as `new` does).
    let mut adopt_note = String::new();
    let do_adopt = adopt_mode.is_some() && !loose_docs.is_empty();
    let do_attach = attach_others && !loose_others.is_empty();
    if do_adopt || do_attach {
        let mut ctx = Ctx {
            root_dir: dir.clone(),
            root_doc: PathBuf::from(&root_name),
            registry: None,
            config: ws_config.clone(),
        };
        let link_registers = ctx.config.reference_style().registers()
            || ctx.config.resolved_relation_styles().values().any(|s| s.registers());
        let mints = (link_registers && ctx.config.identity.fires_on(Trigger::Link))
            || ctx.config.identity.fires_on(Trigger::Create);
        if mints {
            ensure_registry(&mut ctx)?;
        }
        let mut ws = workspace(&ctx)?;
        // `mirror` needs a combined-document root; if the interview chose a
        // separated root, fall back to flat rather than abort a written workspace.
        let strategy = match adopt_mode.filter(|_| do_adopt) {
            Some(AdoptArg::Mirror) => match block_on(ws.plan_mirror(&ctx.root_doc)) {
                Ok(plan) => {
                    let outcome = block_on(ws.apply_plan(&plan))?;
                    for (doc, why) in &outcome.skipped {
                        eprintln!("colophon: could not adopt {}: {why}", doc.display());
                    }
                    adopt_note = format!(
                        "\nimported {} document(s), synthesizing {} folder index(es), mirroring the tree under {root_name}",
                        outcome.adopted.len(),
                        outcome.synthesized.len(),
                    );
                    None // handled
                }
                Err(e) => {
                    eprintln!("colophon: mirror import unavailable ({e}); adopting flat instead");
                    Some(AdoptArg::Flat)
                }
            },
            other => other,
        };
        if let Some(AdoptArg::Flat) = strategy {
            let mut adopted = 0usize;
            for doc in &loose_docs {
                match block_on(ws.adopt(doc, &ctx.root_doc)) {
                    Ok(()) => adopted += 1,
                    Err(e) => eprintln!("colophon: could not adopt {}: {e}", doc.display()),
                }
            }
            adopt_note = format!("\nadopted {adopted} existing document(s) under {root_name}");
        }
        // Attachments: a metadata sidecar for each opaque file, flat under the
        // root (a folder-aware placement would need mirror's node map; the flat
        // link resolves from anywhere).
        if do_attach {
            let mut attached = 0usize;
            for payload in &loose_others {
                match block_on(ws.attach(payload, &ctx.root_doc)) {
                    Ok(_) => attached += 1,
                    Err(e) => eprintln!("colophon: could not attach {}: {e}", payload.display()),
                }
            }
            adopt_note.push_str(&format!(
                "\nattached {attached} non-document file(s) under {root_name}"
            ));
        }
        persist(&ctx, &mut ws)?;
    }

    // The guided intake walk: descend the tree directory by directory, picking
    // which documents to link, which files to attach, and which subdirectories to
    // enter — building a plan that is applied after the root exists. Replaces the
    // one-shot flat/mirror menu for the interactive case.
    if use_walk {
        let mut plan = StructurePlan::default();
        let mut attachments: Vec<(PathBuf, PathBuf)> = Vec::new();
        intake_walk(&dir, Path::new(""), Path::new(&root_name), content.ext(), &mut plan, &mut attachments)?;
        if !plan.is_empty() || !attachments.is_empty() {
            let mut ctx = Ctx {
                root_dir: dir.clone(),
                root_doc: PathBuf::from(&root_name),
                registry: None,
                config: ws_config.clone(),
            };
            let link_registers = ctx.config.reference_style().registers()
                || ctx.config.resolved_relation_styles().values().any(|s| s.registers());
            let mints = (link_registers && ctx.config.identity.fires_on(Trigger::Link))
                || ctx.config.identity.fires_on(Trigger::Create);
            if mints {
                ensure_registry(&mut ctx)?;
            }
            let mut ws = workspace(&ctx)?;
            let outcome = block_on(ws.apply_plan(&plan))?;
            for (doc, why) in &outcome.skipped {
                eprintln!("colophon: could not link {}: {why}", doc.display());
            }
            let mut attached = 0usize;
            for (payload, parent) in &attachments {
                match block_on(ws.attach(payload, parent)) {
                    Ok(_) => attached += 1,
                    Err(e) => eprintln!("colophon: could not attach {}: {e}", payload.display()),
                }
            }
            persist(&ctx, &mut ws)?;
            adopt_note = format!(
                "\nlinked {} document(s), synthesized {} folder index(es), attached {attached} file(s)",
                outcome.adopted.len(),
                outcome.synthesized.len(),
            );
        }
    }

    let author_note = author.as_deref().map(|a| format!(", author {a}")).unwrap_or_default();
    let (embed_label, _) = embed_labels(embed);
    // The path format only appears when a by-path reference is authored — it is
    // inert otherwise.
    let path_note = if reference.uses_path() {
        format!(", path format {}", ws_config.link_format.as_config_str())
    } else {
        String::new()
    };
    // ID storage only matters once identity is on (something mints).
    let id_storage_note = if identity != IdentityArg::Off {
        format!(", id storage {}", id_storage.label())
    } else {
        String::new()
    };
    let details = format!(
        "root: {root_name} — {title}{author_note}\n\
         config: {config_name} — content {}, embed {} ({}), language {}, identity {}, wrapper {}, references {}{path_note}{id_storage_note}",
        content.label(),
        embed.as_config_str(),
        embed_label.to_lowercase(),
        meta.label(),
        identity.label(),
        wrapper.label(),
        reference.label(),
    );
    let next = format!("next: colophon new <path> --parent {root_name}");
    if prompting {
        cliclack::outro(format!("initialized {}\n{details}{adopt_note}\n{next}", dir.display()))?;
    } else {
        println!("initialized {}", dir.display());
        for line in details.lines() {
            println!("  {line}");
        }
        for line in adopt_note.lines().filter(|l| !l.is_empty()) {
            println!("  {line}");
        }
        println!("{next}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Prompt for the embed type, offering only the styles that suit `content` (the
/// first is the default). See [`ContentLang::embed_styles`].
/// Prompt for the reference **wrapper** — the first style axis.
fn prompt_wrapper() -> std::io::Result<WrapperArg> {
    cliclack::select("Wrapper")
        .initial_value(WrapperArg::Markdown)
        .item(WrapperArg::Markdown, "Markdown", "[Title](target) — the diaryx/CommonMark form")
        .item(WrapperArg::Wikilink, "Wikilink", "[[target]] — the Obsidian form")
        .interact()
}

/// Prompt for the reference **addressing** — the second axis. The menu is gated
/// by the two axes already chosen: the registering options (`id`, `split`) appear
/// only when identity can mint IDs, and the by-title options (`alias`, and the
/// `split` that relies on it going down) appear only under the wikilink wrapper —
/// an alias has no markdown spelling, so offering it under markdown would author
/// a wikilink the user did not ask for.
fn prompt_reference(identity: IdentityArg, wrapper: WrapperArg) -> std::io::Result<ReferenceArg> {
    let registers = identity != IdentityArg::Off;
    let wikilink = wrapper == WrapperArg::Wikilink;
    let mut select = cliclack::select("References between documents")
        .initial_value(ReferenceArg::Path)
        .item(ReferenceArg::Path, "By path", "readable; rewritten when a file moves");
    if registers {
        select = select.item(
            ReferenceArg::Id,
            "By stable ID",
            "durable; the registry tracks where each file lives",
        );
    }
    if wikilink {
        select = select.item(
            ReferenceArg::Alias,
            "By title",
            "[[Title]] — readable, not move/rename-safe",
        );
        if registers {
            select = select.item(
                ReferenceArg::Split,
                "Readable down, durable up",
                "contents by title ([[Title]]), part_of by ID",
            );
        }
    }
    select.interact()
}

/// Prompt for where IDs are stored — registry vs. a self-describing frontmatter
/// shadow. The `frontmatter-only` mode (no registry) is intentionally not offered
/// here; it forfeits tombstones and is reachable only via `--id-storage`.
fn prompt_id_storage() -> std::io::Result<IdStorageArg> {
    cliclack::select("Where IDs are stored")
        .initial_value(IdStorageArg::Frontmatter)
        .item(
            IdStorageArg::Frontmatter,
            "In each file (+ registry)",
            "each document carries its own `id`; portable, travels with the file",
        )
        .item(IdStorageArg::Registry, "Registry only", "IDs live in one registry document")
        .interact()
}

/// Prompt for how *path* references are rendered — asked only when a by-path
/// reference is authored. The wrapper is already chosen: markdown offers the
/// full bracket-or-bare set, wikilink offers only the inner path *shape* (it
/// always wraps).
fn prompt_path_format(wrapper: WrapperArg) -> std::io::Result<LinkStyleArg> {
    let mut select = cliclack::select("Path format").initial_value(LinkStyleArg::MarkdownRoot);
    select = match wrapper {
        WrapperArg::Markdown => select
            .item(LinkStyleArg::MarkdownRoot, "Workspace-absolute", "[Title](/path.md)")
            .item(LinkStyleArg::MarkdownRelative, "Relative", "[Title](../path.md)")
            .item(LinkStyleArg::PlainRelative, "Plain relative", "../path.md")
            .item(LinkStyleArg::PlainCanonical, "Plain workspace path", "path.md"),
        WrapperArg::Wikilink => select
            .item(LinkStyleArg::MarkdownRoot, "Workspace-absolute", "[[/path.md]]")
            .item(LinkStyleArg::PlainRelative, "Relative", "[[../path.md]]")
            .item(LinkStyleArg::PlainCanonical, "Workspace path", "[[path.md]]"),
    };
    select.interact()
}

fn prompt_embed_style(content: ContentLang) -> std::io::Result<EmbedStyle> {
    let styles = content.embed_styles();
    let mut select = cliclack::select("Embed type").initial_value(styles[0]);
    for &style in styles {
        let (label, hint) = embed_labels(style);
        select = select.item(style, label, hint);
    }
    select.interact()
}

/// Prompt for the config language, offering only the languages compiled into
/// this binary that fit `embed` (YAML always; TOML/JSON/fig per feature; `fig`
/// omitted for the delimiter style). See [`config_languages`].
fn prompt_config_language(embed: EmbedStyle) -> std::io::Result<MetaFormat> {
    let options = config_languages(embed);
    let mut select = cliclack::select("Config language").initial_value(options[0].0);
    for (format, label) in options {
        select = select.item(format, label, "");
    }
    select.interact()
}

fn cmd_show(file: &Path) -> CmdResult {
    let (_, doc) = load(file)?;
    let set = relation_set();

    println!("{}", file.display());

    if let Some(title) = doc.meta.get("title").and_then(Value::as_str) {
        println!("  title: {title}");
    }

    if !doc.has_meta() {
        println!("  (no embedded metadata)");
        return Ok(ExitCode::SUCCESS);
    }

    let children = set.children(&doc.meta);
    if let Some(spanning) = set.spanning_relation() {
        println!("  {spanning} ({} children):", children.len());
        for child in &children {
            println!("    - {child}");
        }
    }

    // Overlay relations (everything that isn't the spanning tree), grouped and
    // printed in the vocabulary's declared order.
    let spanning = set.spanning_relation();
    let edges = set.edges(&doc.meta);
    for relation in set.relations() {
        if Some(relation.name.as_str()) == spanning {
            continue;
        }
        let targets: Vec<&str> = edges
            .iter()
            .filter(|e| e.relation == relation.name)
            .map(|e| e.target.as_str())
            .collect();
        if targets.is_empty() {
            continue;
        }
        println!("  {}:", relation.name);
        for target in targets {
            println!("    - {target}");
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn cmd_links(file: &Path, relation: Option<&str>) -> CmdResult {
    let (_, doc) = load(file)?;
    for edge in relation_set().edges(&doc.meta) {
        if relation.is_none_or(|want| want == edge.relation) {
            println!("{}\t{}", edge.relation, edge.target);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_meta(file: &Path, format: Option<MetaFormat>) -> CmdResult {
    let (_, doc) = load(file)?;
    let Some(mapping) = doc.meta.as_mapping() else {
        return Err("document has no embedded metadata".into());
    };
    // Default to the format the document already uses.
    let format = format.map(Format::from).unwrap_or_else(|| {
        doc.carrier.map(|c| c.format()).unwrap_or(Format::Yaml)
    });
    print!("{}", meta::serialize_mapping(mapping, format)?);
    Ok(ExitCode::SUCCESS)
}

fn cmd_get(file: &Path, key: &str) -> CmdResult {
    let (_, doc) = load(file)?;
    let mut value = &doc.meta;
    for part in key.split('.') {
        value = match part.parse::<usize>() {
            Ok(index) => value.as_sequence().and_then(|s| s.get(index)),
            Err(_) => value.get(part),
        }
        .ok_or_else(|| format!("no `{key}` in {}", file.display()))?;
    }
    match value {
        Value::Null => println!("null"),
        Value::Bool(b) => println!("{b}"),
        Value::Int(i) => println!("{i}"),
        Value::Float(f) => println!("{f}"),
        Value::String(s) => println!("{s}"),
        compound => {
            let format = doc.carrier.map(|c| c.format()).unwrap_or(Format::Yaml);
            print!("{}", meta::serialize_value(compound, format)?);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_body(file: &Path) -> CmdResult {
    let (_, doc) = load(file)?;
    print!("{}", doc.body);
    Ok(ExitCode::SUCCESS)
}

fn cmd_render(file: &Path) -> CmdResult {
    let (_, doc) = load(file)?;
    let format = colophon::ContentFormat::from_extension(file).ok_or_else(|| {
        format!(
            "{}: not a recognized body format (expected .md/.markdown or .dj/.djot)",
            file.display()
        )
    })?;
    let html = colophon::render_html(&doc.body, format)?;
    print!("{html}");
    Ok(ExitCode::SUCCESS)
}

fn cmd_set(file: &Path, key: &str, value: &str) -> CmdResult {
    let (text, doc) = load(file)?;
    let updated = edit::set_in_text(&text, doc.carrier, key, edit::infer_scalar(value))?;
    std::fs::write(file, updated)?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_unset(file: &Path, key: &str) -> CmdResult {
    let (text, doc) = load(file)?;
    let updated = edit::unset_in_text(&text, doc.carrier, key)?;
    std::fs::write(file, updated)?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_tree(root: Option<&Path>) -> CmdResult {
    let ctx = find_root()?;
    let root = match root {
        Some(r) => ws_rel(&ctx, r)?,
        None => ctx.root_doc.clone(),
    };
    let node = block_on(workspace(&ctx)?.tree(&root))?;
    print_node(&node, "", true, true);
    Ok(ExitCode::SUCCESS)
}

/// Render one tree node: `path — title (marker)`, then its children with
/// box-drawing connectors.
fn print_node(node: &Node, prefix: &str, is_last: bool, is_root: bool) {
    let connector = if is_root {
        String::new()
    } else {
        format!("{prefix}{}", if is_last { "└── " } else { "├── " })
    };
    let name = node
        .title
        .as_deref()
        .or(node.label.as_deref())
        .map(|t| format!("{} — {t}", node.path.display()))
        .unwrap_or_else(|| node.path.display().to_string());
    let marker = match &node.kind {
        NodeKind::Doc => String::new(),
        NodeKind::Missing => " (missing)".to_string(),
        NodeKind::Cycle => " (cycle!)".to_string(),
        NodeKind::Unreadable(e) => format!(" (unreadable: {e})"),
        NodeKind::UnresolvedId(id) => format!(" (unresolved id: {id})"),
        NodeKind::AmbiguousAlias(name) => format!(" (ambiguous alias: [[{name}]])"),
    };
    println!("{connector}{name}{marker}");
    let child_prefix = if is_root {
        String::new()
    } else {
        format!("{prefix}{}", if is_last { "    " } else { "│   " })
    };
    for (i, child) in node.children.iter().enumerate() {
        print_node(child, &child_prefix, i + 1 == node.children.len(), false);
    }
}

/// One choice on an explore screen — what selecting the menu item does.
enum ExploreAction {
    /// Page the current document's raw text.
    View,
    /// Open the current document in `$EDITOR`.
    Edit,
    /// Navigate to another document (a resolved forward link or a backlink).
    Goto(PathBuf),
    /// A link that resolves to nothing followable (external, unresolved id,
    /// ambiguous alias) — selecting it just prints why.
    Note(String),
    /// Return to the previously-visited document.
    Back,
    Quit,
}

/// Interactively walk the workspace graph: at each document, view or edit it, or
/// follow any forward link (in any relation) or backlink to move on. A thin loop
/// over the library's resolution — the same path/id/alias resolution `tree` and
/// `check` use, with the reachability-scoped title index and the backlink map
/// each computed once up front.
fn cmd_explore(file: Option<&Path>) -> CmdResult {
    let ctx = find_root()?;
    let ws = workspace(&ctx)?;
    let root = ctx.root_doc.clone();
    let mut current = match file {
        Some(f) => ws_rel(&ctx, f)?,
        None => root.clone(),
    };
    // Alias resolution and backlinks, computed once — both bounded/lazy, so cheap
    // even at the root of a large repo.
    let titles = block_on(ws.title_index_scoped(&root))?;
    let backlinks = block_on(ws.backlinks(&root))?;

    let mut history: Vec<PathBuf> = Vec::new();
    loop {
        let full = ctx.root_dir.join(&current);
        let (text, doc) = match load(&full) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("colophon: cannot open {}: {e}", current.display());
                match history.pop() {
                    Some(prev) => {
                        current = prev;
                        continue;
                    }
                    None => return Ok(ExitCode::FAILURE),
                }
            }
        };
        let title = doc.meta.get("title").and_then(Value::as_str).unwrap_or_default().to_string();

        // Build the menu: view/edit, every forward link (by relation), every
        // backlink, then navigation.
        let mut actions: Vec<(String, String, ExploreAction)> = Vec::new();
        actions.push(("View this document".into(), "page the raw file".into(), ExploreAction::View));
        actions.push(("Edit in $EDITOR".into(), String::new(), ExploreAction::Edit));

        for relation in ws.relations().relations() {
            let Some(value) = doc.meta.get(&relation.name) else { continue };
            for raw in value.link_strings() {
                let parsed = link::Link::parse(&raw);
                let (label, action) = match ws.resolve_link_with(&current, &parsed, Some(&titles)) {
                    Target::Path(p) => {
                        let t = doc_title(&ctx, &p);
                        (format!("{}: {t}  ({})", relation.name, p.display()), ExploreAction::Goto(p))
                    }
                    Target::External => (
                        format!("{}: {} (external)", relation.name, parsed.target),
                        ExploreAction::Note("external link — not followed".into()),
                    ),
                    Target::UnresolvedId(id) => (
                        format!("{}: {id} (unresolved id)", relation.name),
                        ExploreAction::Note("this id has no live registry entry".into()),
                    ),
                    Target::AmbiguousAlias(name) => (
                        format!("{}: {name} (ambiguous alias)", relation.name),
                        ExploreAction::Note("several documents share this title".into()),
                    ),
                };
                actions.push((label, String::new(), action));
            }
        }

        if let Some(inbound) = backlinks.get(&current) {
            for backlink in inbound {
                let by = if backlink.by_id { "id" } else { "path" };
                actions.push((
                    format!("← {} [{}]", backlink.source.display(), backlink.site),
                    format!("linked from, by {by}"),
                    ExploreAction::Goto(backlink.source.clone()),
                ));
            }
        }

        if !history.is_empty() {
            actions.push(("Back".into(), "the previous document".into(), ExploreAction::Back));
        }
        actions.push(("Quit".into(), String::new(), ExploreAction::Quit));

        let header = if title.is_empty() {
            current.display().to_string()
        } else {
            format!("{} — {title}", current.display())
        };
        let mut menu = cliclack::select(header);
        for (i, (label, hint, _)) in actions.iter().enumerate() {
            menu = menu.item(i, label, hint);
        }
        // Any error (including a Ctrl-C / Esc cancel) leaves the explorer.
        let Ok(choice) = menu.interact() else { break };

        match &actions[choice].2 {
            ExploreAction::View => page_text(&text)?,
            ExploreAction::Edit => edit_file(&full)?,
            ExploreAction::Goto(p) => {
                history.push(current.clone());
                current = p.clone();
            }
            ExploreAction::Note(message) => eprintln!("colophon: {message}"),
            ExploreAction::Back => {
                if let Some(prev) = history.pop() {
                    current = prev;
                }
            }
            ExploreAction::Quit => break,
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The title a linked document declares (its `title` frontmatter), else a title
/// derived from its filename — the label an explore menu shows for a link.
fn doc_title(ctx: &Ctx, rel: &Path) -> String {
    load(&ctx.root_dir.join(rel))
        .ok()
        .and_then(|(_, d)| d.meta.get("title").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| link::path_to_title(rel))
}

/// Page `text` through `$PAGER` (default `less`), falling back to a plain print
/// when no pager can be spawned.
fn page_text(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
    let mut parts = pager.split_whitespace();
    let Some(program) = parts.next() else {
        print!("{text}");
        return Ok(());
    };
    let spawned = std::process::Command::new(program)
        .args(parts)
        .stdin(std::process::Stdio::piped())
        .spawn();
    match spawned {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            child.wait()?;
        }
        Err(_) => print!("{text}"),
    }
    Ok(())
}

/// Open `path` in `$EDITOR`/`$VISUAL` (default `vi`), inheriting the terminal.
fn edit_file(path: &Path) -> std::io::Result<()> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    std::process::Command::new(program).args(parts).arg(path).status()?;
    Ok(())
}

fn cmd_check(root: Option<&Path>, fix: bool) -> CmdResult {
    let mut ctx = find_root()?;
    let root = match root {
        Some(r) => ws_rel(&ctx, r)?,
        None => ctx.root_doc.clone(),
    };
    let mut ws = workspace(&ctx)?;
    let findings = block_on(ws.check(&root))?;
    if fix {
        return cmd_check_fix(&mut ctx, &mut ws, &root, &findings);
    }
    for finding in &findings {
        println!("{finding}");
    }
    if findings.is_empty() {
        println!("ok: no findings");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("{} finding(s)", findings.len());
        Ok(ExitCode::FAILURE)
    }
}

/// Interactive repair: walk the findings, and for each one that has a safe,
/// metadata-only fix, show it and ask before applying. Findings with no fix are
/// printed as needing attention. `suggest_fix` is consulted lazily, so a fix
/// applied to one document correctly declines a now-stale finding later.
fn cmd_check_fix(
    ctx: &mut Ctx,
    ws: &mut Workspace<StdFs, Minter, FileIndex>,
    root: &Path,
    findings: &[colophon::Finding],
) -> CmdResult {
    let mut applied = 0usize;
    let mut needs_attention = 0usize;
    let mut apply_all = false;
    for finding in findings {
        // An orphan has no metadata-only `Fix` (nothing in the document is
        // wrong); repairing it means *adopting* it under a parent. Offer the
        // workspace root as that parent — the same flat adoption `init --adopt`
        // performs — since a batch fix can't ask which subtree it belongs to.
        if let colophon::Finding::Orphan { doc } = finding {
            println!("⚑  {finding}");
            println!("   → adopt {} under {}", doc.display(), root.display());
            let apply = apply_all
                || match prompt("   adopt? [y]es / [n]o / [a]ll / [q]uit: ")?.as_str() {
                    "a" | "all" => {
                        apply_all = true;
                        true
                    }
                    "y" | "yes" => true,
                    "q" | "quit" => {
                        println!("stopped; {applied} fix(es) applied");
                        break;
                    }
                    _ => false,
                };
            if apply {
                block_on(ws.adopt(doc, root))?;
                applied += 1;
            } else {
                needs_attention += 1;
            }
            continue;
        }
        let Some(fix) = block_on(ws.suggest_fix(finding))? else {
            println!("•  {finding}");
            needs_attention += 1;
            continue;
        };
        println!("⚑  {finding}");
        println!("   → {fix}");
        let apply = apply_all
            || match prompt("   apply? [y]es / [n]o / [a]ll / [q]uit: ")?.as_str() {
                "a" | "all" => {
                    apply_all = true;
                    true
                }
                "y" | "yes" => true,
                "q" | "quit" => {
                    println!("stopped; {applied} fix(es) applied");
                    return Ok(ExitCode::SUCCESS);
                }
                _ => false,
            };
        if apply {
            block_on(ws.apply_fix(&fix))?;
            applied += 1;
        }
    }
    // A fix may have registered an ID (an adopted `id`, or an id-link back-link):
    // make sure a registry exists and persist the identity changes to disk. Gate
    // on the index actually having changed, so a purely path-based fix (a
    // path-style inverse, adopting an orphan by path) does not bootstrap an empty
    // registry document as a side effect.
    if applied > 0 && ws.index().is_dirty() {
        ensure_registry(ctx)?;
        persist(ctx, ws)?;
    }
    println!("applied {applied} fix(es); {needs_attention} finding(s) need attention");
    Ok(ExitCode::SUCCESS)
}

/// Prompt on stderr, read a trimmed, lowercased line from stdin (EOF → empty).
fn prompt(message: &str) -> Result<String, AnyError> {
    use std::io::Write;
    eprint!("{message}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

fn cmd_new(path: &Path, parent: &Path) -> CmdResult {
    let mut ctx = find_root()?;
    // Authoring a reference that registers (the default style, or any relation's
    // override — e.g. `part_of: id` in a split) mints IDs, as does an eager
    // policy; ensure a registry to persist them exists *before* the workspace is
    // built over it.
    let link_registers = ctx.config.reference_style().registers()
        || ctx.config.resolved_relation_styles().values().any(|s| s.registers());
    let mints = (link_registers && ctx.config.identity.fires_on(Trigger::Link))
        || ctx.config.identity.fires_on(Trigger::Create);
    if mints {
        ensure_registry(&mut ctx)?;
    }
    let mut ws = workspace(&ctx)?;
    let created = block_on(ws.create(&ws_rel(&ctx, path)?, &ws_rel(&ctx, parent)?))?;
    persist(&ctx, &mut ws)?;
    // A separated child is a pair — the metadata node the parent links, plus its
    // prose body file. Name both so it is clear two files were written.
    match &created.body {
        Some(body) => {
            println!("created {} (in {})", created.node.display(), parent.display());
            println!("  body: {}", body.display());
        }
        None => println!("created {} (in {})", path.display(), parent.display()),
    }
    Ok(ExitCode::SUCCESS)
}

/// Attach an arbitrary file — or, with `--all`, every loose file under the
/// workspace — minting a metadata sidecar and linking it under a parent
/// (default: the workspace root). Mirrors [`cmd_new`] — an id-registering
/// reference style or an eager policy mints IDs, so a registry is ensured first.
fn cmd_attach(
    payload: Option<&Path>,
    parent: Option<&Path>,
    all: bool,
    recursive: bool,
) -> CmdResult {
    let mut ctx = find_root()?;
    let link_registers = ctx.config.reference_style().registers()
        || ctx.config.resolved_relation_styles().values().any(|s| s.registers());
    let mints = (link_registers && ctx.config.identity.fires_on(Trigger::Link))
        || ctx.config.identity.fires_on(Trigger::Create);
    if mints {
        ensure_registry(&mut ctx)?;
    }
    // Default the parent to the workspace root — the common "attach this to my
    // workspace" case needs no `--parent`.
    let parent_rel = match parent {
        Some(p) => ws_rel(&ctx, p)?,
        None => ctx.root_doc.clone(),
    };
    if recursive && !all {
        return Err("--recursive only applies with --all".into());
    }
    let mut ws = workspace(&ctx)?;

    if all {
        if payload.is_some() {
            return Err("pass a file or --all, not both".into());
        }
        // Bounded to reached directories by default; `--recursive` sweeps the
        // whole tree (a pure asset dump you know is all attachments).
        let loose = if recursive {
            block_on(ws.loose_attachments())?
        } else {
            block_on(ws.loose_attachments_in(&ctx.root_doc))?
        };
        if loose.is_empty() {
            println!("no loose files to attach");
            return Ok(ExitCode::SUCCESS);
        }
        let mut attached = 0usize;
        for p in &loose {
            match block_on(ws.attach(p, &parent_rel)) {
                Ok(node) => {
                    println!("attached {} (sidecar {})", p.display(), node.display());
                    attached += 1;
                }
                Err(e) => eprintln!("colophon: could not attach {}: {e}", p.display()),
            }
        }
        persist(&ctx, &mut ws)?;
        println!("attached {attached} file(s) under {}", parent_rel.display());
        return Ok(ExitCode::SUCCESS);
    }

    let Some(payload) = payload else {
        return Err("specify a file to attach, or pass --all".into());
    };
    let node = block_on(ws.attach(&ws_rel(&ctx, payload)?, &parent_rel))?;
    persist(&ctx, &mut ws)?;
    println!(
        "attached {} (sidecar {} in {})",
        payload.display(),
        node.display(),
        parent_rel.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_mv(from: &Path, to: &Path) -> CmdResult {
    let ctx = find_root()?;
    let mut ws = workspace(&ctx)?;
    block_on(ws.rename(&ws_rel(&ctx, from)?, &ws_rel(&ctx, to)?))?;
    persist(&ctx, &mut ws)?;
    println!("moved {} -> {}", from.display(), to.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_rm(path: &Path, force: bool) -> CmdResult {
    let ctx = find_root()?;
    let mut ws = workspace(&ctx)?;
    let danglers = block_on(ws.delete(&ws_rel(&ctx, path)?, force))?;
    persist(&ctx, &mut ws)?;
    println!("deleted {}", path.display());
    for finding in &danglers {
        eprintln!("warning: now dangling — {finding}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_id(file: &Path) -> CmdResult {
    let mut ctx = find_root()?;
    if !ctx.config.identity.fires_on(Trigger::Link) {
        return Err("identity is off in this workspace's config \
             (run `colophon config identity lazy` to enable stable IDs)"
            .into());
    }
    ensure_registry(&mut ctx)?;
    let mut ws = workspace(&ctx)?;
    let id = block_on(ws.register(&ws_rel(&ctx, file)?, Trigger::Link))?;
    persist(&ctx, &mut ws)?;
    println!("{}", link::id_target(&id));
    Ok(ExitCode::SUCCESS)
}

fn cmd_config(key: Option<&str>, value: Option<&str>) -> CmdResult {
    let ctx = find_root()?;
    match (key, value) {
        // No key: print the effective config (defaults + root + config document).
        (None, _) => {
            print!("{}", meta::serialize_mapping(&ctx.config.to_mapping(), Format::Yaml)?);
        }
        // Key only: read that value from the linked config document.
        (Some(key), None) => {
            let ws = workspace(&ctx)?;
            match block_on(ws.config_get(&ctx.root_doc, key))? {
                Some(v) => match v.as_str() {
                    Some(s) => println!("{s}"),
                    None => println!("{}", meta::serialize_value(&v, Format::Yaml)?.trim_end()),
                },
                None => {
                    eprintln!("colophon: {key} is not set");
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        // Key + value: materialize/link the config document if needed, then set.
        (Some(key), Some(value)) => {
            let mut ctx = ctx;
            let config_doc = ensure_config(&mut ctx)?;
            let full = ctx.root_dir.join(&config_doc);
            let text = std::fs::read_to_string(&full)?;
            let doc = Document::parse(&config_doc, &text)?;
            let updated = edit::set_in_text(&text, doc.carrier, key, edit::infer_scalar(value))?;
            std::fs::write(&full, updated)?;
            println!("set {key} = {value} in {}", config_doc.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Ensure the workspace *declares* a config document, bootstrapping one when it
/// does not: create `colophon.<ext>` (in the workspace's metadata format) beside
/// the root (self-described with a title and a `part_of` back to the root, in
/// the workspace link style) and add the `config` pointer to the root's
/// metadata. Returns its path relative to the root. Mirrors [`ensure_registry`].
fn ensure_config(ctx: &mut Ctx) -> Result<PathBuf, AnyError> {
    let ws = workspace(ctx)?;
    if let Some(existing) = block_on(ws.config_path(&ctx.root_doc))? {
        return Ok(existing);
    }
    let format = ctx.config.default_embed_format;
    let config_name = sidecar_name(CONFIG_STEM, format);
    let config_rel = PathBuf::from(&config_name);
    let config_full = ctx.root_dir.join(&config_rel);
    if !config_full.exists() {
        // The root's title (or a title from its filename) labels the back-link.
        let root_full = ctx.root_dir.join(&ctx.root_doc);
        let root_title = std::fs::read_to_string(&root_full)
            .ok()
            .and_then(|t| Document::parse(&ctx.root_doc, &t).ok())
            .and_then(|d| d.meta.get("title").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| colophon::path_to_title(&ctx.root_doc));
        let part_of =
            colophon::format_link(ctx.config.link_format, &config_rel, &ctx.root_doc, &root_title);
        let mut seed = colophon::Mapping::new();
        seed.insert("title".into(), Value::String("colophon config".into()));
        seed.insert("part_of".into(), Value::String(part_of));
        std::fs::write(&config_full, meta::serialize_mapping(&seed, format)?)?;
    }
    // Link it from the root via the `config` relation.
    let root_full = ctx.root_dir.join(&ctx.root_doc);
    let text = std::fs::read_to_string(&root_full)?;
    let doc = Document::parse(&ctx.root_doc, &text)?;
    let updated = edit::set_in_text(&text, doc.carrier, "config", edit::infer_scalar(&config_name))?;
    std::fs::write(&root_full, updated)?;
    eprintln!("initialized {} (linked from {})", config_rel.display(), ctx.root_doc.display());
    Ok(config_rel)
}

fn cmd_backlinks(file: &Path) -> CmdResult {
    let ctx = find_root()?;
    let target = ws_rel(&ctx, file)?;
    let links = block_on(workspace(&ctx)?.backlinks_to(&ctx.root_doc, &target))?;
    for backlink in &links {
        let kind = if backlink.by_id { "id" } else { "path" };
        println!("{}\t{}\t{kind}", backlink.source.display(), backlink.site);
    }
    if links.is_empty() {
        eprintln!("no backlinks to {}", target.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_resolve(id: &str) -> CmdResult {
    let ctx = find_root()?;
    let ws = workspace(&ctx)?;
    let id = Id(id.strip_prefix(link::ID_SCHEME).unwrap_or(id).to_string());
    match ws.index().resolve(&id) {
        Some(path) => {
            println!("{}", path.display());
            Ok(ExitCode::SUCCESS)
        }
        None if ws.index().is_tombstoned(&id) => {
            eprintln!("colophon: {id} is tombstoned — its document was deleted");
            Ok(ExitCode::FAILURE)
        }
        None => {
            eprintln!("colophon: {id} is not in this workspace's registry");
            Ok(ExitCode::FAILURE)
        }
    }
}
