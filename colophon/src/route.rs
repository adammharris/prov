//! Routes — addressing a node by its path *through the containment tree*, and
//! materializing the segments that do not exist yet.
//!
//! A path (`Daily/2026/2026_index.md`) addresses a file; a **route**
//! (`Daily/2026/2026-07`) addresses a node by walking the spanning relation from
//! a start document, matching each segment against a child's **title**. The two
//! are deliberately separate surfaces: containment here is link-shaped, not
//! directory-shaped (DESIGN §3), so a route is a fact about the *tree* and may
//! cross directories — or not touch them at all.
//!
//! The motivating friction is the recurring-entry workflow: a daily note under
//! `Daily/2026/2026-07` needs its month node to exist, and on the first of the
//! month it does not. Naming the route and asking for missing segments to be
//! created is `mkdir -p` for the containment tree — the one part of that workflow
//! a shell alias cannot express, since it means finding-or-creating nodes and
//! linking them in both directions. Everything else (what a date *is*, how it is
//! formatted) stays in the shell, where it is the user's business and not the
//! workspace's (DESIGN §2: opinionated mechanism, flexible vocabulary).
//!
//! The work splits in two so a caller can preview before it writes, exactly as
//! [`plan_mirror`](Workspace::plan_mirror) does: [`Workspace::plan_route`] walks
//! and returns a [`RoutePlan`] without touching disk;
//! [`Workspace::apply_route`] realizes it, reusing
//! [`create`](Workspace::create) for each synthesized node. The plan is also
//! where policy lives: it *reports* what is missing rather than deciding whether
//! that is allowed, so a caller without `--parents` can refuse it and a caller
//! with `--dry-run` can print it.
//!
//! ## Resolution is bounded
//!
//! Matching a segment reads one node's spanning children and their titles — never
//! a workspace-wide scan. This looks like it should trip the alias-addressed
//! spanning hazard (DESIGN §8, where descending an alias-addressed tree needs
//! every title up front), but it does not: a route descends from a *known* node
//! one segment at a time, so only the children of the nodes actually on the route
//! are ever read. Those are index nodes, which are small.

use std::path::{Path, PathBuf};

use crate::content::ContentFormat;
use crate::document::MetaCarrier;
use crate::error::{Error, Result};
use crate::fs::Storage;
use crate::identity::IdentityPolicy;
use crate::index::IndexStore;
use crate::intake::SynthNode;
use crate::link::{self, Link};
use crate::meta::Value;
use crate::workspace::{Target, Workspace};

/// Where a route's *synthesized* nodes are written on disk.
///
/// This governs file placement only — never the graph. Containment is declared
/// by links either way, and the terminal document created *under* the route
/// always lands beside its parent (plain [`create`](Workspace::create)
/// behavior), so the two layouts differ solely in where the intermediate index
/// nodes go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// Each synthesized node is a file beside its parent — `daily.md`,
    /// `2026.md`, `2026-07.md` all in the start document's directory.
    ///
    /// Cheap for a shallow route (`--under Inbox`), but two routes that share a
    /// segment name (`Daily/2026` and `Projects/2026`) collide on one filename,
    /// and a deep route piles every generation into one directory.
    Flat,
    /// Each synthesized node gets its own directory and an `index` file inside
    /// it — `daily/index.md`, `daily/2026/index.md`,
    /// `daily/2026/2026-07/index.md`.
    ///
    /// The default: `--parents` exists for deep routes, where flat collides and
    /// piles up. Matches the folder-note convention
    /// [`plan_mirror`](Workspace::plan_mirror) already synthesizes and
    /// [`existing_node`](crate::intake) already recognizes, so a nested route and
    /// a mirrored import produce the same shape.
    #[default]
    Nested,
}

/// A plan to walk a route and create what is missing. Produced by
/// [`Workspace::plan_route`] without mutating anything, applied by
/// [`Workspace::apply_route`]; inspect it in between for a dry run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePlan {
    /// The nodes the route already resolved to, start-first — `resolved[0]` is
    /// the start document. Never empty.
    pub resolved: Vec<PathBuf>,
    /// Nodes to create, **parents-first**, so each one's parent exists by the
    /// time it is created. Empty when the whole route already exists — the check
    /// a caller makes to decide whether `--parents` was needed.
    pub synthesize: Vec<SynthNode>,
    /// The route's last node: the deepest resolved node when nothing is missing,
    /// otherwise the last synthesized one. This is the parent a caller creates
    /// under once the plan is applied.
    pub terminal: PathBuf,
}

