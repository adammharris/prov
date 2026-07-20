//! The `init` command and its interactive intake — the one command that *creates*
//! a workspace rather than operating on an existing one, and by far the largest.
//!
//! It classifies the target directory (greenfield, loose notes, an existing tree,
//! an already-initialized workspace), runs the guided interview (or takes flag
//! defaults), writes the root and config documents, and optionally adopts or
//! attaches what is already on disk. Split out from `main.rs` so the dispatcher
//! stays legible; everything it needs from the CLI's session layer and the `clap`
//! grammar it reaches through `use super::*` (a submodule sees its parent's items).

use super::*;

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
    /// Empty, or only files prov doesn't treat as content documents (images,
    /// code, data). `init` proceeds exactly as on a fresh directory.
    Greenfield,
    /// Content documents are present but none declares a containment link — a
    /// loose folder of notes. `init` can proceed, leaving them unlinked (a future
    /// `adopt` pulls them in); `docs` are their workspace-relative paths.
    LooseContent { docs: Vec<PathBuf> },
    /// A top-level document declares `contents` — an existing prov/diaryx tree
    /// rooted here. `init` must not mint a competing root; `root` is the detected
    /// top-level root candidate, if unambiguous.
    Structured { root: Option<PathBuf> },
    /// A prov root or config document is already present — this is an
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
/// prov root is a top-level document, so "is this already a workspace?" is a
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
                    (
                        has_part_of || doc.meta.get("contents").is_some(),
                        !has_part_of,
                    )
                })
                .unwrap_or((false, false));
            FoundDoc {
                rel,
                structural,
                root_candidate,
            }
        })
        .collect()
}

/// Classify `dir` for `init`. Whether it is already a workspace is decided by the
/// **top-level** documents (an `index.*`/`prov.*` marker, or a top-level
/// document that declares containment) — never by a recursive sweep, so a
/// vendored or nested tree deeper in the repo cannot be mistaken for the root or
/// inflate the count. Otherwise the loose content is gathered (recursively, for a
/// `mirror` import) to decide loose-vs-greenfield. The second return is every
/// loose *non-document* file (image, PDF, binary, source code) `init` can offer
/// to attach. Empty for an already-a-workspace directory (`init` aborts).
fn classify_dir(dir: &Path) -> (DirState, Vec<PathBuf>) {
    // An existing root (`index.<content|meta-ext>`) or config sidecar
    // (`prov.<meta-ext>`) at the top level means this is already a workspace.
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
    // README-rooted vault with no index/prov marker) — this is already a
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
    let loose: Vec<PathBuf> = docs
        .into_iter()
        .filter(|d| !d.structural)
        .map(|d| d.rel)
        .collect();
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
/// `others` — anything [`prov::is_opaque_payload`] treats as bytes). Hidden
/// entries (`.`-prefixed) are skipped, mirroring the library's scans; unreadable
/// or unparsable content files count as plain (non-structural) content.
fn scan_docs(dir: &Path, rel: &Path, docs: &mut Vec<FoundDoc>, others: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
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
            // A non-document file prov cannot read (image, PDF, data, code)
            // is an attachment candidate; a whole-file metadata file (`.yaml`,
            // `.json`) is neither content nor opaque, so it is simply ignored.
            if prov::is_opaque_payload(&child_rel) {
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
        docs.push(FoundDoc {
            rel: child_rel,
            structural,
            root_candidate,
        });
    }
}

/// Pick the workspace root from a set of found documents: a `readme` stem wins
/// (an `index` would have been caught as already-initialized), else a lone
/// root-candidate. Two-plus candidates are ambiguous — `None`, and `init` won't
/// guess.
fn pick_root_candidate(docs: &[FoundDoc]) -> Option<PathBuf> {
    let candidates: Vec<&PathBuf> = docs
        .iter()
        .filter(|d| d.root_candidate)
        .map(|d| &d.rel)
        .collect();
    let stem_is = |p: &Path, want: &str| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case(want))
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
        let name = d
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        sel = sel.item(d.clone(), name, "adopt this document as the root");
    }
    // The empty path is the "create a new index" sentinel.
    sel = sel.item(
        PathBuf::new(),
        "Create a new index",
        "synthesize a fresh root document",
    );
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
        .and_then(|d| {
            d.meta
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
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
            } else if prov::is_opaque_payload(&rel) {
                // A whole-file metadata file (`.yaml` config/registry) is neither
                // a document to adopt nor an opaque payload — it is skipped.
                others.push(rel);
            }
        }
    }
    docs.sort();
    others.sort();
    subdirs.sort();
    DirListing {
        docs,
        others,
        subdirs,
    }
}

