//! `prov` — command-line companion for the prov library.
//!
//! A thin adapter: parse arguments, call into the library, render the result.
//! The workspace *semantics* — discovery, bootstrap, the mutation engine — live
//! in `prov`; this crate is argument parsing, session plumbing, and
//! presentation.
//!
//! The crate is split across three modules to keep each legible:
//!
//! - [`cli`] — the `clap` argument grammar and the enums that mirror the
//!   library's config axes (the CLI *spelling* of each concept).
//! - [`init`] — the `init` command and its interactive intake (the one command
//!   that *creates* a workspace, and the largest).
//! - `main` (here) — the dispatcher, the session layer that discovers the
//!   workspace ([`find_root`]) and drives the library's [`prov::StdFs`]-backed
//!   engine through the dependency-free [`prov::block_on`] executor, and the
//!   remaining command handlers.
//!
//! Single-document commands (`show`, `links`, `meta`, `get`, `body`, `set`,
//! `unset`) operate on the pure layers and need no workspace; workspace commands
//! (`tree`, `check`, `new`, `mv`, `rm`, …) discover a root first.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use prov::document::MetaCarrier;
use prov::tree::{Node, NodeKind};
use prov::{
    Addressing, Adoption, ChangeSet, ContentFormat, Document, EmbedStyle, FileIndex, Format, Id,
    IdStorage, IndexStore, Layout, LinkStyle, Mapping, Minter, Notation, PathStyle, RelationSet,
    RoutePlan, StdFs, StructurePlan, SynthNode, Target, Trigger, Value, Workspace, WorkspaceConfig,
    block_on, edit, link, meta,
};

mod cli;
use cli::*;