impl RoutePlan {
    /// The whole route already exists — nothing to create, so `--parents` is
    /// not needed and [`apply_route`](Workspace::apply_route) is a no-op.
    pub fn is_complete(&self) -> bool {
        self.synthesize.is_empty()
    }
}

/// A title as *text*, whatever scalar type the metadata format guessed it into.
///
/// This exists because YAML types unquoted scalars: a hand-written year index
/// (`title: 2026`) parses as [`Value::Int`], not [`Value::String`], so
/// [`Value::as_str`] alone would fail to match the route segment `2026` — the
/// exact case routes are for. The scalar's *type* here is an accident of the
/// serialization; a user who wrote `title: 2026` means the title "2026". (Note
/// colophon's own writes quote it, so this is about meeting existing workspaces
/// where they are — not about our round-trip.)
///
/// A non-scalar (a sequence, a mapping, null) is not a title and matches nothing.
fn title_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null | Value::Sequence(_) | Value::Mapping(_) => None,
    }
}

/// The on-disk path for a synthesized node: `slug(segment)` beside `parent`
/// (flat) or `slug(segment)/index` under `parent`'s directory (nested), in the
/// content grammar `ext`.
///
/// The segment is slugged ([`link::slug`]) so the *file* stays readable
/// (DESIGN §1) while the segment text — casing, spaces, punctuation — is kept
/// verbatim as the node's title, which is what the route matches on.
fn synth_path(parent: &Path, segment: &str, layout: Layout, ext: &str) -> PathBuf {
    let dir = parent.parent().unwrap_or(Path::new(""));
    let stem = link::slug(segment);
    match layout {
        Layout::Flat => link::normalize(dir.join(format!("{stem}.{ext}"))),
        Layout::Nested => link::normalize(dir.join(stem).join(format!("index.{ext}"))),
    }
}

