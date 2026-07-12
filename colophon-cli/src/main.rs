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
    ContentFormat, Document, EmbedStyle, FileIndex, Format, Id, IndexStore, LinkStyle, Mapping,
    Minter, Registration, RelationSet, StdFs, Trigger, Value, Workspace, WorkspaceConfig, block_on,
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
        /// How path links are written (default: markdown-root).
        #[arg(long, value_enum)]
        link_style: Option<LinkStyleArg>,
        /// When documents earn a stable ID: off (paths only), lazy (on
        /// link-by-id or publish), or eager (at creation) (default: lazy).
        #[arg(long, value_enum)]
        identity: Option<IdentityArg>,
        /// How colophon writes the links it authors: by path, or by stable id
        /// (requires `--identity` ≠ off) (default: path). Distinct from
        /// `--link-style`, which sets how *path* links are formatted.
        #[arg(long, value_enum)]
        links: Option<LinkKind>,
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

/// How colophon authors the structural links it writes — the `id_links` config
/// key, the *second* identity axis. `Path` writes readable path links (rewritten
/// on move); `Id` writes `colophon:<id>` targets that survive moves untouched.
/// Only meaningful when identity can mint on a link, so `init` gates the prompt
/// on `--identity` ≠ off.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LinkKind {
    Path,
    Id,
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
        Command::Init { dir, title, author, meta, embed, content, link_style, identity, links, yes } => {
            cmd_init(dir.as_deref(), title, author, meta, embed, content, link_style, identity, links, yes)
        }
        Command::Set { file, key, value } => cmd_set(&file, &key, &value),
        Command::Unset { file, key } => cmd_unset(&file, &key),
        Command::Tree { root } => cmd_tree(root.as_deref()),
        Command::Check { root, fix } => cmd_check(root.as_deref(), fix),
        Command::New { path, parent } => cmd_new(&path, &parent),
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
    let index = match &ctx.registry {
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
    };
    Ok(Workspace::builder(StdFs)
        .root(&ctx.root_dir)
        .identity(Minter::with(ctx.config.identity, entropy_seed()))
        .index(index)
        .link_style(ctx.config.link_format)
        .id_links(ctx.config.id_links)
        .default_embed_format(ctx.config.default_embed_format)
        .build())
}

