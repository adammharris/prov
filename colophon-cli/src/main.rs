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

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use colophon::tree::{Node, NodeKind};
use colophon::{
    Document, FileIndex, Format, Id, IndexStore, Minter, RelationSet, StdFs, Trigger, Value,
    Workspace, block_on, edit, link, meta,
};

/// Where the persistent ID registry lives, relative to the workspace root.
/// User-owned data in the tree, per DESIGN §6 — not an app-private dotfolder
/// format, just a fig-parseable snapshot anyone can read.
const INDEX_PATH: &str = ".colophon/index.yaml";

/// A self-describing plaintext workspace, from the command line.
#[derive(Parser)]
#[command(name = "colophon", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
        /// The root document to discover from.
        root: PathBuf,
    },
    /// Check workspace integrity from a root: broken links, case mismatches,
    /// duplicate containment, missing inverse links. Exits 1 on findings.
    Check {
        /// The root document to check from.
        root: PathBuf,
    },
    /// Create a document as a child of a parent, linking both directions.
    New {
        /// Path of the document to create.
        path: PathBuf,
        /// The parent document (gains a spanning link to the new file).
        #[arg(long, short)]
        parent: PathBuf,
    },
    /// Move/rename a document, maintaining every affected link (parent entry,
    /// children's inverse links, and the document's own relative links).
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
    /// Registers it in .colophon/index.yaml on first use — link that target
    /// from any document and it survives moves.
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
}

/// CLI spelling of the metadata formats colophon compiles in.
#[derive(Clone, Copy, ValueEnum)]
enum MetaFormat {
    Yaml,
    Json,
    Fig,
}

impl From<MetaFormat> for Format {
    fn from(f: MetaFormat) -> Format {
        match f {
            MetaFormat::Yaml => Format::Yaml,
            MetaFormat::Json => Format::Json,
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
        Command::Set { file, key, value } => cmd_set(&file, &key, &value),
        Command::Unset { file, key } => cmd_unset(&file, &key),
        Command::Tree { root } => cmd_tree(&root),
        Command::Check { root } => cmd_check(&root),
        Command::New { path, parent } => cmd_new(&path, &parent),
        Command::Mv { from, to } => cmd_mv(&from, &to),
        Command::Rm { path, force } => cmd_rm(&path, force),
        Command::Id { file } => cmd_id(&file),
        Command::Resolve { id } => cmd_resolve(&id),
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

/// The workspace the multi-document commands drive: the process filesystem
/// rooted at the current directory, a lazy identity policy, and the persistent
/// registry loaded from [`INDEX_PATH`] (empty when absent — the registry only
/// materializes on disk once something registers).
fn workspace() -> Result<Workspace<StdFs, Minter, FileIndex>, Box<dyn std::error::Error>> {
    let index = match std::fs::read_to_string(INDEX_PATH) {
        Ok(text) => FileIndex::from_str(&text, Format::Yaml)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileIndex::new(Format::Yaml),
        Err(e) => return Err(e.into()),
    };
    Ok(Workspace::builder(StdFs)
        .identity(Minter::lazy(entropy_seed()))
        .index(index)
        .build())
}

/// Persist the registry when a command changed it.
fn save_index(ws: &mut Workspace<StdFs, Minter, FileIndex>) -> Result<(), Box<dyn std::error::Error>> {
    if !ws.index().is_dirty() {
        return Ok(());
    }
    if let Some(dir) = Path::new(INDEX_PATH).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(INDEX_PATH, ws.index().render()?)?;
    ws.index_mut().mark_clean();
    Ok(())
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
        doc.embed.map(|k| k.inner_format()).unwrap_or(Format::Yaml)
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
            let format = doc.embed.map(|k| k.inner_format()).unwrap_or(Format::Yaml);
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

fn cmd_set(file: &Path, key: &str, value: &str) -> CmdResult {
    let text = std::fs::read_to_string(file)?;
    let updated = edit::set_in_text(&text, key, edit::infer_scalar(value))?;
    std::fs::write(file, updated)?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_unset(file: &Path, key: &str) -> CmdResult {
    let text = std::fs::read_to_string(file)?;
    let updated = edit::unset_in_text(&text, key)?;
    std::fs::write(file, updated)?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_tree(root: &Path) -> CmdResult {
    let node = block_on(workspace()?.tree(root))?;
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

fn cmd_check(root: &Path) -> CmdResult {
    let findings = block_on(workspace()?.check(root))?;
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

fn cmd_new(path: &Path, parent: &Path) -> CmdResult {
    let mut ws = workspace()?;
    block_on(ws.create(path, parent))?;
    save_index(&mut ws)?;
    println!("created {} (in {})", path.display(), parent.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_mv(from: &Path, to: &Path) -> CmdResult {
    let mut ws = workspace()?;
    block_on(ws.rename(from, to))?;
    save_index(&mut ws)?;
    println!("moved {} -> {}", from.display(), to.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_rm(path: &Path, force: bool) -> CmdResult {
    let mut ws = workspace()?;
    block_on(ws.delete(path, force))?;
    save_index(&mut ws)?;
    println!("deleted {}", path.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_id(file: &Path) -> CmdResult {
    let mut ws = workspace()?;
    let id = block_on(ws.register(file, Trigger::Link))?;
    save_index(&mut ws)?;
    println!("{}", link::id_target(&id));
    Ok(ExitCode::SUCCESS)
}

fn cmd_resolve(id: &str) -> CmdResult {
    let ws = workspace()?;
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