impl<FS: Storage, Id, Ix: IndexStore> Workspace<FS, Id, Ix> {
    /// Split a route string into its segments. `/` separates; empty segments
    /// (a leading, trailing, or doubled separator) are dropped, so `/Daily//2026/`
    /// and `Daily/2026` are the same route.
    ///
    /// Free-standing rather than a method because it is pure text: a caller that
    /// already has segments (an editor, a script) should call
    /// [`plan_route`](Self::plan_route) directly.
    pub fn route_segments(route: &str) -> Vec<&str> {
        route
            .split('/')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// The unique spanning child of `parent` whose **title** is `segment`.
    ///
    /// The child's own `title` is the truth; the parent's link label is a cached
    /// copy of it (authored by `create`, and able to drift), so this reads each
    /// candidate rather than trusting the label. Matching is exact and
    /// case-sensitive — a route is addressing, and addressing that guesses is
    /// worse than addressing that misses.
    ///
    /// Two children sharing a title is an error, not a coin-flip: it is the same
    /// unresolvable ambiguity [`NodeKind::AmbiguousAlias`](crate::tree::NodeKind)
    /// marks in a walk and [`Finding::AmbiguousAlias`](crate::validate::Finding)
    /// reports in a check.
    async fn child_titled(&self, parent: &Path, segment: &str) -> Result<Option<PathBuf>> {
        let (_, doc) = self.load(parent).await?;
        let mut matches: Vec<PathBuf> = Vec::new();
        for raw in self.relations().children(&doc.meta) {
            let link = Link::parse(&raw);
            // A target that cannot be resolved to a path cannot be title-matched:
            // an external URL has no document, and an unresolved id or ambiguous
            // alias is already broken — `check`'s business, not the route's.
            let Target::Path(path) = self.resolve_link(parent, &link) else {
                continue;
            };
            // A child that has gone missing or unreadable is likewise `check`'s
            // problem; skipping it keeps the route walk resilient the way `tree`'s
            // is, rather than dying on a broken sibling of the node we want.
            let Ok((_, child)) = self.load(&path).await else {
                continue;
            };
            if child.meta.get("title").and_then(title_text).as_deref() == Some(segment) {
                matches.push(path);
            }
        }
        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some(matches.remove(0))),
            _ => Err(Error::Structure(format!(
                "{} has {} children titled {segment:?} ({}); a route cannot say which",
                parent.display(),
                matches.len(),
                matches
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            ))),
        }
    }

    /// Plan the walk of `segments` from `start`, resolving each segment to a
    /// spanning child by title and planning a node for every segment that does
    /// not resolve. Mutates nothing.
    ///
    /// Resolution stops at the first miss: once a segment is missing, everything
    /// below it is missing too (a node that does not exist has no children), so
    /// the rest of the route is synthesized without further reads.
    ///
    /// Synthesized nodes inherit the content grammar of the *start* document, as
    /// folder-notes do in a mirror import. A **separated** or whole-file parent
    /// is refused at the point synthesis would happen — folder-note synthesis
    /// assumes a combined grammar (the node *is* the content file), and a plan
    /// that predicted `2026-07/index.md` while `create` actually wrote
    /// `2026-07/index.yaml` plus a body would be a lying preview. A route that
    /// resolves completely never hits this, so a separated workspace can still
    /// address existing nodes by route.
    pub async fn plan_route(
        &self,
        start: &Path,
        segments: &[&str],
        layout: Layout,
    ) -> Result<RoutePlan> {
        let start = link::normalize(start);
        if !self.fs().try_exists(&self.root().join(&start)).await? {
            return Err(Error::Structure(format!(
                "{} does not exist",
                start.display()
            )));
        }
        // Synthesized nodes are minted in the start document's grammar.
        let ext = start
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("md")
            .to_string();

        let mut current = start.clone();
        let mut resolved = vec![start];
        let mut synthesize: Vec<SynthNode> = Vec::new();

        for segment in segments {
            // Past the first miss nothing can resolve, so stop reading and plan.
            let found = if synthesize.is_empty() {
                self.child_titled(&current, segment).await?
            } else {
                None
            };
            match found {
                Some(child) => {
                    current = child.clone();
                    resolved.push(child);
                }
                None => {
                    // Only the first synthesized node has an on-disk parent to
                    // vet; the rest are parented by nodes this plan will create
                    // in the grammar it chose, which is combined by construction.
                    if synthesize.is_empty() {
                        self.assert_combined(&current).await?;
                    }
                    let path = synth_path(&current, segment, layout, &ext);
                    // Every synthesized node is vetted, not just the first: after
                    // a miss the route keeps deriving paths from directories that
                    // may well exist (a `Daily/2026/` full of months is exactly
                    // the case), so a later segment can land on occupied ground
                    // just as easily as the first.
                    self.assert_vacant(&path, segment, layout).await?;
                    synthesize.push(SynthNode {
                        path: path.clone(),
                        parent: current.clone(),
                        title: (*segment).to_string(),
                    });
                    current = path;
                }
            }
        }

        Ok(RoutePlan {
            resolved,
            synthesize,
            terminal: current,
        })
    }

    /// Whether `meta` declares the spanning relation in *either* direction — the
    /// structural test for "this document is already a node in some containment
    /// tree".
    ///
    /// Deliberately not a filename test. A document is a node because it declares
    /// containment, never because of what it is called (DESIGN §2): this finds a
    /// `daily_index.md` or a `2026_may.md` that no `index`/`readme` stem check
    /// would, and correctly declines a bare `index.md` that declares nothing —
    /// which is not a node, only a suggestively named file.
    fn declares_containment(&self, meta: &Value) -> bool {
        let rels = self.relations();
        let Some(spanning) = rels.spanning_relation() else {
            return false;
        };
        if meta.get(spanning).is_some() {
            return true;
        }
        rels.relations()
            .iter()
            .find(|r| r.name == spanning)
            .and_then(|r| r.inverse.as_deref())
            .is_some_and(|inverse| meta.get(inverse).is_some())
    }

    /// Refuse to synthesize a node on top of one that already exists.
    ///
    /// Two ways a route can land on occupied ground, both silent before this:
    ///
    /// 1. A document already sits at the synthesized path. `create` would fail
    ///    anyway, but only *after* the nodes above it were written — refusing
    ///    while planning keeps a `--dry-run` honest and a partial route unwritten.
    /// 2. **Nested only:** the directory the node would be created *in* already
    ///    holds a node. This is the case that matters. A route segment is slugged
    ///    to get a directory name, so `--under "Daily/…"` aims at `daily/`, which
    ///    on a case-insensitive filesystem *is* an existing `Daily/` — and a
    ///    `Daily/` holding a perfectly good `daily_index.md` titled "Daily Index"
    ///    would silently gain a second, competing index whose only distinction is
    ///    that its title matched what the user typed. The workspace ends up with
    ///    two indexes for one directory and a root linking to the wrong one.
    ///
    /// The refusal is not "one index per directory" — containment is link-shaped
    /// (DESIGN §3) and a directory may legitimately hold many nodes. It is
    /// narrower: *this* node is being placed by slugging a title, so an existing
    /// node in the target directory means the segment almost certainly meant that
    /// node, under a title the route did not spell. Refusing and naming the title
    /// found is the whole repair.
    ///
    /// Flat layout skips the directory check: it creates no directory, so its
    /// neighbours are the parent's own siblings and finding nodes there is
    /// expected, not evidence of anything.
    async fn assert_vacant(&self, path: &Path, segment: &str, layout: Layout) -> Result<()> {
        if self.fs().try_exists(&self.root().join(path)).await? {
            return Err(Error::Structure(format!(
                "route segment {segment:?} would create {}, but a file is already there",
                path.display()
            )));
        }
        if layout != Layout::Nested {
            return Ok(());
        }
        let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
            return Ok(());
        };
        let abs = self.root().join(dir);
        if !self.fs().try_exists(&abs).await? {
            return Ok(());
        }
        let Ok(entries) = self.fs().read_dir(&abs).await else {
            return Ok(());
        };
        for entry in entries {
            let Some(name) = entry.file_name() else {
                continue;
            };
            let rel = link::normalize(dir.join(name));
            if ContentFormat::from_extension(&rel).is_none() {
                continue;
            }
            // An unreadable neighbour is `check`'s problem, not the route's —
            // the same resilience `child_titled` applies to a broken sibling.
            let Ok((_, doc)) = self.load(&rel).await else {
                continue;
            };
            if !self.declares_containment(&doc.meta) {
                continue;
            }
            let titled = doc
                .meta
                .get("title")
                .and_then(title_text)
                .map(|t| format!(" (titled {t:?})"))
                .unwrap_or_default();
            // The *segment* the caller typed, never the slug derived from it: the
            // slug is an implementation detail of file placement, and echoing it
            // back ("instead of \"daily\"") misnames what the user actually wrote.
            // Only the title is offered as a remedy, because only the title is one.
            // The tempting second clause — "or adopt it if it isn't linked" — names
            // a command that does not exist: `adopt` is a library call and an `init`
            // flag, never a subcommand. An error that prescribes a cure the CLI
            // cannot dispense is worse than one that just states the problem.
            return Err(Error::Structure(format!(
                "route segment {segment:?} would create {}, but {}{} is already a node there; \
                 route to its title instead",
                path.display(),
                rel.display(),
                titled,
            )));
        }
        Ok(())
    }

    /// Refuse a parent whose grammar makes folder-note synthesis lie — a
    /// separated pair or a bare whole-file node. Mirrors the same refusal in
    /// [`plan_mirror`](Self::plan_mirror), for the same reason.
    async fn assert_combined(&self, parent: &Path) -> Result<()> {
        let (_, doc) = self.load(parent).await?;
        if doc.content_attr().is_some() || matches!(doc.carrier, Some(MetaCarrier::WholeFile(_))) {
            return Err(Error::Structure(format!(
                "cannot create a route node under {} — a separated or whole-file parent has no \
                 combined grammar for the new node to inherit; create it explicitly instead",
                parent.display()
            )));
        }
        Ok(())
    }
}