/// The node document a directory already has — an `index`- or `readme`-stemmed
/// plaintext file directly in it — or `None` (a folder-note must be synthesized).
/// Directory scope of the root discovery in `find_root`/`existing_node`.
fn existing_dir_node(root_dir: &Path, rel_dir: &Path) -> Option<PathBuf> {
    let listing = dir_listing(root_dir, rel_dir);
    let stem_is = |p: &Path, want: &str| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case(want))
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
    let docs: Vec<PathBuf> = listing
        .docs
        .iter()
        .filter(|d| d.as_path() != node)
        .cloned()
        .collect();
    if !docs.is_empty() {
        let items: Vec<(PathBuf, String, String)> = docs
            .iter()
            .map(|d| {
                (
                    d.clone(),
                    d.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    String::new(),
                )
            })
            .collect();
        let picked = cliclack::multiselect(format!(
            "Documents in {here} to link under {}:",
            node.display()
        ))
        .items(&items)
        .initial_values(docs.clone())
        .required(false)
        .interact()?;
        for child in picked {
            plan.adoptions.push(Adoption {
                child,
                parent: node.to_path_buf(),
            });
        }
    }

    // 2. Non-document files → write a metadata sidecar for each chosen one.
    if !listing.others.is_empty() {
        let items: Vec<(PathBuf, String, String)> = listing
            .others
            .iter()
            .map(|f| {
                (
                    f.clone(),
                    f.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    String::new(),
                )
            })
            .collect();
        let picked =
            cliclack::multiselect(format!("Non-document files in {here} to give metadata:"))
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
            .map(|d| {
                (
                    d.clone(),
                    format!("{}/", d.file_name().unwrap_or_default().to_string_lossy()),
                    String::new(),
                )
            })
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
                    plan.adoptions.push(Adoption {
                        child: node_rel.clone(),
                        parent: node.to_path_buf(),
                    });
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
pub(crate) fn cmd_init(
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
    fixity: Option<FixityArg>,
    no_recycle_bin: bool,
    updated_field: Option<String>,
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
        // is not built yet, so refuse with the path that is — `prov config`
        // attaches policy to the existing root.
        DirState::Structured { root } => {
            let root_note = root
                .as_ref()
                .map(|r| format!(" (root: {})", r.display()))
                .unwrap_or_default();
            return Err(format!(
                "this directory already holds a prov workspace{root_note}. \
                 `init` would mint a competing root — to attach prov configuration \
                 to the existing tree, run `prov config <key> <value>` from here."
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
            let nested = docs
                .iter()
                .any(|d| d.parent().is_some_and(|p| !p.as_os_str().is_empty()));
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
                        "{n} existing document(s){others_note} here aren't part of a prov workspace — what should init do?"
                    ));
                    if nested {
                        menu = menu.item(
                            "mirror",
                            "Import the folder tree",
                            "mirror each directory as a node (synthesizing folder indexes)",
                        );
                    }
                    menu = menu
                        .item(
                            "flat",
                            "Adopt flat",
                            "link every file directly under the new root",
                        )
                        .item(
                            "leave",
                            "Leave unlinked",
                            "initialize anyway; prov check will list them",
                        )
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
                        "prov: note — {n} existing document(s) here will not be linked \
                         into the workspace (prov check will list them; `--adopt flat` links them)."
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
            .item("leave", "Leave unlinked", "invisible to prov until you attach them")
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
    let fixity_prompt_possible = fixity.is_none();
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
            || id_storage_prompt_possible
            || fixity_prompt_possible);
    if prompting {
        cliclack::intro("prov init")?;
    }

    // Root selection (the walk's first step): adopt one of the directory's own
    // top-level documents as the root, or synthesize a fresh index. Offered only
    // when loose documents are present to choose from.
    let root_pick: Option<PathBuf> = if use_walk {
        let top_docs = dir_listing(&dir, Path::new("")).docs;
        if top_docs.is_empty() {
            None
        } else {
            prompt_root_choice(&top_docs)?
        }
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
            if prov::embed_carrier(embed, m.into()).is_none() {
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
            .item(
                IdentityArg::Lazy,
                "On demand",
                "an ID is minted when a document is linked by ID or published",
            )
            .item(
                IdentityArg::Off,
                "None",
                "documents are addressed by path only",
            )
            .item(
                IdentityArg::Eager,
                "From creation",
                "every document gets an ID when it is created",
            )
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

    // The archival safety axes. Fixity is prompted (three meaningful tiers);
    // recycle-bin (on unless opted out) and the updated-timestamp field are
    // flag-only — a recoverable delete is the right default without asking, and a
    // timestamp field name is a niche text input.
    let fixity = match fixity {
        Some(f) => f,
        None if interactive => prompt_fixity()?,
        None => FixityArg::Payloads,
    };
    let recycle_bin = !no_recycle_bin;
    let updated_field = updated_field.unwrap_or_default();

    // Assemble the workspace preferences these choices encode. The wrapper +
    // path-format prompts fix the notation/path_style axes; `write_onto` then
    // layers the reference addressing and any per-relation split.
    let link_ls: LinkStyle = link_style.into();
    let mut ws_config = WorkspaceConfig {
        identity: identity.registration(),
        notation: Notation::from_wrapper(wrapper.into(), link_ls),
        path_style: link_ls.axes().1,
        reference_target: Addressing::Path,
        reference_label: false,
        relation_styles: std::collections::BTreeMap::new(),
        spanning: None,
        relation_defs: std::collections::BTreeMap::new(),
        fields: std::collections::BTreeMap::new(),
        id_storage: id_storage.into(),
        default_embed_format: meta.into(),
        embed_style: embed,
        content_format: content.into(),
        recycle_bin,
        fixity: fixity.into(),
        updated: updated_field.clone(),
    };
    reference.write_onto(&mut ws_config);

    let meta_format: Format = meta.into();
    let config_name = sidecar_name(CONFIG_STEM, meta_format);
    let content_name = format!("index.{}", content.ext());
    // The carrier the root's metadata lives in — a fenced block in the content
    // file, or (for `separate`) a whole-file sibling node. Validated already, so
    // this never fails here.
    let carrier = prov::embed_carrier(embed, meta_format).ok_or_else(|| {
        format!(
            "`{}` metadata cannot be embedded as `{}`",
            meta.label(),
            embed.as_config_str()
        )
    })?;

    // Write the root, and learn which file is the structural root document (the
    // node the `config` pointer is added to, and the `next:` hint names).
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
                let node_name = format!("index.{}", prov::document::whole_file_extension(format));
                std::fs::write(dir.join(&content_name), content.heading(&title))?;
                let mut node = Mapping::new();
                node.insert("title".into(), Value::String(title.clone()));
                if let Some(author) = &author {
                    node.insert("author".into(), Value::String(author.clone()));
                }
                node.insert("content".into(), Value::String(content_name.clone()));
                node.insert("config".into(), Value::String(config_name.clone()));
                std::fs::write(
                    dir.join(&node_name),
                    meta::serialize_mapping(&node, format)?,
                )?;
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
    // self-describing (a title) plus the recorded preferences. A whole-file config
    // document (DESIGN §6/§7), the same shape as the registry — and, like the
    // registry, it carries no `part_of`: machinery is reached one-way through the
    // root's `config` pointer, so a back-link would assert a tree membership it
    // does not have (DESIGN §5, "link target kinds").
    let config_rel = PathBuf::from(&config_name);
    let mut config_map = Mapping::new();
    config_map.insert("title".into(), Value::String("prov config".into()));
    for (key, value) in ws_config.to_mapping() {
        config_map.insert(key, value);
    }
    std::fs::write(
        dir.join(&config_rel),
        meta::serialize_mapping(&config_map, meta_format)?,
    )?;

    // An adopted existing root did not get a `config` pointer during synthesis
    // (it was not synthesized) — add it now, comment- and format-preservingly,
    // so the workspace is discoverable from its own root like any other.
    if root_pick.is_some() {
        let root_full = dir.join(&root_name);
        let text = std::fs::read_to_string(&root_full)?;
        let doc = Document::parse(Path::new(&root_name), &text)?;
        let updated = edit::set_in_text(
            &text,
            doc.carrier,
            "config",
            edit::infer_scalar(&config_name),
        )?;
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
        let mints = ctx.config.mints_on_mutation();
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
                        eprintln!("prov: could not adopt {}: {why}", doc.display());
                    }
                    adopt_note = format!(
                        "\nimported {} document(s), synthesizing {} folder index(es), mirroring the tree under {root_name}",
                        outcome.adopted.len(),
                        outcome.synthesized.len(),
                    );
                    None // handled
                }
                Err(e) => {
                    eprintln!("prov: mirror import unavailable ({e}); adopting flat instead");
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
                    Err(e) => eprintln!("prov: could not adopt {}: {e}", doc.display()),
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
                    Err(e) => eprintln!("prov: could not attach {}: {e}", payload.display()),
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
        intake_walk(
            &dir,
            Path::new(""),
            Path::new(&root_name),
            content.ext(),
            &mut plan,
            &mut attachments,
        )?;
        if !plan.is_empty() || !attachments.is_empty() {
            let mut ctx = Ctx {
                root_dir: dir.clone(),
                root_doc: PathBuf::from(&root_name),
                registry: None,
                config: ws_config.clone(),
            };
            let mints = ctx.config.mints_on_mutation();
            if mints {
                ensure_registry(&mut ctx)?;
            }
            let mut ws = workspace(&ctx)?;
            let outcome = block_on(ws.apply_plan(&plan))?;
            for (doc, why) in &outcome.skipped {
                eprintln!("prov: could not link {}: {why}", doc.display());
            }
            let mut attached = 0usize;
            for (payload, parent) in &attachments {
                match block_on(ws.attach(payload, parent)) {
                    Ok(_) => attached += 1,
                    Err(e) => eprintln!("prov: could not attach {}: {e}", payload.display()),
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

    let author_note = author
        .as_deref()
        .map(|a| format!(", author {a}"))
        .unwrap_or_default();
    let (embed_label, _) = embed_labels(embed);
    // The path format only appears when a by-path reference is authored — it is
    // inert otherwise.
    let path_note = if reference.uses_path() {
        format!(
            ", {} notation, {} paths",
            ws_config.notation.as_config_str(),
            ws_config.path_style.as_config_str()
        )
    } else {
        String::new()
    };
    // ID storage only matters once identity is on (something mints).
    let id_storage_note = if identity != IdentityArg::Off {
        format!(", id storage {}", id_storage.label())
    } else {
        String::new()
    };
    // The safety axes: recycle bin (when on), fixity (when recording anything),
    // and the updated-timestamp field (when named).
    let recycle_note = if recycle_bin { ", recycle bin" } else { "" };
    let fixity_note = if fixity != FixityArg::Off {
        format!(", fixity {}", fixity.label())
    } else {
        String::new()
    };
    let updated_note = if updated_field.is_empty() {
        String::new()
    } else {
        format!(", updates `{updated_field}`")
    };
    let details = format!(
        "root: {root_name} — {title}{author_note}\n\
         config: {config_name} — content {}, embed {} ({}), language {}, identity {}, references {}{path_note}{id_storage_note}{recycle_note}{fixity_note}{updated_note}",
        content.label(),
        embed.as_config_str(),
        embed_label.to_lowercase(),
        meta.label(),
        identity.label(),
        reference.label(),
    );
    let next = format!("next: prov new <title> --in {root_name}");
    if prompting {
        cliclack::outro(format!(
            "initialized {}\n{details}{adopt_note}\n{next}",
            dir.display()
        ))?;
    } else {
        eprintln!("initialized {}", dir.display());
        for line in details.lines() {
            eprintln!("  {line}");
        }
        for line in adopt_note.lines().filter(|l| !l.is_empty()) {
            eprintln!("  {line}");
        }
        eprintln!("{next}");
    }
    // The created root document is the workspace's handle — the path a caller opens
    // or feeds to the next `prov` command. Narration above went to stderr.
    println!("{}", dir.join(&root_name).display());
    Ok(ExitCode::SUCCESS)
}

/// Prompt for the embed type, offering only the styles that suit `content` (the
/// first is the default). See [`ContentLang::embed_styles`].
/// Prompt for the reference **wrapper** — the first style axis.
fn prompt_wrapper() -> std::io::Result<WrapperArg> {
    cliclack::select("Wrapper")
        .initial_value(WrapperArg::Markdown)
        .item(
            WrapperArg::Markdown,
            "Markdown",
            "[Title](target) — the diaryx/CommonMark form",
        )
        .item(
            WrapperArg::Wikilink,
            "Wikilink",
            "[[target]] — the Obsidian form",
        )
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
        .item(
            ReferenceArg::Path,
            "By path",
            "readable; rewritten when a file moves",
        );
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
/// Prompt for content-checksum coverage — the archival bit-rot guard. Payloads
/// is the frictionless default; full extends it to editable bodies (paired with
/// `prov edit`); off records nothing.
fn prompt_fixity() -> std::io::Result<FixityArg> {
    cliclack::select("Content checksums (bit-rot detection)")
        .initial_value(FixityArg::Payloads)
        .item(
            FixityArg::Payloads,
            "Attachments",
            "checksum attachment files; verified by `check` (recommended)",
        )
        .item(
            FixityArg::Full,
            "Attachments + document bodies",
            "also checksum bodies; restamped by `prov edit`",
        )
        .item(FixityArg::Off, "Off", "record no checksums")
        .interact()
}

fn prompt_id_storage() -> std::io::Result<IdStorageArg> {
    cliclack::select("Where IDs are stored")
        .initial_value(IdStorageArg::Frontmatter)
        .item(
            IdStorageArg::Frontmatter,
            "In each file (+ registry)",
            "each document carries its own `id`; portable, travels with the file",
        )
        .item(
            IdStorageArg::Registry,
            "Registry only",
            "IDs live in one registry document",
        )
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
            .item(
                LinkStyleArg::MarkdownRoot,
                "Workspace-absolute",
                "[Title](/path.md)",
            )
            .item(
                LinkStyleArg::MarkdownRelative,
                "Relative",
                "[Title](../path.md)",
            )
            .item(LinkStyleArg::PlainRelative, "Plain relative", "../path.md")
            .item(
                LinkStyleArg::PlainCanonical,
                "Plain workspace path",
                "path.md",
            ),
        WrapperArg::Wikilink => select
            .item(
                LinkStyleArg::MarkdownRoot,
                "Workspace-absolute",
                "[[/path.md]]",
            )
            .item(LinkStyleArg::PlainRelative, "Relative", "[[../path.md]]")
            .item(
                LinkStyleArg::PlainCanonical,
                "Workspace path",
                "[[path.md]]",
            ),
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