/// Make sure the workspace *declares* a registry, bootstrapping one when it
/// does not: create `registry.<ext>` (in the workspace's metadata format) beside
/// the root (self-described with a title and a part_of back to the root) and add
/// the `registry` pointer to the root's metadata — comment-preservingly, like
/// any other edit.
fn ensure_registry(ctx: &mut Ctx) -> Result<(), AnyError> {
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
    link_style: Option<LinkStyleArg>,
    identity: Option<IdentityArg>,
    links: Option<LinkKind>,
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

    // Bail before prompting if this directory is already a workspace — a root
    // written by any content grammar (combined) or any metadata format (separate).
    for ext in ROOT_EXTS.iter().chain(META_EXTS) {
        let existing = dir.join(format!("index.{ext}"));
        if existing.exists() {
            return Err(format!(
                "{} already exists — this looks like an initialized workspace",
                existing.display()
            )
            .into());
        }
    }

    let default_title = link::path_to_title(&dir);
    // Prompt only on a real terminal and only when `--yes` wasn't passed.
    let interactive = !yes && std::io::stdin().is_terminal();
    // The link-authoring prompt only appears when identity isn't off, so it only
    // counts toward "will we prompt?" in that case — keeping intro/outro paired
    // with at least one real question.
    let links_prompt_possible = links.is_none() && identity != Some(IdentityArg::Off);
    let prompting = interactive
        && (title.is_none()
            || author.is_none()
            || content.is_none()
            || embed.is_none()
            || meta.is_none()
            || link_style.is_none()
            || identity.is_none()
            || links_prompt_possible);
    if prompting {
        cliclack::intro("colophon init")?;
    }

    // Each field: flag wins; else prompt when interactive; else the default.
    let title = match title {
        Some(t) if !t.is_empty() => t,
        _ if interactive => cliclack::input("Title")
            .default_input(&default_title)
            .placeholder(&default_title)
            .interact::<String>()?,
        _ => default_title,
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
    let link_style = match link_style {
        Some(l) => l,
        None if interactive => cliclack::select("Link style")
            .initial_value(LinkStyleArg::MarkdownRoot)
            .item(LinkStyleArg::MarkdownRoot, "Markdown, workspace-absolute", "[Title](/path.md)")
            .item(LinkStyleArg::MarkdownRelative, "Markdown, relative", "[Title](../path.md)")
            .item(LinkStyleArg::PlainRelative, "Plain relative path", "../path.md")
            .item(LinkStyleArg::PlainCanonical, "Plain workspace path", "path.md")
            .interact()?,
        None => LinkStyleArg::MarkdownRoot,
    };
    // Identity is two independent axes (DESIGN §4). First: *when* a document
    // earns a stable ID. Default lazy, matching `WorkspaceConfig::default()`.
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
    // Second: whether colophon *authors* links by ID. Only meaningful when an ID
    // can be minted on a link, so with identity off it is forced to path links
    // and the prompt is skipped. A `--links id` against `--identity off` is an
    // explicit contradiction — surface it rather than silently ignoring it.
    let id_links = if identity == IdentityArg::Off {
        if links == Some(LinkKind::Id) {
            return Err("`--links id` needs identity to mint IDs (try `--identity lazy`)".into());
        }
        false
    } else {
        match links {
            Some(l) => l == LinkKind::Id,
            None if interactive => {
                cliclack::select("Links between documents")
                    .initial_value(LinkKind::Path)
                    .item(LinkKind::Path, "By path", "readable links; colophon rewrites them when a file moves")
                    .item(LinkKind::Id, "By stable ID", "links never change; the registry tracks where each file lives")
                    .interact()?
                    == LinkKind::Id
            }
            None => false,
        }
    };

    // Assemble the workspace preferences these choices encode.
    let ws_config = WorkspaceConfig {
        link_format: link_style.into(),
        identity: identity.registration(),
        id_links,
        default_embed_format: meta.into(),
        embed_style: embed,
        content_format: content.into(),
    };

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
    let root_name = match carrier {
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

    let author_note = author.as_deref().map(|a| format!(", author {a}")).unwrap_or_default();
    let (embed_label, _) = embed_labels(embed);
    let details = format!(
        "root: {root_name} — {title}{author_note}\n\
         config: {config_name} — content {}, embed {} ({}), language {}, link style {}, identity {}, links {}",
        content.label(),
        embed.as_config_str(),
        embed_label.to_lowercase(),
        meta.label(),
        ws_config.link_format.as_config_str(),
        identity.label(),
        if id_links { "id" } else { "path" },
    );
    let next = format!("next: colophon new <path> --parent {root_name}");
    if prompting {
        cliclack::outro(format!("initialized {}\n{details}\n{next}", dir.display()))?;
    } else {
        println!("initialized {}", dir.display());
        for line in details.lines() {
            println!("  {line}");
        }
        println!("{next}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Prompt for the embed type, offering only the styles that suit `content` (the
/// first is the default). See [`ContentLang::embed_styles`].
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

fn cmd_check(root: Option<&Path>, fix: bool) -> CmdResult {
    let ctx = find_root()?;
    let root = match root {
        Some(r) => ws_rel(&ctx, r)?,
        None => ctx.root_doc.clone(),
    };
    let mut ws = workspace(&ctx)?;
    let findings = block_on(ws.check(&root))?;
    if fix {
        return cmd_check_fix(&mut ws, &findings);
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
    ws: &mut Workspace<StdFs, Minter, FileIndex>,
    findings: &[colophon::Finding],
) -> CmdResult {
    let mut applied = 0usize;
    let mut needs_attention = 0usize;
    let mut apply_all = false;
    for finding in findings {
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
    // Authoring id links (or an eager policy) mints IDs, so ensure a registry to
    // persist them exists *before* the workspace is built over it.
    let mints = (ctx.config.id_links && ctx.config.identity.fires_on(Trigger::Link))
        || ctx.config.identity.fires_on(Trigger::Create);
    if mints {
        ensure_registry(&mut ctx)?;
    }
    let mut ws = workspace(&ctx)?;
    let created = block_on(ws.create(&ws_rel(&ctx, path)?, &ws_rel(&ctx, parent)?))?;
    save_index(&ctx, &mut ws)?;
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

fn cmd_mv(from: &Path, to: &Path) -> CmdResult {
    let ctx = find_root()?;
    let mut ws = workspace(&ctx)?;
    block_on(ws.rename(&ws_rel(&ctx, from)?, &ws_rel(&ctx, to)?))?;
    save_index(&ctx, &mut ws)?;
    println!("moved {} -> {}", from.display(), to.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_rm(path: &Path, force: bool) -> CmdResult {
    let ctx = find_root()?;
    let mut ws = workspace(&ctx)?;
    let danglers = block_on(ws.delete(&ws_rel(&ctx, path)?, force))?;
    save_index(&ctx, &mut ws)?;
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
    save_index(&ctx, &mut ws)?;
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