fn main() -> ExitCode {
    let cli = Cli::parse();
    // `-C <dir>` / `--root <dir>` (or `PROV_ROOT`, which it overrides) runs prov
    // as if it had started in that directory: chdir once, up front, so every
    // downstream `current_dir()`-based root discovery and every relative path
    // argument resolves there — the `git -C` model, in one place.
    if let Some(dir) = cli
        .root
        .clone()
        .or_else(|| std::env::var_os("PROV_ROOT").map(PathBuf::from))
        && let Err(e) = std::env::set_current_dir(&dir)
    {
        eprintln!("prov: could not use root directory {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    let result = match cli.command {
        Command::Show { file } => resolve_target(&file).and_then(|f| cmd_show(&f)),
        Command::Links { file, relation } => {
            resolve_target(&file).and_then(|f| cmd_links(&f, relation.as_deref()))
        }
        Command::Meta { file, format } => resolve_target(&file).and_then(|f| cmd_meta(&f, format)),
        Command::Get { file, key } => resolve_target(&file).and_then(|f| cmd_get(&f, &key)),
        Command::Body { file } => resolve_target(&file).and_then(|f| cmd_body(&f)),
        Command::Render { file } => resolve_target(&file).and_then(|f| cmd_render(&f)),
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
            fixity,
            no_recycle_bin,
            updated_field,
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
            fixity,
            no_recycle_bin,
            updated_field,
            adopt,
            attach,
            yes,
        ),
        Command::Edit { file } => resolve_target(&file).and_then(|f| cmd_edit(&f)),
        Command::Set { file, key, value } => {
            resolve_target(&file).and_then(|f| cmd_set(&f, &key, &value))
        }
        Command::Unset { file, key } => resolve_target(&file).and_then(|f| cmd_unset(&f, &key)),
        Command::Tree { root } => root
            .map(|r| resolve_target(&r))
            .transpose()
            .and_then(|r| cmd_tree(r.as_deref())),
        Command::Explore { file } => cmd_explore(file.as_deref()),
        Command::Check { root, fix } => root
            .map(|r| resolve_target(&r))
            .transpose()
            .and_then(|r| cmd_check(r.as_deref(), fix)),
        Command::New {
            title,
            in_target,
            parents,
            layout,
            dry_run,
            as_path,
            ext,
        } => cmd_new(
            &title,
            &in_target,
            parents,
            layout.into(),
            dry_run,
            as_path.as_deref(),
            ext.as_deref(),
        ),
        Command::Attach {
            payload,
            in_target,
            parents,
            layout,
            all,
            recursive,
        } => cmd_attach(
            payload.as_deref(),
            in_target.as_deref(),
            parents,
            layout.into(),
            all,
            recursive,
        ),
        Command::Mv {
            from,
            to,
            in_target,
            parents,
            layout,
        } => cmd_mv(&from, &to, in_target.as_deref(), parents, layout.into()),
        Command::Reparent {
            path,
            in_target,
            parents,
            layout,
            dry_run,
        } => cmd_reparent(&path, &in_target, parents, layout.into(), dry_run),
        Command::Rm { path, force, purge } => cmd_rm(&path, force, purge),
        Command::Restore { path } => cmd_restore(&path),
        Command::EmptyBin => cmd_empty_bin(),
        Command::Duplicate { source } => cmd_duplicate(&source),
        Command::Convert {
            file,
            axis,
            value,
            recursive,
        } => resolve_target(&file).and_then(|f| cmd_convert(&f, &axis, &value, recursive)),
        Command::Id { file } => resolve_target(&file).and_then(|f| cmd_id(&f)),
        Command::Resolve { id } => cmd_resolve(&id),
        Command::Backlinks { file } => resolve_target(&file).and_then(|f| cmd_backlinks(&f)),
        Command::Config {
            key,
            value,
            setup,
            home,
        } => cmd_config(key.as_deref(), value.as_deref(), setup, home),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("prov: {err}");
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

/// Resolve the workspace root and, on success, warn (once, to stderr) about any
/// config a command would otherwise run past silently — settings prov would
/// ignore, or a config `spec` newer than this build. Suppressed by
/// `PROV_QUIET`. Commands that already report config in full
/// (`check`, `config`) use [`find_root_quiet`] instead.
fn find_root() -> Result<Ctx, AnyError> {
    let ctx = find_root_quiet()?;
    warn_config(&ctx);
    Ok(ctx)
}

/// Warn about config that will not take effect — the proactive counterpart to
/// `check`'s [`prov::Finding::ConfigIssue`]. One stderr line summarizing
/// settings prov would silently ignore (a typo or unrecognized value across
/// either config surface), and one for a `spec` this build is too old to fully
/// read. Quiet when the config is clean, or when `PROV_QUIET` is set.
fn warn_config(ctx: &Ctx) {
    if std::env::var_os("PROV_QUIET").is_some() {
        return;
    }
    let mut issues = Vec::new();
    let mut spec_ahead = None;
    // The root's `prov:` block.
    if let Ok(text) = std::fs::read_to_string(ctx.root_dir.join(&ctx.root_doc))
        && let Ok(doc) = Document::parse(&ctx.root_doc, &text)
        && let Some(block) = doc.meta.get(prov::config::ROOT_CONFIG_KEY)
    {
        issues.extend(prov::diagnose(block));
        spec_ahead = spec_ahead.or_else(|| prov::spec_ahead(block));
    }
    // The dedicated config document.
    let probe: Workspace<StdFs> = Workspace::builder(StdFs).root(&ctx.root_dir).build();
    if let Ok(Some(config_doc)) = block_on(probe.config_path(&ctx.root_doc))
        && let Ok(text) = std::fs::read_to_string(ctx.root_dir.join(&config_doc))
        && let Ok(doc) = Document::parse(&config_doc, &text)
    {
        issues.extend(prov::diagnose(&doc.meta));
        spec_ahead = spec_ahead.or_else(|| prov::spec_ahead(&doc.meta));
    }
    if let Some(declared) = spec_ahead {
        eprintln!(
            "prov: config declares spec {declared} but this build understands spec {} — newer settings may be ignored (upgrade prov)",
            prov::config::SPEC_VERSION
        );
    }
    if let Some(first) = issues.first() {
        eprintln!(
            "prov: {} config setting(s) will be ignored (e.g. `{}`) — run `prov check` for details",
            issues.len(),
            first.key
        );
    }
}

/// Find the workspace root by walking up from the current directory. The walk,
/// the root-candidate rule, and the tie-breaking all live in the library
/// ([`prov::discover`]); this only supplies the real current directory and
/// phrases the two failure modes as CLI diagnostics. Does not warn about config —
/// see [`find_root`].
fn find_root_quiet() -> Result<Ctx, AnyError> {
    let cwd = std::env::current_dir()?;
    match block_on(prov::discover(&StdFs, &cwd))? {
        prov::Discovery::Found(d) => Ok(Ctx {
            root_dir: d.root_dir,
            root_doc: d.root_doc,
            registry: d.registry,
            config: d.config,
        }),
        prov::Discovery::Ambiguous { dir, candidates } => Err(format!(
            "ambiguous workspace root in {}: {} (rename one, or add part_of)",
            dir.display(),
            candidates.join(", ")
        )
        .into()),
        prov::Discovery::NotFound => Err(
            "no workspace root found: no ancestor directory has a document \
with metadata and no part_of\n\
\n\
  If this directory holds content already, run `prov init` here to adopt it\n\
  (use `prov init --adopt` to link existing files in non-interactively).\n\
  Otherwise `prov init` starts a fresh workspace."
                .into(),
        ),
    }
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
    // The relation vocabulary is derived from the config: its declared
    // definitions + spanning (or the diaryx preset when none are declared),
    // with per-relation `style` overrides (up≠down) overlaid.
    let relations = RelationSet::from_config(&ctx.config);
    Ok(Workspace::builder(StdFs)
        .root(&ctx.root_dir)
        .relations(relations)
        .identity(Minter::with(ctx.config.identity, entropy_seed()))
        .index(index)
        .link_style(ctx.config.link_format())
        // `reference_style` is explicit here, so it supersedes the builder's
        // `id_links` fallback entirely — the CLI never sets that legacy axis.
        .reference_style(ctx.config.reference_style())
        .default_embed_format(ctx.config.default_embed_format)
        .fixity(ctx.config.fixity)
        .build())
}

/// Make sure the workspace *declares* a registry, bootstrapping one when it
/// does not: create `registry.<ext>` (in the workspace's metadata format) beside
/// the root (self-described with a title and a part_of back to the root) and add
/// the `registry` pointer to the root's metadata — comment-preservingly, like
/// any other edit.
///
/// Two files, so one [`ChangeSet`]: a bootstrap that wrote the registry document
/// but failed to point the root at it would leave a registry no scan can find —
/// invisible, and silently re-bootstrapped (over) next run.
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

    // Seed: a self-describing node titled "ID registry". Machinery is reached
    // *one-way* through the root's `registry` pointer, so it carries no `part_of`
    // back-link — that would assert a spanning-tree membership it does not have
    // (DESIGN §5, "link target kinds"). The crash-safe "create sidecar + point the
    // root at it" landing lives in the library ([`Workspace::link_sidecar`]).
    let mut seed = Mapping::new();
    seed.insert("title".into(), Value::String("ID registry".into()));
    let probe: Workspace<StdFs> = Workspace::builder(StdFs).root(&ctx.root_dir).build();
    let created =
        block_on(probe.link_sidecar(&ctx.root_doc, "registry", &registry_rel, &seed, format))?;
    if created {
        eprintln!(
            "initialized {} (linked from {})",
            registry_rel.display(),
            ctx.root_doc.display()
        );
    }
    ctx.registry = Some(registry_rel);
    Ok(())
}

/// Persist the registry when a mutation could not stage it itself.
///
/// Normally this does nothing: the library stages the registry write into the
/// same change set as the documents whose links it describes, so by the time a
/// command returns, the index is already clean. The exception is a workspace
/// with no registry document *yet* — `check --fix` deliberately declines to
/// bootstrap one until a fix has actually minted an ID, so the index it dirtied
/// had nowhere to stage to. Give it its new home and write it.
fn save_index(ctx: &Ctx, ws: &mut Workspace<StdFs, Minter, FileIndex>) -> Result<(), AnyError> {
    if !ws.index().is_dirty() {
        return Ok(());
    }
    let Some(rel) = &ctx.registry else {
        return Err("the registry changed but no registry document is declared".into());
    };
    let full = ctx.root_dir.join(rel);
    let host_text = match std::fs::read_to_string(&full) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    ws.index_mut().set_host(rel, &host_text)?;
    let Some((path, rendered)) = ws.index_mut().pending_write()? else {
        return Ok(());
    };
    let mut cs = ChangeSet::new();
    cs.write(path, rendered);
    block_on(cs.apply(&StdFs, &ctx.root_dir))?;
    ws.index_mut().committed(true);
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
    let pairs: Vec<(Id, PathBuf)> = ws
        .index()
        .iter()
        .map(|(id, path)| (id.clone(), path.clone()))
        .collect();
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

/// How a CLI argument names a document — the addressing mode carried by the
/// *value*, not by which flag it was passed to.
///
/// This mirrors the library's [`Addressing`](prov::Addressing) (`Path`/`Id`/
/// `Alias`) and its `Link::parse`, which have always disambiguated a target by its
/// own syntax. The CLI briefly did it with flag names instead (`--in-path` vs
/// `--in-title`), which cost a flag per mode per argument and could only ever be
/// afforded on *one* argument — the parent — leaving every subject path-only. A
/// grammar costs one flag total and works in every slot, including subjects.
///
/// The spellings are chosen so a bare path stays a bare path: `id:` is the
/// library's own [`ID_SCHEME`](prov::link::ID_SCHEME), and `@` is not legal at
/// the start of a *relative* path anyone writes by habit. A file genuinely named
/// `@foo.md` is still addressable as `./@foo.md`, which parses as a path.
#[derive(Debug, PartialEq, Eq)]
enum TargetSpec<'a> {
    /// A filesystem path — the default, and the only mode that needs no workspace.
    Path(&'a str),
    /// `id:<id>` (or the legacy `prov:<id>`) — resolved through the registry.
    Id(&'a str),
    /// `@Daily/2026/08` — a route of titles walked from the workspace root. Bare
    /// `@` is the root document itself.
    Route(&'a str),
}

/// Classify a CLI target. Pure text: no filesystem, no workspace, no guessing —
/// the string says which mode it is or it is a path.
fn parse_target(s: &str) -> TargetSpec<'_> {
    if let Some(id) = link::strip_id_scheme(s) {
        return TargetSpec::Id(id);
    }
    match s.strip_prefix('@') {
        Some(route) => TargetSpec::Route(route),
        None => TargetSpec::Path(s),
    }
}

/// Resolve a target that names an *existing* document, to a path this process can
/// open (absolute for id/route, as-written for a path).
///
/// Root discovery is **lazy**: a plain path resolves without one, so `show`,
/// `meta`, `get`, `body`, `links`, `render`, `set`, and `unset` keep working on any
/// file anywhere — outside a workspace, in a tarball, wherever. Only `@` and `id:`
/// need a workspace, and only then is one discovered. That property is worth
/// keeping: those commands read a *file*, and only the other modes make the
/// argument mean a *node*.
fn resolve_target(s: &str) -> Result<PathBuf, AnyError> {
    match parse_target(s) {
        TargetSpec::Path(p) => Ok(PathBuf::from(p)),
        TargetSpec::Id(id) => {
            let ctx = find_root()?;
            let ws = workspace(&ctx)?;
            let id = Id(id.to_string());
            match ws.index().resolve(&id) {
                Some(path) => Ok(ctx.root_dir.join(path)),
                None if ws.index().is_tombstoned(&id) => {
                    Err(format!("{id} is tombstoned — its document was deleted").into())
                }
                None => Err(format!("{id} is not in the registry").into()),
            }
        }
        TargetSpec::Route(route) => {
            let ctx = find_root()?;
            let ws = workspace(&ctx)?;
            let terminal = resolve_route(&ctx, &ws, route)?;
            Ok(ctx.root_dir.join(terminal))
        }
    }
}

/// Walk a route of titles to an existing node, workspace-relative. Refuses to
/// create: a *subject* that does not exist is a mistake, never an instruction —
/// only a `--in` destination may be synthesized, and only with `-p`.
fn resolve_route(
    ctx: &Ctx,
    ws: &Workspace<StdFs, Minter, FileIndex>,
    route: &str,
) -> Result<PathBuf, AnyError> {
    let segments = Workspace::<StdFs>::route_segments(route);
    let plan = block_on(ws.plan_route(&ctx.root_doc, &segments, Layout::Nested))?;
    if !plan.is_complete() {
        let missing = &plan.synthesize[0];
        return Err(format!(
            "@{route} stops at {}: no child titled {:?}",
            missing.parent.display(),
            missing.title,
        )
        .into());
    }
    Ok(plan.terminal)
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

mod init;
use init::cmd_init;

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
    let format = format
        .map(Format::from)
        .unwrap_or_else(|| doc.carrier.map(|c| c.format()).unwrap_or(Format::Yaml));
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
    let format = prov::ContentFormat::from_extension(file).ok_or_else(|| {
        format!(
            "{}: not a recognized body format (expected .md/.markdown or .dj/.djot)",
            file.display()
        )
    })?;
    let html = prov::render_html(&doc.body, format)?;
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

fn cmd_edit(file: &Path) -> CmdResult {
    // Snapshot before the editor so we can tell whether the user actually changed
    // anything — an open-and-quit must not bump the timestamp or restamp.
    let before = std::fs::read(file).ok();
    edit_file(file)?;
    let changed = std::fs::read(file).ok() != before;

    let ctx = find_root()?;
    let rel = ws_rel(&ctx, file)?;
    if !changed {
        println!("edited {} (no changes)", rel.display());
        return Ok(ExitCode::SUCCESS);
    }

    // The bookkeeping a real edit implies, in one crash-safe write: restamp the
    // body checksum (under `full`), and stamp the `updated` field (when
    // configured) with the current time — RFC 3339 UTC, the machine-standard
    // value the library reads back (DESIGN §2). Both self-gate, so this is a
    // no-op when neither is enabled.
    let mut ws = workspace(&ctx)?;
    let now = now_rfc3339();
    let updated =
        (!ctx.config.updated.is_empty()).then_some((ctx.config.updated.as_str(), now.as_str()));
    let wrote = block_on(ws.record_content_update(&rel, updated))?;
    persist(&ctx, &mut ws)?;

    match (wrote, updated.is_some()) {
        (true, true) => println!(
            "edited {} — stamped `{}` + checksum",
            rel.display(),
            ctx.config.updated
        ),
        (true, false) => println!("edited {} — content checksum updated", rel.display()),
        (false, true) => println!(
            "edited {} — stamped `{}`",
            rel.display(),
            ctx.config.updated
        ),
        _ => println!("edited {}", rel.display()),
    }
    Ok(ExitCode::SUCCESS)
}

/// The current time as an RFC 3339 UTC timestamp (`2026-07-16T14:30:00Z`) — the
/// machine-standard value prov stores for provenance fields like `updated`
/// (DESIGN §2). Hand-rolled from the system clock rather than pulling in a date
/// crate, in the spirit of the dependency-free SHA-256 and journal checksum. A
/// pre-epoch clock (only a badly-wrong system) formats as the epoch.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs_to_rfc3339(secs)
}

/// Format seconds-since-Unix-epoch as an RFC 3339 UTC timestamp. Split out from
/// [`now_rfc3339`] so the calendar arithmetic is testable without a clock.
fn secs_to_rfc3339(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Days since 1970-01-01 → (year, month, day), by Howard Hinnant's civil-calendar
/// algorithm — exact for the whole proleptic Gregorian range, no leap-year
/// special-casing beyond the era arithmetic.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64; // day-of-era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day-of-year [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { y + 1 } else { y }, month, day)
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
                eprintln!("prov: cannot open {}: {e}", current.display());
                match history.pop() {
                    Some(prev) => {
                        current = prev;
                        continue;
                    }
                    None => return Ok(ExitCode::FAILURE),
                }
            }
        };
        let title = doc
            .meta
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // Build the menu: view/edit, every forward link (by relation), every
        // backlink, then navigation.
        let mut actions: Vec<(String, String, ExploreAction)> = Vec::new();
        actions.push((
            "View this document".into(),
            "page the raw file".into(),
            ExploreAction::View,
        ));
        actions.push(("Edit in $EDITOR".into(), String::new(), ExploreAction::Edit));

        // Documents already reachable from this screen by a forward link. A
        // backlink whose source is in this set is the inverse of a link we
        // already show — the child's `part_of` mirroring our `contents`, most
        // often — and navigates to the same place, so it is suppressed below to
        // keep a folder-note's menu from listing every child twice.
        let mut forward_targets: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();

        for relation in ws.relations().relations() {
            let Some(value) = doc.meta.get(&relation.name) else {
                continue;
            };
            for raw in value.link_strings() {
                let parsed = link::Link::parse(&raw);
                let (label, action) = match ws.resolve_link_with(&current, &parsed, Some(&titles)) {
                    Target::Path(p) => {
                        let t = doc_title(&ctx, &p);
                        forward_targets.insert(p.clone());
                        (
                            format!("{}: {t}  ({})", relation.name, p.display()),
                            ExploreAction::Goto(p),
                        )
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
                // Skip the inverse of a forward link already on this screen — the
                // same document, reached the same way (a child's `part_of` echoing
                // our `contents`). Genuinely-new backlinks (a `related` from a
                // document we don't link to) are unaffected.
                if forward_targets.contains(&backlink.source) {
                    continue;
                }
                let by = if backlink.by_id { "id" } else { "path" };
                actions.push((
                    format!("← {} [{}]", backlink.source.display(), backlink.site),
                    format!("linked from, by {by}"),
                    ExploreAction::Goto(backlink.source.clone()),
                ));
            }
        }

        if !history.is_empty() {
            actions.push((
                "Back".into(),
                "the previous document".into(),
                ExploreAction::Back,
            ));
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
            ExploreAction::Note(message) => eprintln!("prov: {message}"),
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
        .and_then(|(_, d)| {
            d.meta
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
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
    std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status()?;
    Ok(())
}

fn cmd_check(root: Option<&Path>, fix: bool) -> CmdResult {
    // `check` reports config issues in full (Finding::ConfigIssue), so skip the
    // one-line find_root warning that would just duplicate them.
    let mut ctx = find_root_quiet()?;
    // Heal first, validate second: if a mutation was interrupted by a crash, a
    // write-ahead journal is on disk. Roll it forward before reading the
    // workspace, so `check` reports on a consistent tree — and so the recovery
    // that `Error::Torn` points here to perform actually happens.
    match block_on(prov::recover(&StdFs, &ctx.root_dir))? {
        prov::Recovered::Applied(n) => {
            println!("recovered an interrupted change: rolled {n} op(s) forward from the journal");
        }
        prov::Recovered::Nothing => {}
    }
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
    findings: &[prov::Finding],
) -> CmdResult {
    let mut applied = 0usize;
    let mut needs_attention = 0usize;
    let mut apply_all = false;
    for finding in findings {
        // An orphan has no metadata-only `Fix` (nothing in the document is
        // wrong); repairing it means *adopting* it under a parent. Offer the
        // workspace root as that parent — the same flat adoption `init --adopt`
        // performs — since a batch fix can't ask which subtree it belongs to.
        if let prov::Finding::Orphan { doc } = finding {
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

/// Print a route plan without applying it: what resolved, what is missing, and
/// where the missing nodes would land. Shared by `--dry-run` and the error a
/// missing route raises without `-p`, so the two describe the plan identically.
/// Resolve a `--in DOC` / `--under ROUTE` placement to the parent document it
/// names. `Ok(None)` means the caller should stop having already printed
/// something the user asked for (a `--dry-run` preview).
///
/// Shared by `new`, `reparent`, and `mv`, because a route is only ever another
/// way to *name* a parent — never a different kind of operation. Extracted at the
/// third caller rather than the second: `new` alone justified nothing, but three
/// copies of the -p/--dry-run policy would drift, and the drift would be silent
/// (each command deciding on its own what a missing segment means).
///
/// Synthesized nodes are `create`d, so they mint IDs on the same terms as any
/// other document — a caller that mints must `ensure_registry` *before* this runs,
/// not merely before its own write.
fn resolve_placement(
    ctx: &Ctx,
    ws: &mut Workspace<StdFs, Minter, FileIndex>,
    target: &str,
    parents: bool,
    layout: Layout,
    dry_run: bool,
) -> Result<Option<PathBuf>, AnyError> {
    let route = match parse_target(target) {
        // A path or an id names a parent that must already exist — it has no
        // segments to synthesize. `-p` still applies to the *leaf* (idempotent
        // create, handled by the caller), so it is allowed here, just inert for
        // the parent.
        TargetSpec::Path(_) | TargetSpec::Id(_) => {
            let resolved = resolve_target(target)?;
            return Ok(Some(ws_rel(ctx, &resolved)?));
        }
        TargetSpec::Route(route) => route,
    };
    let segments = Workspace::<StdFs>::route_segments(route);
    let plan = block_on(ws.plan_route(&ctx.root_doc, &segments, layout))?;
    if dry_run {
        show_route_plan(route, &plan);
        if plan.is_complete() {
            println!(
                "\nnothing to create; the route resolves to {}",
                plan.terminal.display()
            );
        } else if !parents {
            println!(
                "\n{} segment(s) missing — re-run with -p to create them",
                plan.synthesize.len()
            );
        }
        return Ok(None);
    }
    if !plan.is_complete() && !parents {
        // Name the first missing segment and where the walk got to: the useful
        // half of the error is *how far* the route resolved.
        let missing = &plan.synthesize[0];
        return Err(format!(
            "@{route} stops at {}: no child titled {:?}\n\
             re-run with -p to create the missing segment(s), or --dry-run to preview",
            missing.parent.display(),
            missing.title,
        )
        .into());
    }
    let created = plan.synthesize.len();
    let terminal = block_on(ws.apply_route(&plan))?;
    for synth in &plan.synthesize {
        println!("created {} ({:?})", synth.path.display(), synth.title);
    }
    if created > 0 {
        persist(ctx, ws)?;
    }
    Ok(Some(terminal))
}

fn show_route_plan(route: &str, plan: &RoutePlan) {
    println!("route {route:?}");
    for (depth, node) in plan.resolved.iter().enumerate() {
        println!(
            "  {:indent$}{} (exists)",
            "",
            node.display(),
            indent = depth * 2
        );
    }
    let base = plan.resolved.len();
    for (depth, synth) in plan.synthesize.iter().enumerate() {
        println!(
            "  {:indent$}{} (create, titled {:?})",
            "",
            synth.path.display(),
            synth.title,
            indent = (base + depth) * 2
        );
    }
}

/// Create a document under a parent named by `--in` — a path, an `id:` handle,
/// or an `@`-route through the containment tree (optionally with `-p` to create
/// the route segments that don't exist yet). The addressing mode is carried by
/// the value itself (see [`parse_target`]), not by a per-mode flag.
#[allow(clippy::too_many_arguments)]
fn cmd_new(
    title: &str,
    in_target: &str,
    parents: bool,
    layout: Layout,
    dry_run: bool,
    as_path: Option<&Path>,
    ext: Option<&str>,
) -> CmdResult {
    let mut ctx = find_root()?;
    // Authoring a reference that registers (the default style, or any relation's
    // override — e.g. `part_of: id` in a split) mints IDs, as does an eager
    // policy; ensure a registry to persist them exists *before* the workspace is
    // built over it. A route's synthesized nodes are `create`d too, so they mint
    // on the same terms — the registry has to exist before the route is applied,
    // not just before the leaf.
    let mints = ctx.config.mints_on_mutation();
    if mints && !dry_run {
        ensure_registry(&mut ctx)?;
    }

    // Resolve the parent. A path `--in` is already a path; a `@`-route walks the
    // tree from the root, and (with `-p`) creates what it doesn't find. Either way
    // the rest of this function is unchanged — a route is just another way to
    // *name* a parent, never a different kind of creation.
    let mut ws = workspace(&ctx)?;
    let Some(parent_rel) = resolve_placement(&ctx, &mut ws, in_target, parents, layout, dry_run)?
    else {
        return Ok(ExitCode::SUCCESS);
    };

    // The new document's path: an explicit `--as` wins; otherwise a readable
    // filename derived from the title — `slug(title).<ext>` beside the parent,
    // where the extension is `--ext` or the workspace's content format. The title
    // itself is always recorded in metadata (structure lives there, not the name).
    let path = match as_path {
        Some(p) => ws_rel(&ctx, p)?,
        None => {
            let extension = ext
                .map(str::to_owned)
                .unwrap_or_else(|| ctx.config.content_format.extension().to_string());
            let name = format!("{}.{extension}", link::slug(title));
            parent_rel.parent().unwrap_or(Path::new("")).join(name)
        }
    };
    // Leaf idempotency (`-p`): a target that already exists as the *same*
    // document (same title) is a no-op — `mkdir -p` for the leaf, completing the
    // route-parent `-p` above, so a daily-note cron can re-run the same command.
    // A path held by a *different*-titled document is a real collision and still
    // errors. Without `-p`, an existing leaf errors as before (via `create`).
    if parents && ws.fs_path(&path).exists() {
        let (_, existing) = load(&ws.fs_path(&path))?;
        if existing.meta.get("title").and_then(Value::as_str) != Some(title) {
            return Err(format!(
                "{} already exists with a different title — refusing to reuse it \
                 (pick another title, or --as to name a different file)",
                path.display()
            )
            .into());
        }
        if dry_run {
            println!(
                "exists: {} (in {}) — no-op",
                path.display(),
                parent_rel.display()
            );
            return Ok(ExitCode::SUCCESS);
        }
        // Ensure the containment link both ways (idempotent; refuses a contested
        // parent), so an existing-but-unlinked file converges too.
        block_on(ws.adopt(&path, &parent_rel))?;
        persist(&ctx, &mut ws)?;
        println!("exists: {} (in {})", path.display(), parent_rel.display());
        return Ok(ExitCode::SUCCESS);
    }
    if dry_run {
        println!(
            "would create {} (in {})",
            path.display(),
            parent_rel.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    // (`ws` is the one built above — reusing it keeps any IDs a route just minted
    // in the same in-memory index this create registers into.)
    let created = block_on(ws.create_with_title(&path, &parent_rel, title))?;
    persist(&ctx, &mut ws)?;
    // A separated child is a pair — the metadata node the parent links, plus its
    // prose body file. Name both so it is clear two files were written.
    match &created.body {
        Some(body) => {
            println!(
                "created {} (in {})",
                created.node.display(),
                parent_rel.display()
            );
            println!("  body: {}", body.display());
        }
        None => println!(
            "created {} (in {})",
            created.node.display(),
            parent_rel.display()
        ),
    }
    Ok(ExitCode::SUCCESS)
}

/// Attach an arbitrary file — or, with `--all`, every loose file under the
/// workspace — minting a metadata sidecar and linking it under a parent
/// (default: the workspace root). Mirrors [`cmd_new`] — an id-registering
/// reference style or an eager policy mints IDs, so a registry is ensured first.
#[allow(clippy::too_many_arguments)]
fn cmd_attach(
    payload: Option<&Path>,
    in_target: Option<&str>,
    parents: bool,
    layout: Layout,
    all: bool,
    recursive: bool,
) -> CmdResult {
    let mut ctx = find_root()?;
    let mints = ctx.config.mints_on_mutation();
    if mints {
        ensure_registry(&mut ctx)?;
    }
    if recursive && !all {
        return Err("--recursive only applies with --all".into());
    }
    let mut ws = workspace(&ctx)?;
    // Default the parent to the workspace root — the common "attach this to my
    // workspace" case names no parent at all. Otherwise it is resolved exactly as
    // every other command resolves one, so an `@`-route `--in` works here too.
    let parent_rel = match in_target {
        None => ctx.root_doc.clone(),
        Some(t) => match resolve_placement(&ctx, &mut ws, t, parents, layout, false)? {
            Some(p) => p,
            None => return Ok(ExitCode::SUCCESS),
        },
    };

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
                Err(e) => eprintln!("prov: could not attach {}: {e}", p.display()),
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

fn cmd_mv(
    from: &str,
    to: &Path,
    in_target: Option<&str>,
    parents: bool,
    layout: Layout,
) -> CmdResult {
    let from_resolved = resolve_target(from)?;
    let mut ctx = find_root()?;
    // `rename` mints nothing, but `--under -p` synthesizes nodes with `create`,
    // which does — so a registry has to exist before the route runs, exactly as in
    // `new`/`reparent`. Plain `mv` skips this and stays as cheap as it was.
    if in_target.is_some() {
        let mints = ctx.config.mints_on_mutation();
        if mints {
            ensure_registry(&mut ctx)?;
        }
    }
    let mut ws = workspace(&ctx)?;
    let to_rel = ws_rel(&ctx, to)?;
    block_on(ws.rename(&ws_rel(&ctx, &from_resolved)?, &to_rel))?;
    println!("moved {} -> {}", from_resolved.display(), to.display());

    // The move first, then the reparent — in that order because `rename` has
    // already retargeted every inbound link, so the parent the reparent removes is
    // found at the document's *new* path. Doing it the other way would reparent a
    // path that is about to stop existing.
    if let Some(target) = in_target {
        let Some(parent_rel) = resolve_placement(&ctx, &mut ws, target, parents, layout, false)?
        else {
            return Ok(ExitCode::SUCCESS);
        };
        block_on(ws.reparent(&to_rel, &parent_rel))?;
        println!("reparented {} -> in {}", to.display(), parent_rel.display());
    }
    persist(&ctx, &mut ws)?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_reparent(
    path: &str,
    in_target: &str,
    parents: bool,
    layout: Layout,
    dry_run: bool,
) -> CmdResult {
    let mut ctx = find_root()?;
    // A route's synthesized nodes are `create`d and so mint on the same terms as
    // any other document — the registry must exist before the route is applied.
    // (The reparent itself authors links too, which an id-authoring workspace
    // registers.)
    let mints = ctx.config.mints_on_mutation();
    if mints && !dry_run {
        ensure_registry(&mut ctx)?;
    }

    let mut ws = workspace(&ctx)?;
    let Some(parent_rel) = resolve_placement(&ctx, &mut ws, in_target, parents, layout, dry_run)?
    else {
        return Ok(ExitCode::SUCCESS);
    };
    let path_rel = ws_rel(&ctx, &resolve_target(path)?)?;
    block_on(ws.reparent(&path_rel, &parent_rel))?;
    persist(&ctx, &mut ws)?;
    println!(
        "reparented {} -> in {}",
        path_rel.display(),
        parent_rel.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_rm(path: &str, force: bool, purge: bool) -> CmdResult {
    let resolved = resolve_target(path)?;
    let ctx = find_root()?;
    let mut ws = workspace(&ctx)?;
    let target = ws_rel(&ctx, &resolved)?;

    // The safe default — move to the recycle bin — unless the workspace opted out
    // (`recycle_bin: false`) or the caller asked for a hard delete (`--purge`).
    let danglers = if ctx.config.recycle_bin && !purge {
        let danglers = block_on(ws.recycle(&target, force, None))?;
        persist(&ctx, &mut ws)?;
        println!(
            "moved {} to the recycle bin (restore with `prov restore`)",
            resolved.display()
        );
        danglers
    } else {
        let danglers = block_on(ws.delete(&target, force))?;
        persist(&ctx, &mut ws)?;
        println!("deleted {}", resolved.display());
        danglers
    };
    for finding in &danglers {
        eprintln!("warning: now dangling — {finding}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_restore(path: &str) -> CmdResult {
    let ctx = find_root()?;
    let mut ws = workspace(&ctx)?;
    // The document is deleted, so its path cannot be `resolve_target`-ed (that
    // reads the file); take it as given, relative to the workspace root.
    let from = ws_rel(&ctx, Path::new(path))?;
    block_on(ws.restore(&from, &ctx.root_doc))?;
    persist(&ctx, &mut ws)?;
    println!("restored {}", from.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_empty_bin() -> CmdResult {
    let ctx = find_root()?;
    let mut ws = workspace(&ctx)?;
    let purged = block_on(ws.empty_bin(&ctx.root_doc))?;
    persist(&ctx, &mut ws)?;
    println!("purged {purged} document(s) from the recycle bin");
    Ok(ExitCode::SUCCESS)
}

fn cmd_convert(file: &Path, axis: &str, value: &str, recursive: bool) -> CmdResult {
    let ctx = find_root()?;
    let mut ws = workspace(&ctx)?;
    // Convert authors path links in a target [`LinkStyle`], which fuses the
    // notation (bracketed/bare) and path resolution. Each axis composes with the
    // workspace's current *other* axis; `wikilink` has no path rendering to
    // convert, so it is rejected here.
    match axis {
        "path_style" | "path-style" => {
            let ps = PathStyle::from_config_str(value).ok_or_else(|| {
                format!("unknown path_style `{value}` (expected root|relative|canonical)")
            })?;
            let style = LinkStyle::from_axes(ctx.config.notation, ps);
            let n = block_on(ws.convert_link_style(&ws_rel(&ctx, file)?, style, recursive))?;
            persist(&ctx, &mut ws)?;
            println!("converted {n} document(s) to {value} path resolution");
        }
        "notation" => {
            let nt = Notation::from_config_str(value)
                .ok_or_else(|| format!("unknown notation `{value}` (expected markdown|bare)"))?;
            if nt == Notation::Wikilink {
                return Err("convert: `wikilink` has no path rendering to convert".into());
            }
            let style = LinkStyle::from_axes(nt, ctx.config.path_style);
            let n = block_on(ws.convert_link_style(&ws_rel(&ctx, file)?, style, recursive))?;
            persist(&ctx, &mut ws)?;
            println!("converted {n} document(s) to {value} notation");
        }
        "metadata.format" | "metadata_format" | "format" => {
            let fmt = prov::metadata_format_from_str(value).ok_or_else(|| {
                format!("unknown metadata.format `{value}` (expected yaml|json|toml|fig)")
            })?;
            let n = block_on(ws.convert_meta_format(&ws_rel(&ctx, file)?, fmt, recursive))?;
            persist(&ctx, &mut ws)?;
            println!("converted {n} document(s) to {value} frontmatter");
        }
        "metadata.embed" | "metadata_embed" | "embed" => {
            let style = EmbedStyle::from_config_str(value).ok_or_else(|| {
                format!(
                    "unknown metadata.embed `{value}` \
                     (expected delimited|code_block|html_script|html_code)"
                )
            })?;
            let n = block_on(ws.convert_meta_embed(&ws_rel(&ctx, file)?, style, recursive))?;
            persist(&ctx, &mut ws)?;
            println!("converted {n} document(s) to {value} embedding");
        }
        other => {
            return Err(format!(
                "convert: axis `{other}` is not supported (only `notation`, `path_style`, \
                 `metadata.format`, and `metadata.embed`)"
            )
            .into());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_duplicate(source: &str) -> CmdResult {
    let resolved = resolve_target(source)?;
    let mut ctx = find_root()?;
    // Attaching the copy authors the parent's spanning entry, which mints an ID
    // when that style registers (or under an eager policy) — same as `new`, so
    // bootstrap a registry to persist it before building the workspace.
    let mints = ctx.config.mints_on_mutation();
    if mints {
        ensure_registry(&mut ctx)?;
    }
    let mut ws = workspace(&ctx)?;
    let copy = block_on(ws.duplicate(&ws_rel(&ctx, &resolved)?))?;
    persist(&ctx, &mut ws)?;
    println!("duplicated {} -> {}", resolved.display(), copy.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_id(file: &Path) -> CmdResult {
    let mut ctx = find_root()?;
    if !ctx.config.identity.fires_on(Trigger::Link) {
        return Err("identity is off in this workspace's config \
             (run `prov config identity lazy` to enable stable IDs)"
            .into());
    }
    ensure_registry(&mut ctx)?;
    let mut ws = workspace(&ctx)?;
    let id = block_on(ws.register(&ws_rel(&ctx, file)?, Trigger::Link))?;
    persist(&ctx, &mut ws)?;
    println!("{}", link::id_target(&id));
    Ok(ExitCode::SUCCESS)
}

/// Look up a dotted key (`references.notation`) in a nested config mapping,
/// descending one mapping per segment.
fn lookup_dotted<'a>(map: &'a Mapping, dotted: &str) -> Option<&'a Value> {
    let mut segments = dotted.split('.');
    let mut current = map.get(segments.next()?)?;
    for seg in segments {
        current = current.get(seg)?;
    }
    Some(current)
}

/// Build the nested probe a dotted `config <key> <value>` implies, so `diagnose`
/// validates `references.notation=wikilink` as the nested shape it understands
/// rather than reading `references.notation` as one unknown top-level key.
fn nest_probe(dotted: &str, value: Value) -> Value {
    let mut node = value;
    for key in dotted.rsplit('.') {
        let mut m = Mapping::new();
        m.insert(key.to_string(), node);
        node = Value::Mapping(m);
    }
    node
}

/// Materialize the full effective config explicitly into the config document:
/// every setting written at its current-or-default value, so a workspace never
/// relies on invisible defaults. Bootstraps `prov.yaml` if none is linked,
/// preserves the document's own fields (title/part_of and any user fields) and
/// every setting already present (those are already in the effective config),
/// and fills in the rest. Canonicalizes layout (comments in the config document
/// are not preserved).
fn cmd_config_setup(mut ctx: Ctx) -> CmdResult {
    let config_doc = ensure_config(&mut ctx)?;
    let full = ctx.root_dir.join(&config_doc);
    let text = std::fs::read_to_string(&full)?;
    let doc = Document::parse(&config_doc, &text)?;
    let policy = ctx.config.to_mapping();
    // Keep the document's non-policy fields (title, part_of, user fields) in
    // place, then write every effective policy key explicitly after them.
    let mut map = Mapping::new();
    if let Some(existing) = doc.meta.as_mapping() {
        for (k, v) in existing {
            if !policy.contains_key(k) {
                map.insert(k.clone(), v.clone());
            }
        }
    }
    let count = policy.len();
    for (k, v) in policy {
        map.insert(k, v);
    }
    std::fs::write(
        &full,
        meta::serialize_mapping(&map, ctx.config.default_embed_format)?,
    )?;
    println!(
        "wrote {count} explicit setting(s) to {}",
        config_doc.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Relocate the workspace's declared policy to a single home (`config --home`).
/// A *move*, not a materialization: only the *recognized policy* keys declared
/// across the two surfaces travel — no defaults baked in, and user fields stay
/// put — so the effective config is unchanged, just consolidated. Reads span both
/// homes regardless (`Ctx`); this only chooses where the bytes live.
fn cmd_config_home(mut ctx: Ctx, home: ConfigHome) -> CmdResult {
    // The recognized policy vocabulary: the keys `WorkspaceConfig` round-trips.
    // Anything else in a surface (a user field, a stray note) is not policy and
    // must not travel — so it is what the move ignores and what the delete guards.
    let recognized: std::collections::HashSet<String> =
        ctx.config.to_mapping().keys().cloned().collect();
    let declared = collect_declared_policy(&ctx, &recognized)?;
    match home {
        ConfigHome::Sidecar => move_policy_to_sidecar(&mut ctx, &declared, &recognized),
        ConfigHome::Root => move_policy_to_root(&mut ctx, &declared, &recognized),
    }
}

type KeySet = std::collections::HashSet<String>;

/// The recognized policy declared across both homes — the root's inline `prov:`
/// block with the sidecar's policy overlaid (the effective precedence *config
/// document > root block*), filtered to `recognized` so only policy travels.
/// Deep-merged, so a nested block present in both (e.g. `references`) combines
/// key-by-key rather than one home's block wholesale replacing the other's —
/// matching how `WorkspaceConfig::apply` layers.
fn collect_declared_policy(ctx: &Ctx, recognized: &KeySet) -> Result<Mapping, AnyError> {
    let mut merged = Mapping::new();
    let root_full = ctx.root_dir.join(&ctx.root_doc);
    if let Ok(text) = std::fs::read_to_string(&root_full)
        && let Ok(doc) = Document::parse(&ctx.root_doc, &text)
        && let Some(Value::Mapping(block)) = doc.meta.get(prov::config::ROOT_CONFIG_KEY)
    {
        merged = filter_keys(block, recognized);
    }
    let probe: Workspace<StdFs> = Workspace::builder(StdFs).root(&ctx.root_dir).build();
    if let Some(config_doc) = block_on(probe.config_path(&ctx.root_doc))? {
        let full = ctx.root_dir.join(&config_doc);
        let text = std::fs::read_to_string(&full)?;
        let doc = Document::parse(&config_doc, &text)?;
        if let Some(map) = doc.meta.as_mapping() {
            deep_merge(&mut merged, &filter_keys(map, recognized));
        }
    }
    Ok(merged)
}

/// A copy of `map` keeping only top-level keys in `keys`.
fn filter_keys(map: &Mapping, keys: &KeySet) -> Mapping {
    map.iter()
        .filter(|(k, _)| keys.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// A copy of `map` dropping the top-level keys in `keys`.
fn drop_keys(map: &Mapping, keys: &KeySet) -> Mapping {
    map.iter()
        .filter(|(k, _)| !keys.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Recursively overlay `overlay` onto `base`: a mapping-valued key present in both
/// merges key-by-key; every other key is replaced. The deep counterpart of
/// `Mapping::extend`, so `references: { notation }` in one home and
/// `references: { target }` in the other combine rather than clobber.
fn deep_merge(base: &mut Mapping, overlay: &Mapping) {
    for (key, value) in overlay {
        match (base.get_mut(key), value) {
            (Some(Value::Mapping(base_inner)), Value::Mapping(overlay_inner)) => {
                deep_merge(base_inner, overlay_inner);
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Rewrite the root's `prov:` block to `keep` its non-policy fields only (the
/// recognized policy having moved out): if nothing remains, remove the `prov:`
/// key entirely; otherwise set it to the remainder. The root's body and all other
/// fields are preserved.
fn strip_root_policy(ctx: &Ctx, recognized: &KeySet) -> Result<(), AnyError> {
    let root_full = ctx.root_dir.join(&ctx.root_doc);
    let text = std::fs::read_to_string(&root_full)?;
    let doc = Document::parse(&ctx.root_doc, &text)?;
    let Some(Value::Mapping(block)) = doc.meta.get(prov::config::ROOT_CONFIG_KEY) else {
        return Ok(());
    };
    let remainder = drop_keys(block, recognized);
    let updated = if remainder.is_empty() {
        edit::unset_in_text(&text, doc.carrier, prov::config::ROOT_CONFIG_KEY)?
    } else {
        edit::set_meta_in_text(
            &text,
            doc.carrier,
            prov::config::ROOT_CONFIG_KEY,
            &Value::Mapping(remainder),
        )?
    };
    std::fs::write(&root_full, updated)?;
    Ok(())
}

/// `config --home sidecar`: write the declared policy into `prov.yaml` (creating
/// and linking it if absent), preserving the sidecar's own `title`/`part_of` and
/// any non-policy fields, then strip the recognized policy from the root's `prov:`
/// block. Comments in the config document are not preserved (rebuilt canonically,
/// like `--setup`).
fn move_policy_to_sidecar(ctx: &mut Ctx, declared: &Mapping, recognized: &KeySet) -> CmdResult {
    let config_doc = ensure_config(ctx)?;
    let full = ctx.root_dir.join(&config_doc);
    let text = std::fs::read_to_string(&full)?;
    let doc = Document::parse(&config_doc, &text)?;
    // Keep every non-policy field the sidecar already has (title, part_of, and any
    // hand-added content), then write the policy after it.
    let mut map = doc
        .meta
        .as_mapping()
        .map(|m| drop_keys(m, recognized))
        .unwrap_or_default();
    for (k, v) in declared {
        map.insert(k.clone(), v.clone());
    }
    std::fs::write(
        &full,
        meta::serialize_mapping(&map, ctx.config.default_embed_format)?,
    )?;
    strip_root_policy(ctx, recognized)?;
    println!(
        "moved workspace policy to {} and cleared the root `prov:` block",
        config_doc.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// `config --home root`: merge the declared policy into the root's `prov:` block
/// (preserving any non-policy field already there), then retire the sidecar. If
/// stripping the policy leaves the sidecar with only its `title`/`part_of`, it is
/// deleted and its `config:` pointer removed; if it still carries hand-added
/// fields, it is kept (rewritten without the moved policy) so nothing is lost.
fn move_policy_to_root(ctx: &mut Ctx, declared: &Mapping, recognized: &KeySet) -> CmdResult {
    let root_full = ctx.root_dir.join(&ctx.root_doc);
    let text = std::fs::read_to_string(&root_full)?;
    let doc = Document::parse(&ctx.root_doc, &text)?;
    let mut block = match doc.meta.get(prov::config::ROOT_CONFIG_KEY) {
        Some(Value::Mapping(m)) => m.clone(),
        _ => Mapping::new(),
    };
    for (k, v) in declared {
        block.insert(k.clone(), v.clone());
    }
    let updated = edit::set_meta_in_text(
        &text,
        doc.carrier,
        prov::config::ROOT_CONFIG_KEY,
        &Value::Mapping(block),
    )?;
    std::fs::write(&root_full, updated)?;

    let probe: Workspace<StdFs> = Workspace::builder(StdFs).root(&ctx.root_dir).build();
    if let Some(config_doc) = block_on(probe.config_path(&ctx.root_doc))? {
        let sidecar_full = ctx.root_dir.join(&config_doc);
        let sidecar_text = std::fs::read_to_string(&sidecar_full)?;
        let sidecar = Document::parse(&config_doc, &sidecar_text)?;
        let remainder = sidecar
            .meta
            .as_mapping()
            .map(|m| drop_keys(m, recognized))
            .unwrap_or_default();
        let only_self_describing = remainder
            .keys()
            .all(|k| k == "title" || k == "part_of");
        if only_self_describing {
            // The sidecar is now empty of meaning — remove its pointer and delete it.
            let text = std::fs::read_to_string(&root_full)?;
            let doc = Document::parse(&ctx.root_doc, &text)?;
            if doc.meta.get("config").is_some() {
                let updated = edit::unset_in_text(&text, doc.carrier, "config")?;
                std::fs::write(&root_full, updated)?;
            }
            std::fs::remove_file(&sidecar_full)?;
            println!(
                "moved workspace policy into the root `prov:` block and removed {}",
                config_doc.display()
            );
        } else {
            // Hand-added fields remain — keep the sidecar, just without the policy.
            std::fs::write(
                &sidecar_full,
                meta::serialize_mapping(&remainder, ctx.config.default_embed_format)?,
            )?;
            let kept: Vec<String> = remainder
                .keys()
                .filter(|k| k.as_str() != "title" && k.as_str() != "part_of")
                .cloned()
                .collect();
            println!(
                "moved workspace policy into the root `prov:` block; kept {} for its non-policy field(s): {}",
                config_doc.display(),
                kept.join(", ")
            );
        }
    } else {
        println!("moved workspace policy into the root `prov:` block");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_config(
    key: Option<&str>,
    value: Option<&str>,
    setup: bool,
    home: Option<ConfigHome>,
) -> CmdResult {
    let ctx = find_root_quiet()?;
    if setup {
        return cmd_config_setup(ctx);
    }
    if let Some(home) = home {
        return cmd_config_home(ctx, home);
    }
    match (key, value) {
        // No key: print the effective config (defaults + root + config document).
        (None, _) => {
            print!(
                "{}",
                meta::serialize_mapping(&ctx.config.to_mapping(), Format::Yaml)?
            );
        }
        // Key only: read that value from the *effective* config (defaults + root
        // frontmatter + config document), so it agrees with the no-key form
        // above. Reading the config document alone would report "not set" for a
        // value that comes from root frontmatter (the diaryx-compat path) or
        // stands at its default — a divergence between the two forms.
        (Some(key), None) => {
            let effective = ctx.config.to_mapping();
            // Dotted keys address nested axes (`references.notation`).
            match lookup_dotted(&effective, key) {
                Some(v) => match v.as_str() {
                    Some(s) => println!("{s}"),
                    None => println!("{}", meta::serialize_value(v, Format::Yaml)?.trim_end()),
                },
                None => {
                    eprintln!("prov: {key} is not set");
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        // Key + value: materialize/link the config document if needed, then set.
        (Some(key), Some(value)) => {
            let mut ctx = ctx;
            let inferred = edit::infer_scalar(value);
            // Refuse to write a setting prov would silently ignore — the same
            // conditions `check` flags (a key that resembles a real axis but
            // isn't, or a recognized axis with an unrecognized value). Running the
            // shared diagnostic over a one-key probe keeps set-time and check-time
            // judgments identical. A truly novel key (resembling no axis) is left
            // to pass — it may be a user field or a forward-compatible key.
            let probe = nest_probe(key, inferred.clone().into());
            if let Some(issue) = prov::diagnose(&probe).into_iter().next() {
                match issue.kind {
                    prov::ConfigIssueKind::UnknownKey { suggestion } => {
                        eprintln!(
                            "prov: unknown config key `{key}` — did you mean `{suggestion}`?"
                        );
                    }
                    prov::ConfigIssueKind::InvalidValue { value, expected } => {
                        eprintln!(
                            "prov: `{value}` is not a valid {key} (expected: {})",
                            expected.join(", ")
                        );
                    }
                    prov::ConfigIssueKind::SpanningNotSingleParent { inverse } => {
                        eprintln!(
                            "prov: spanning relation's inverse `{inverse}` must be `cardinality: one` to form a single-parent tree"
                        );
                    }
                }
                return Ok(ExitCode::FAILURE);
            }
            let config_doc = ensure_config(&mut ctx)?;
            let full = ctx.root_dir.join(&config_doc);
            let text = std::fs::read_to_string(&full)?;
            let doc = Document::parse(&config_doc, &text)?;
            let updated = edit::set_in_text(&text, doc.carrier, key, inferred)?;
            std::fs::write(&full, updated)?;
            println!("set {key} = {value} in {}", config_doc.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Ensure the workspace *declares* a config document, bootstrapping one when it
/// does not: create `prov.<ext>` (in the workspace's metadata format) beside
/// the root (self-described with a title) and add the `config` pointer to the
/// root's metadata. Returns its path relative to the root. Mirrors
/// [`ensure_registry`], including its change set: a config document the root does
/// not point at is one nothing will ever read. Like the registry, it carries no
/// `part_of` — machinery is reached one-way through the root's pointer (DESIGN §5).
fn ensure_config(ctx: &mut Ctx) -> Result<PathBuf, AnyError> {
    let probe: Workspace<StdFs> = Workspace::builder(StdFs).root(&ctx.root_dir).build();
    if let Some(existing) = block_on(probe.config_path(&ctx.root_doc))? {
        return Ok(existing);
    }
    let format = ctx.config.default_embed_format;
    let config_rel = PathBuf::from(sidecar_name(CONFIG_STEM, format));

    let mut seed = Mapping::new();
    seed.insert("title".into(), Value::String("prov config".into()));

    let created =
        block_on(probe.link_sidecar(&ctx.root_doc, "config", &config_rel, &seed, format))?;
    if created {
        eprintln!(
            "initialized {} (linked from {})",
            config_rel.display(),
            ctx.root_doc.display()
        );
    }
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
            eprintln!("prov: {id} is tombstoned — its document was deleted");
            Ok(ExitCode::FAILURE)
        }
        None => {
            eprintln!("prov: {id} is not in this workspace's registry");
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::secs_to_rfc3339;

    #[test]
    fn rfc3339_matches_known_instants() {
        // Cross-checked against `date -u -r <secs>` / any RFC 3339 reference.
        assert_eq!(secs_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(secs_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        // A leap day, to exercise the calendar arithmetic.
        assert_eq!(secs_to_rfc3339(1_582_934_400), "2020-02-29T00:00:00Z");
        // End-of-year boundary.
        assert_eq!(secs_to_rfc3339(1_609_459_199), "2020-12-31T23:59:59Z");
    }
}