impl<FS: Storage, IdP: IdentityPolicy, Ix: IndexStore> Workspace<FS, IdP, Ix> {
    /// Apply a [`RoutePlan`]: create each missing node under its parent,
    /// parents-first, and return the route's terminal node — the parent a caller
    /// then creates the real document under.
    ///
    /// Each node is minted with [`create_titled`](Self::create_titled), so it is
    /// linked in both directions, titled after its route segment (not its
    /// `index` stem), and registered if the identity policy says so — exactly as
    /// a mirror import's folder-notes are. A complete plan writes nothing.
    ///
    /// Unlike [`apply_plan`](Self::apply_plan), a failure here aborts rather than
    /// being collected: a route is a chain, so a node that cannot be created
    /// leaves everything below it unparented. Nodes created before the failure
    /// stay — they are correctly linked, and re-running resolves them instead of
    /// recreating them.
    pub async fn apply_route(&mut self, plan: &RoutePlan) -> Result<PathBuf> {
        for synth in &plan.synthesize {
            self.create_titled(&synth.path, &synth.parent, Some(&synth.title))
                .await?;
        }
        Ok(plan.terminal.clone())
    }

    /// Resolve a route, creating any missing segments, and return its terminal
    /// node: [`plan_route`](Self::plan_route) then [`apply_route`](Self::apply_route).
    ///
    /// The unconditional-create convenience. A caller that must *refuse* to
    /// create (no `--parents`) or show what it would create (`--dry-run`) wants
    /// the two halves instead.
    pub async fn ensure_route(
        &mut self,
        start: &Path,
        segments: &[&str],
        layout: Layout,
    ) -> Result<PathBuf> {
        let plan = self.plan_route(start, segments, layout).await?;
        self.apply_route(&plan).await
    }
}

// These tests use YAML frontmatter fixtures, so they run under the `yaml` feature.
#[cfg(all(test, feature = "yaml"))]
mod tests {
    use super::*;
    use crate::exec::block_on;
    use crate::fs::StdFs;

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> String {
        std::fs::read_to_string(dir.join(rel)).unwrap()
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("colophon-route-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ws(dir: &Path) -> Workspace<StdFs> {
        Workspace::builder(StdFs).root(dir).build()
    }

    #[test]
    fn segments_ignore_empty_and_surrounding_separators() {
        assert_eq!(
            Workspace::<StdFs>::route_segments("/Daily//2026/ 2026-07 /"),
            vec!["Daily", "2026", "2026-07"]
        );
        assert!(Workspace::<StdFs>::route_segments("").is_empty());
    }

    #[test]
    fn a_fully_existing_route_resolves_and_plans_nothing() {
        let dir = tempdir("resolve");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- daily.md\n---\n",
        );
        write(
            &dir,
            "daily.md",
            "---\ntitle: Daily\npart_of: index.md\ncontents:\n- y2026.md\n---\n",
        );
        write(
            &dir,
            "y2026.md",
            "---\ntitle: '2026'\npart_of: daily.md\n---\n",
        );

        let plan = block_on(ws(&dir).plan_route(
            Path::new("index.md"),
            &["Daily", "2026"],
            Layout::Nested,
        ))
        .unwrap();
        assert!(plan.is_complete(), "nothing to create");
        // Resolution is by *title*, so the route finds `y2026.md` from "2026".
        assert_eq!(
            plan.resolved,
            vec![
                PathBuf::from("index.md"),
                PathBuf::from("daily.md"),
                PathBuf::from("y2026.md")
            ]
        );
        assert_eq!(plan.terminal, PathBuf::from("y2026.md"));
    }

    #[test]
    fn nested_layout_synthesizes_a_directory_per_segment() {
        // The motivating case: a fresh workspace, the whole route missing.
        let dir = tempdir("nested");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");

        let plan = block_on(ws(&dir).plan_route(
            Path::new("index.md"),
            &["Daily", "2026", "2026-07"],
            Layout::Nested,
        ))
        .unwrap();
        assert!(!plan.is_complete());
        let paths: Vec<_> = plan.synthesize.iter().map(|s| s.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("daily/index.md"),
                PathBuf::from("daily/2026/index.md"),
                PathBuf::from("daily/2026/2026-07/index.md"),
            ],
            "parents-first, a directory per segment"
        );
        assert_eq!(plan.terminal, PathBuf::from("daily/2026/2026-07/index.md"));

        let terminal = block_on(ws(&dir).apply_route(&plan)).unwrap();
        // The segment text is the node's title, verbatim; the slug is only the file.
        assert!(read(&dir, "daily/index.md").contains("title: Daily"));
        assert!(read(&dir, "daily/2026/2026-07/index.md").contains("title: 2026-07"));
        // Linked both ways, all the way down.
        assert!(read(&dir, "index.md").contains("[Daily](/daily/index.md)"));
        assert!(read(&dir, "daily/index.md").contains("2026/index.md"));
        assert!(read(&dir, "daily/2026/index.md").contains("2026-07/index.md"));
        // The whole synthesized chain validates.
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);

        // And the terminal is a usable parent: the day note lands inside the
        // month's directory, because `create` puts it beside its parent.
        let created = block_on(ws(&dir).create_with_title(
            Path::new("daily/2026/2026-07/2026-07-14.md"),
            &terminal,
            "2026-07-14",
        ))
        .unwrap();
        assert_eq!(
            created.node,
            PathBuf::from("daily/2026/2026-07/2026-07-14.md")
        );
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn flat_layout_keeps_synthesized_nodes_beside_the_start() {
        let dir = tempdir("flat");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");

        let plan =
            block_on(ws(&dir).plan_route(Path::new("index.md"), &["Daily", "2026"], Layout::Flat))
                .unwrap();
        let paths: Vec<_> = plan.synthesize.iter().map(|s| s.path.clone()).collect();
        assert_eq!(
            paths,
            vec![PathBuf::from("daily.md"), PathBuf::from("2026.md")]
        );

        block_on(ws(&dir).apply_route(&plan)).unwrap();
        // Quoted: a year-shaped segment stays a *string* title, so the route that
        // created it can match it back on the next run.
        assert!(
            read(&dir, "2026.md").contains("title: '2026'"),
            "{}",
            read(&dir, "2026.md")
        );
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn a_partial_route_resolves_what_exists_and_synthesizes_the_rest() {
        // The month rollover: `Daily/2026` is there from last month, `2026-08` is
        // not. The new node must land under the *existing* year node, wherever it
        // already lives — which is what makes this more than `mkdir -p`.
        let dir = tempdir("partial");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- daily/index.md\n---\n",
        );
        write(
            &dir,
            "daily/index.md",
            "---\ntitle: Daily\npart_of: ../index.md\ncontents:\n- 2026/index.md\n---\n",
        );
        write(
            &dir,
            "daily/2026/index.md",
            "---\ntitle: '2026'\npart_of: ../index.md\n---\n",
        );

        let plan = block_on(ws(&dir).plan_route(
            Path::new("index.md"),
            &["Daily", "2026", "2026-08"],
            Layout::Nested,
        ))
        .unwrap();
        assert_eq!(plan.resolved.len(), 3, "start + Daily + 2026 all existed");
        assert_eq!(plan.synthesize.len(), 1, "only the month is new");
        assert_eq!(
            plan.synthesize[0].path,
            PathBuf::from("daily/2026/2026-08/index.md")
        );
        assert_eq!(
            plan.synthesize[0].parent,
            PathBuf::from("daily/2026/index.md")
        );

        block_on(ws(&dir).apply_route(&plan)).unwrap();
        assert!(read(&dir, "daily/2026/index.md").contains("2026-08/index.md"));
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn applying_the_same_route_twice_resolves_instead_of_recreating() {
        // Idempotence is what makes the daily alias safe to re-run: the second
        // call must find the month node, not collide with it.
        let dir = tempdir("idempotent");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        let segs = ["Daily", "2026", "2026-07"];

        let first =
            block_on(ws(&dir).ensure_route(Path::new("index.md"), &segs, Layout::Nested)).unwrap();
        let second =
            block_on(ws(&dir).ensure_route(Path::new("index.md"), &segs, Layout::Nested)).unwrap();
        assert_eq!(
            first, second,
            "the second run resolves to the same terminal"
        );

        let plan =
            block_on(ws(&dir).plan_route(Path::new("index.md"), &segs, Layout::Nested)).unwrap();
        assert!(
            plan.is_complete(),
            "second time around there is nothing to create"
        );
        assert_eq!(block_on(ws(&dir).check("index.md")).unwrap(), vec![]);
    }

    #[test]
    fn an_empty_route_is_the_start_document() {
        let dir = tempdir("empty");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        let plan =
            block_on(ws(&dir).plan_route(Path::new("index.md"), &[], Layout::Nested)).unwrap();
        assert!(plan.is_complete());
        assert_eq!(plan.terminal, PathBuf::from("index.md"));
    }

    #[test]
    fn a_route_matches_titles_not_filenames() {
        // The node's title is the address; its slug is incidental. A segment that
        // matches a *filename* but no title must not resolve.
        let dir = tempdir("titles");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- journal.md\n---\n",
        );
        write(
            &dir,
            "journal.md",
            "---\ntitle: Daily\npart_of: index.md\n---\n",
        );

        let by_title =
            block_on(ws(&dir).plan_route(Path::new("index.md"), &["Daily"], Layout::Nested))
                .unwrap();
        assert!(
            by_title.is_complete(),
            "'Daily' resolves to journal.md by title"
        );
        assert_eq!(by_title.terminal, PathBuf::from("journal.md"));

        let by_stem =
            block_on(ws(&dir).plan_route(Path::new("index.md"), &["journal"], Layout::Nested))
                .unwrap();
        assert!(!by_stem.is_complete(), "the filename is not an address");
    }

    #[test]
    fn a_year_titled_without_quotes_still_matches_its_segment() {
        // The workspace colophon did *not* write: a hand-authored year index whose
        // `title: 2026` YAML typed into an integer. The segment must still match —
        // otherwise a route would synthesize a second `2026` beside the real one,
        // which is the one failure that would quietly corrupt the tree this
        // feature exists to maintain.
        let dir = tempdir("untyped");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- 2026_index.md\n---\n",
        );
        write(
            &dir,
            "2026_index.md",
            "---\ntitle: 2026\npart_of: index.md\n---\n",
        );

        let plan = block_on(ws(&dir).plan_route(Path::new("index.md"), &["2026"], Layout::Nested))
            .unwrap();
        assert!(
            plan.is_complete(),
            "an unquoted year title is still the title '2026'"
        );
        assert_eq!(plan.terminal, PathBuf::from("2026_index.md"));
    }

    #[test]
    fn two_children_with_one_title_are_an_error_not_a_guess() {
        let dir = tempdir("ambiguous");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- a.md\n- b.md\n---\n",
        );
        write(&dir, "a.md", "---\ntitle: Daily\npart_of: index.md\n---\n");
        write(&dir, "b.md", "---\ntitle: Daily\npart_of: index.md\n---\n");

        let err = block_on(ws(&dir).plan_route(Path::new("index.md"), &["Daily"], Layout::Nested))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2 children titled \"Daily\""), "{msg}");
        assert!(
            msg.contains("a.md") && msg.contains("b.md"),
            "names both: {msg}"
        );
    }

    #[test]
    fn a_broken_sibling_does_not_derail_the_route() {
        // `tree`'s resilience, applied to routes: a missing child of a node on the
        // route is `check`'s finding, not this walk's crash.
        let dir = tempdir("broken-sibling");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- gone.md\n- https://example.com\n- daily.md\n---\n",
        );
        write(
            &dir,
            "daily.md",
            "---\ntitle: Daily\npart_of: index.md\n---\n",
        );

        let plan = block_on(ws(&dir).plan_route(Path::new("index.md"), &["Daily"], Layout::Nested))
            .unwrap();
        assert!(plan.is_complete());
        assert_eq!(plan.terminal, PathBuf::from("daily.md"));
    }

    #[test]
    fn a_separated_parent_is_refused_only_when_it_must_synthesize() {
        // Grammar the plan cannot honestly predict: refuse rather than preview a
        // lie. But addressing an *existing* node by route is still fine.
        let dir = tempdir("separated");
        write(
            &dir,
            "index.yaml",
            "title: Root\ncontent: index.md\ncontents:\n- daily.yaml\n",
        );
        write(&dir, "index.md", "# Root\n");
        write(&dir, "daily.yaml", "title: Daily\npart_of: index.yaml\n");

        let ok = block_on(ws(&dir).plan_route(Path::new("index.yaml"), &["Daily"], Layout::Nested))
            .unwrap();
        assert!(
            ok.is_complete(),
            "an existing route resolves under a separated root"
        );

        let err = block_on(ws(&dir).plan_route(
            Path::new("index.yaml"),
            &["Daily", "2026"],
            Layout::Nested,
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("separated or whole-file parent"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_start_document_is_an_error() {
        let dir = tempdir("no-start");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        let err = block_on(ws(&dir).plan_route(Path::new("nope.md"), &["Daily"], Layout::Nested))
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn a_directory_already_holding_a_node_refuses_instead_of_minting_a_competitor() {
        // The bug the guard exists for, in the shape that produced it: `Daily/`
        // already holds its index, titled "Daily Index". The route segment
        // `Daily` slugs to `daily/` — the same directory on a case-insensitive
        // filesystem — so without the guard this mints `daily/index.md` beside
        // the real index and links the root to the competitor.
        let dir = tempdir("occupied");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- '[Daily Index](/daily/daily_index.md)'\n---\n",
        );
        write(
            &dir,
            "daily/daily_index.md",
            "---\ntitle: Daily Index\npart_of: '[Home](/index.md)'\ncontents:\n---\n",
        );

        let err = block_on(ws(&dir).plan_route(Path::new("index.md"), &["Daily"], Layout::Nested))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("daily/daily_index.md"),
            "names what is in the way: {msg}"
        );
        assert!(
            msg.contains("Daily Index"),
            "names the title to route to instead: {msg}"
        );
    }

    #[test]
    fn a_directory_of_plain_documents_does_not_block_a_route_node() {
        // The guard is structural, not positional. `notes/` holds an ordinary
        // document declaring no containment, so it is not "already a node" and
        // the route may still put its index there. Note the inverse of the test
        // above: a *filename* check would have been fooled here by the absence of
        // anything called `index`, and fooled there by its presence.
        let dir = tempdir("vacant");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        write(&dir, "notes/loose.md", "---\ntitle: Loose\n---\n");

        let plan = block_on(ws(&dir).plan_route(Path::new("index.md"), &["Notes"], Layout::Nested))
            .unwrap();
        assert_eq!(
            plan.synthesize
                .iter()
                .map(|s| s.path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("notes/index.md")]
        );
    }

    #[test]
    fn a_file_squatting_the_synthesized_path_is_refused_while_planning() {
        // `create` would fail on this anyway — but only after writing every node
        // above it. Refusing during the plan keeps `--dry-run` honest and leaves
        // a refused route with nothing written at all.
        let dir = tempdir("squat");
        write(&dir, "index.md", "---\ntitle: Home\n---\n");
        write(&dir, "daily/index.md", "---\ntitle: Unrelated\n---\n");

        let err = block_on(ws(&dir).plan_route(Path::new("index.md"), &["Daily"], Layout::Nested))
            .unwrap_err();
        assert!(err.to_string().contains("a file is already there"), "{err}");
    }

    #[test]
    fn flat_layout_is_not_blocked_by_the_nodes_beside_it() {
        // Flat creates no directory, so a synthesized node's neighbours are the
        // parent's own siblings — which are nodes by definition. Applying the
        // directory check here would refuse every flat route.
        let dir = tempdir("flat-vacant");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- '[Other](/other.md)'\n---\n",
        );
        write(
            &dir,
            "other.md",
            "---\ntitle: Other\npart_of: '[Home](/index.md)'\n---\n",
        );

        let plan =
            block_on(ws(&dir).plan_route(Path::new("index.md"), &["Daily"], Layout::Flat)).unwrap();
        assert_eq!(plan.synthesize[0].path, PathBuf::from("daily.md"));
    }

    #[test]
    fn a_later_segment_landing_on_an_occupied_directory_is_refused_too() {
        // The vet runs on every synthesized node, not just the first: `Daily`
        // resolves, then `2026` misses and aims at `daily/2026/` — a directory
        // that already holds a year index under a title the route did not spell.
        // This is the month-rollover shape, where the directories below a
        // resolved node are exactly the ones most likely to already exist.
        let dir = tempdir("deep-occupied");
        write(
            &dir,
            "index.md",
            "---\ntitle: Home\ncontents:\n- '[Daily](/daily/index.md)'\n---\n",
        );
        write(
            &dir,
            "daily/index.md",
            "---\ntitle: Daily\npart_of: '[Home](/index.md)'\ncontents:\n---\n",
        );
        write(
            &dir,
            "daily/2026/2026_index.md",
            "---\ntitle: '2026 Index'\ncontents:\n---\n",
        );

        let err = block_on(ws(&dir).plan_route(
            Path::new("index.md"),
            &["Daily", "2026"],
            Layout::Nested,
        ))
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("daily/2026/2026_index.md"),
            "names what is in the way: {msg}"
        );
        assert!(
            msg.contains("2026 Index"),
            "names the title to route to instead: {msg}"
        );
    }
}
