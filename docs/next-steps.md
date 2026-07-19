```fig
part_of = [prov](/README.md)
```
# Next steps — working notes

Deferred items from the identity / wikilink / link-syntax work, so we don't lose
them. Not curated design (that's `DESIGN.md`); this is a scratch backlog.

## Identity & backlinks

- **Step 4 — gated malformed-id autofix.** The one document-repairing heal: when
  the census finds a malformed `prov:<id>` near an edge the registry resolves
  uniquely, offer to restore it. Directional invariant: forward links are ground
  truth; the index heals *toward* them; it rewrites a document *only* for a
  dangling id it can resolve from its own record. Everything else: report.

- **Route C — persist the backlink map.** Where "id-backlink registration"
  finally has a home. Store the census-derived backlink map in the index's
  *derived* section (structurally separate from the authoritative `id → path`
  registry, per DESIGN §5), plus a `Reconciled` report (backlink added/dropped,
  out-of-band id-link registered). The census is its self-heal.

- **Frontmatter id-shadow (DESIGN §5 escape hatch).** Stamp a forward id-link's
  id into the *source's* frontmatter so the forward-link truth is complete in the
  nodes: backlinks become fully derivable, the §4 out-of-band hazard becomes
  recoverable, and self-healing goes total — no central authoritative residue.
  The thesis-aligned alternative to Route C.

- **Authoring `[[prov:id]]` wikilinks.** The write side of the original
  idea #2: mint via `Trigger::Link`, drop the target into body prose. Closes the
  loop — the whole census/rename/backlink stack was built to support this.

## Autofix (DESIGN §8 — the sleeper feature)

Principle established: **autofix edits metadata only, never body prose** — a
`[[…]]` that is really code (`[[inf] * n for _ in range(m)]]`) must never be
"repaired", and structure-aware body editing is a later layer. So body-link
findings are diagnosis-only; frontmatter findings are fixable.

- ✅ **Missing inverse** — `suggest_fix` / `apply_fix` + interactive
  `prov check --fix`. Adds the back-link, style-matched (absolute vs
  relative) to how the parent referenced the child; declines when the child
  already claims a different parent (contested).
- **Contested containment** (`… already contained elsewhere`, or a MissingInverse
  whose child claims another parent). The interesting interactive case: present
  the conflict and let the human pick — (a) make this the real parent [set the
  child's `part_of` here + drop the other's spanning entry], (b) demote this
  container's link from spanning → an overlay relation, (c) remove it. Needs a
  richer `Fix` (RemoveEntry / RetargetEntry) and a multi-choice prompt.
- **Broken frontmatter link** — offer removal, or a fuzzy relink when a
  similarly-named file exists. (Body broken links stay diagnosis-only.)
- **Non-interactive `--fix`** (apply all safe) for scripting once the safe set is
  trusted; today `--fix` is interactive (EOF → skip).

## Body parsing (`twig`)

The library prov was waiting on to parse file bodies now exists:
[`twig`](https://github.com/diaryx-org/twig), a sister Zig-backed project
(document formats, the way `fig` is for config formats). Wired in as a path
dependency for now (`../twig/bindings/rust/twig` from the workspace root) —
switch to a published version once `twig`'s Rust bindings have proven out.

- ✅ **`content.rs` + `ContentFormat`.** `ContentFormat::from_extension`
  (`.md`/`.markdown` → Markdown, `.dj`/`.djot` → Djot) needs no feature; it's
  the type the deferred `content_format` config knob (below) will store.
- ✅ **`content` feature — real FFI, both `render_html` and `code_spans`.**
  `twig`'s C ABI gained `twig_document_code_spans` alongside
  `twig_document_render_html` (a `TwigSpan{start,end}` array, one entry per
  `verbatim`/`code_block`/`raw_inline`/`raw_block` AST node —
  `twig/src/c_abi.zig`, header at `twig/bindings/c/include/twig.h`), and its
  Rust bindings a matching `Document::code_spans() -> Vec<Range<usize>>`
  (`twig/bindings/rust/twig/src/lib.rs`). prov's `content::render_html`/
  `code_spans` are direct calls into that — no subprocess. `prov render
  <file>` (prov-cli, same feature) exercises rendering end-to-end.
- ✅ **Wired into `census`/`check`/rename — and it had to be more than a
  post-filter.** `link::scan_wikilinks(path, body)` is the one entry point
  `validate.rs`'s `walk` and `mutate.rs`'s two rename-time body-rewrite
  helpers call (never `parse_wikilinks` directly). A real vault turned up why
  a simple "filter matches that overlap a code span" post-filter
  (`exclude_code_spans`, kept as a narrower utility with its caveat spelled
  out) can't do this alone: `parse_wikilinks`'s greedy "next `]]` wins, code
  or not" scan lets one stray `[[` inside a fenced Python block
  (`[[float('inf')] * width for _ in range(m + 1)]`) eat every `]]` *after*
  it in the document — including a real `[[gone.md]]` further down — merging
  them into one bogus match that swallows the real link whole before any
  span-overlap filter ever sees it separately. `scan_wikilinks` fixes this at
  the source: it treats each code span as opaque *before* scanning, running
  `parse_wikilinks` independently on each prose run between (and around) code
  spans and stitching the results back into `body`-relative spans, so a
  code-block bracket can never be in the same scan as prose that follows it.
  `validate::tests::check_does_not_flag_python_list_comprehensions_in_a_code_
  block_as_broken_links` reproduces the real report life-sized. No config
  knob was added — it's automatic whenever `content` is compiled in and the
  extension is recognized, degrading silently to the old unfiltered scan
  otherwise (feature off, unrecognized extension, or a twig failure). Still
  not done: needs a `prov-cli` rebuilt with `--features content` to
  actually take effect — not a default feature yet, since it pulls in the
  path-dependent `twig` (no released version to depend on by default).
  Whether it should become default once `twig` is published is open.
- ✅ **`twig`'s generic query surface is now exposed at the C boundary** — the
  hoped-for selector export landed. `twig_document_query` (Rust:
  `Document::query(selector)`, a CSS-lite selector reaching *every* node kind,
  returning `QueryMatch { span, kind }`) replaced the code-kind-specific
  accessor `code_spans` used to bind; `code_spans` now selects the code kinds
  itself over the generic API. Crucially for link ownership, twig also exposes a
  flat-node array (`Editor::nodes() -> [FlatNode]`) whose `destination:
  Option<String>` carries each `link`/`image` node's target. ✅ **Consumed:**
  `content::link_spans` queries `link` nodes for their spans, and
  `link::scan_body_links` slices each span and parses it with `Link::parse` (the
  span is authoritative, so no `destination` lookup is needed and no
  balanced-paren scan can over-reach). This is what made link-syntax **Stage 2**
  land (see below). Still unused from this surface: `image` nodes and the
  `destination`/reference-link path — a follow-up when non-inline links matter.

## Workspace config (the `config` relation)

Established: **workspace config is a reachable, self-describing document linked
from the root via a well-known `config` relation** — the registry's §6
reachability move, applied to policy. Lazily materialized (`prov config <k>
<v>` creates + links `prov.yaml` on first write); absent config = all
defaults. `link_format` precedence: config doc > root frontmatter (diaryx compat)
> default.

- ✅ `config` relation + `config_path`/`config_get`; CLI `config` get/set/print
  with `ensure_config` bootstrap; autofix + `find_root` read from it.
- ✅ **Typed `WorkspaceConfig`** (`config.rs`): `link_format`, `identity`,
  `id_links`, `embed_format`, with `paths_only()`/`stable_ids()` presets and
  `apply`/`from_meta`/`to_mapping` round-trip. The CLI builds the whole
  workspace from it, so **Diaryx and Obsidian are each just a config** —
  verified: `prov id` refuses under Diaryx / mints under Obsidian;
  `prov new` authors id links under Obsidian and a move leaves them
  untouched (registry does the maintenance). `prov config` prints all knobs.
- ✅ **id-link authoring** (`Workspace::authored_target`): `create` and autofix
  author `prov:<id>` (registering the target) when `id_links` is on and
  identity registers on a link, else a path in the link style. `create` mints
  IDs → `cmd_new` bootstraps the registry first when it will mint.
- ✅ **`default_embed_format`** wired into `create` (new-doc archetype default).
- ✅ **`content_format`** — the body-prose grammar, a full `WorkspaceConfig` field
  (`markdown`/`djot`/`html`), persisted by `init` (from `--content`) and read back
  like every other knob. `ContentFormat::extension()` gives the canonical file
  extension, so **title-primary `prov new "A Title"`** derives a readable
  filename (`link::slug(title).<content-ext>`) beside the parent while recording
  the real title in metadata; `--as <path>` / `--ext <e>` override the derived
  name (DESIGN §1 legibility — a slug, never an opaque `note-3.md`). The
  title-primary library seam is `Workspace::create_with_title`.
- **More config knobs.** `vocabulary` (a named `RelationSet` preset, later a full
  spec).
- **`prov config preset diaryx|obsidian`** — write a whole preset via
  `WorkspaceConfig::to_mapping` (the round-trip is already there).
- **Route `rename`'s path rewrites through the link style too.** `create` and
  autofix now author via the style/id seam; rename's inbound path rewrites still
  emit relative. Fold them through `format_link` for full consistency.
- **Builder threading smell.** Each new knob (`link_style`, `id_links`,
  `default_embed_format`) is hand-threaded through `identity()`/`index()`.
  Consider a shared inner `settings` struct the type-flipping methods carry whole.
- **Custom registration combos.** `identity` serializes as `off`/`lazy`/`eager`;
  a non-preset trigger set falls back to `lazy` on write. Represent as a
  sub-mapping if custom combos ever matter.
- **Config doc's own `part_of` style.** On first creation it's written in the
  link style active *before* the setting applies (default markdown-root), which
  can differ from the value just set. Cosmetic; rewrite it in the final style.
- **Generalize "workspace resource via well-known relation."** Registry + config
  are the same shape (reachable, self-describing, lazily materialized). Codify a
  small reserved-relation spine; a derived-index cache (Route C) is the next
  instance. Also: refactor `ensure_registry` to share this bootstrap shape.

## Config conversion (per-file `convert`)

Established: setting a config axis governs *new* documents; a parallel **`convert`
mutation** reconciles *existing* ones — the workspace can "restate itself" in a
different style/format/grammar while structure is preserved (§6 reachability + §7
format-agnosticism, made an action). Decided this session:

- **Per-file by default (DESIGN §8), not workspace-wide.** `convert <file> <axis>
  <value>` restyles only what *that* document declares; `-r` extends to its
  spanning subtree (so `convert <root> … -r` is the whole-workspace case). No
  `--all`. `-f`/force is reserved for the lossy/destructive directions. A mixed
  style across the tree is valid and `check`-clean.
- **One command surface over ~4 distinct engines** (not one uniform transform):
  1. ✅ **Reference re-authoring** (`link_format`, and later `reference_wrapper/
     target/label`, `relation_styles`) — re-spell links, frontmatter *and* body,
     destination/label/wrapper preserved, id/external/alias skipped.
     `Workspace::convert_link_style` + `restyle_frontmatter_links`/
     `restyle_body_links`; CLI `convert <file> link_format <style> [-r]`. Only the
     `link_format` axis so far; the other reference axes are the natural next add.
  2. **Metadata language** (`embed_format` yaml↔fig↔json, `embed_style`) — reserialize
     frontmatter via `meta::serialize_mapping`; `embed_style: separate` already *is*
     `separate`/`combine`. Comment loss across formats is the caveat.
  3. **Content transcode** (`content_format` md↔djot) — twig `Document::serialize`
     transcodes (proven: md→djot), *plus* a `.md→.dj` rename whose inbound-link
     cascade is `rename`'s existing job. The heavy, lossy one — gate behind `-f`.
  4. **Identity migration** (`id_storage`, `identity`) — stamp/strip ids, build/drop
     the registry; some directions destructive (identity→off breaks id links).
- **Un-abstract until the 2nd engine (DESIGN §10 discipline).** `convert_link_style`
  is a concrete method, not a `Migration` trait. Extract the shared plan-then-apply
  seam (reuse the `StructurePlan` preview pattern) only when engine 2 lands to
  justify it. `restyle_frontmatter_links` is a near-sibling of `rerelativize` (move
  vs restyle); a shared `map_frontmatter_links(…, render)` could unify them then.

## Routes (`route.rs`)

Landed: `prov new --in-title Daily/2026/2026-07 -p`. The position taken, so it
doesn't get relitigated: **the workflow is not prov's to own.** A `daily`
command would bake diaryx vocabulary into the core (§2/§9), and a workflow DSL in
`prov.yaml` would be worse — it would restate, in config, a fact the links
*already declare* (where daily entries live), which is the authoritative-vs-derived
confusion §5 warns about, while the genuinely non-derivable half (a date format)
is a fact about the *user*, not the workspace, and so can't live in a document
that's versioned and shared with the content. The split: prov supplies the
primitive a shell can't express (find-or-create nodes, linked both ways, registry
maintained); a two-line alias supplies the dates.

- **`--layout`'s default is `nested`, and that's a judgment call.** Flat is
  consistent with `create`'s beside-the-parent rule, but at depth it piles every
  generation into one directory and two routes sharing a segment name
  (`Daily/2026`, `Projects/2026`) collide on one filename. `-p` exists for deep
  routes, so nested wins. Note the *terminal* document is unaffected either way —
  it always lands beside its resolved parent — so this never contradicts `create`.
- **Route addressing is `new`-only so far.** `mv`, `attach`, `duplicate`, and
  `adopt` all name a parent by path and would take `--in-title` the same way. Worth
  doing once the segment/route surface has proven out; `route_segments` +
  `plan_route` are already the whole seam.
- **The synthesis seam is still un-extracted (deliberate, but the debt is now
  real).** `route.rs` reuses `intake`'s `SynthNode` and both end in the same
  `create_titled` loop, so this *is* the second consumer the "un-abstract until
  the 2nd engine" rule was waiting for (§10 discipline). It was left concrete
  because the two differ in the ways that matter — a plan of one chain vs. a
  forest, abort-on-failure vs. collect-and-continue — and a premature
  `Plan`/`Apply` trait would have to paper over both. Revisit when a third
  synthesizer appears, or when `--in-title` spreads to the other mutations.
- **Title matching is exact and case-sensitive.** `Daily/2026` won't find a node
  titled `daily`. Deliberate (addressing that guesses is worse than addressing
  that misses), but a `--fuzzy`/case-insensitive fallback that *reports* what it
  matched is a plausible ergonomic follow-up.
- **`title_text` coerces non-string scalars.** A hand-written `title: 2026` is a
  YAML integer, so route matching compares scalar *text*, not just
  `Value::as_str` — otherwise a route would synthesize a second `2026` beside a
  perfectly good one. If title-matching spreads (`title.rs`'s index does the same
  job for aliases), this coercion should probably move there and be shared.
- **An unlinked file in the way is an honest error, not a silent adopt.** `-p`
  onto a route whose file already exists on disk but isn't linked now refuses
  during the plan (`assert_vacant`) rather than mid-write. The old note here said
  "the fix is `adopt`, and the error doesn't say so" — that was wrong twice over:
  `adopt` is a library call and an `init` flag, **never a subcommand**, so naming it
  would prescribe a cure the CLI cannot dispense. The message now states the problem
  and offers only the remedy that exists (route to the title). Re-add the adopt
  clause if and when `prov adopt` is real — see below.
- **`assert_vacant` refuses; it deliberately does not reuse.** A route segment that
  lands on a directory already holding a node stops with that node's title. The
  tempting next step — resolve the segment *to* that node — is the one thing this
  must not do: the segment only "matched" because its slug equalled a directory
  name, which would make file layout load-bearing for graph addressing (§3) and
  leave routes meaning something other than what they spell. The cost is typing the
  real title once, and the error prints it.
- **The refusal is not "one index per directory."** That's diaryx's rule, and
  re-importing it as a lint would be directory-thinking: containment is link-shaped,
  so a directory may hold as many nodes as it likes and prov has no opinion.
  `assert_vacant` fires only where synthesis is *forced to pick a filename by
  slugging a title*, which is the one place the directory genuinely constrains the
  graph. Nowhere else should grow a version of this check.

## Reparenting (`reparent`, `mv --in`)

Landed. The axis it establishes, which is the part worth keeping straight:

| verb | path | place in tree |
| --- | --- | --- |
| `mv A B` | **changes** | preserved |
| `reparent A --in P` | preserved | **changes** |
| `mv A B --in P` | changes | changes |

The orthogonality *is* the design. Containment is link-shaped (§3), so a node may
live in any directory and relocating the file is a separate decision — which is why
`reparent` needs no `Layout` flag (moving is `mv`'s job) and why `mv --in` is pure
convenience rather than a third concept. `mv` runs first inside it, because `rename`
retargets inbound links and the reparent must then find the parent at the document's
*new* path.

`Workspace::reparent` is the verb `adopt` deliberately refuses to be: adopt is
additive and declines a child that already claims a different parent ("a contested
containment a human must resolve"); reparent *replaces* the claim. An unparented
child is accepted, which makes reparent a superset — so `prov adopt` was never
added, since `reparent --in` already links an orphan.

- **It is atomic against errors, detectable against crashes.** Three documents
  change, and they land as one `ChangeSet` (`prov/src/change.rs`), so an I/O
  failure at any of them unwinds the rest: no error leaves the child contained
  twice. A crash is still a crash — unwinding is driven from memory — so the write
  *order* still earns its keep, chosen so every window a crash could expose is a
  finding `check` reports: repoint the child (→ `MissingInverse` if it stops there),
  add the new entry (→ `DuplicateContainment`), remove the old one last. Removing
  first would leave a child pointing at a parent that forgot it — the one state in
  this set `check` does *not* look for, which is exactly why it is last. Closing
  the crash window needs a journal and an `fsync` seam on `Storage`; nothing else
  will.
- **The cycle check is a walk, not a census.** Reparenting a node under its own
  descendant is refused by walking `part_of` up from the new parent. Cheap and
  bounded, but note *why* it must be refused rather than reported: the detached pair
  still claims itself in both directions, so nothing looks broken from inside the
  loop — it simply becomes unreachable, and per the orphan gap below, unreachable is
  precisely what `check` cannot see.
- **`reparent` moves one node, not a selection.** No `-r`, no globs; a subtree moves
  by moving its root, which is the whole point of a spanning tree. Bulk reparenting
  (every `2026-07-*` under a new month) is a shell loop today and probably should
  stay one.
- **A path passed to `--in-title` is caught, but only because a file is decisive.**
  `--in-path` takes a path and `--in-title` takes a route, and the two are spelled
  identically — so in a workspace whose index titles mirror their directory names
  (`2026` in `2026/`), a path handed to `--in-title` resolves segment after segment as
  a route and dies only on the filename, reading as "the route nearly worked". The
  check is narrow on purpose: *the string names an existing **file***. Routes name
  nodes by title, so a route that is also a filename means the wrong flag; and a
  directory (`Daily/2026/07`) must not trip it, since that is a perfectly good route
  and the exact workflow the feature exists for. Nothing softer would be safe — the
  slug of `index.md` is `indexmd`, so `-p` would have cheerfully created
  `daily/2026/07/indexmd/index.md` titled "index.md".

## The CLI target grammar

Landed, replacing `--in-path`/`--in-title` (which had themselves replaced
`--in`/`--under`/`--parent` days earlier — the churn is itself the evidence). A
document argument now declares its addressing mode in the **value**:

| spelling | mode | needs a workspace |
| --- | --- | --- |
| `daily.md` | path | no |
| `@Daily/2026/08` | route of titles from the root (bare `@` = the root) | yes |
| `id:fpk38j` (or legacy `prov:`) | registry handle | yes |

This is not a new idea — it is the library's own. `Addressing::{Path, Id, Alias}`
and `Link::parse` have always disambiguated a target by its syntax; the CLI had
been reinventing the same distinction as flag names, one layer up, incompatibly.

Why flags could never have worked, stated once so it isn't retried:

- **Cost is N modes × M slots.** Two flags covered two of three modes on one
  argument. Adding ids meant `--in-id`, and `Addressing` is an open enum.
- **It could only ever reach the parent.** A *subject* positional has no flag to
  spare, so every subject was path-only — `reparent <PATH>`, `rm <PATH>`,
  `show <FILE>`, ~17 arguments in all. Backwards: the subject is the thing you
  know by meaning ("the July 14 entry"), the destination is the thing you'd more
  plausibly know by path.
- **`--in-title` mislabelled itself.** In the library, title-addressing is
  `Addressing::Alias` — *one* name, resolved globally through `TitleIndex`, with
  §8's scan hazard. A route is a *bounded walk through many titles*: different
  mechanism, different hazard, and plural. The flag name papered over a real
  distinction that `@` leaves visible.

Design notes worth keeping:

- **Root discovery is lazy, and that is load-bearing.** `show`/`links`/`meta`/
  `get`/`body`/`render`/`set`/`unset` were workspace-*free*: they read a file, from
  anywhere, workspace or not. Resolving every argument through a workspace would
  have quietly destroyed that. A path resolves with no root; only `@`/`id:` discover
  one, because only they make the argument mean a *node* rather than a file.
- **`-p` is refused on a non-route `--in`** rather than ignored. A path or id names
  something that must already exist; `-p` beside one is a mistake, not a no-op.
- **Subjects never synthesize.** Only a `--in` destination may be created, and only
  with `-p`. A subject that does not resolve is a mistake, never an instruction.
- **`ArgGroup` disappeared entirely.** The mutually-exclusive placement group only
  existed to stop two flags naming the same thing. A grammar makes that
  unrepresentable — good evidence the shape is right rather than merely different.
- **`@` needs an escape and has one:** a file literally called `@foo.md` is
  `./@foo.md`, since only a *leading* `@` is stripped. Pinned by test.

Still flag-shaped and unfinished: `attach`'s `<PAYLOAD>` stays a path on purpose (a
binary has no title and no id), and `mv`'s `<TO>` stays a path (it names a location
that does not exist yet — there is nothing to address). `new`'s positional is a
title for the *new* document, not a reference. Those three are correct as paths; the
rest of the surface now speaks one grammar.

## CLI test coverage is one file old

`prov-cli/tests/targets.rs` is the **first** test over the binary; before it,
every CLI behaviour — flag vocabulary, output, exit codes, the interview — was
untested, which is why `--in`/`--in-title` confusion shipped twice. The library is well
covered and the CLI is not, and the gap is not cosmetic: the bugs that reached a real
workspace this cycle (a route silently synthesizing a competing spine, an error
naming a nonexistent `adopt` command, a path accepted as a route) were *all* CLI-layer
and *all* invisible to library tests, because in each case the library did exactly
what it was asked. Worth extending to the other placement-taking commands and to
`init`'s adoption paths.

## Index naming is hardcoded in three places

Not configurable anywhere — no `WorkspaceConfig` field, three independent literals:
`intake::existing_node` (`"index"`/`"readme"`), the CLI's `pick_root_candidate`
(same pair), and `route::synth_path` (`format!("index.{ext}")`). Two different sins
hide here and should be separated before either is fixed:

- **Detecting *someone else's* index by filename is the bug.** `existing_node` asks
  "which file is this directory's node?" and answers by name. Its own test proves
  what that's worth: the fixture's `notes/index.md` is `---\ntitle: Notes Home\n---`
  — no `contents`, no `part_of`, structurally identical to the `notes/leaf.md`
  beside it. It wins on its name alone. But no structure means no index: the check
  isn't detection at all, it's **collision-avoidance with mirror's own synthesis
  target** wearing detection's clothes. The honest rewrite is a structural predicate
  (`route::declares_containment` is one already) plus *adopt-the-file-at-the-synth-path*
  — after which the name only matters because it's the name prov itself chose.
  Note this changes `readme` handling: a structureless `README.md` stops being a
  folder node and becomes a child. That's correct, and it's exactly why the stems
  belong in config as the *user's* declared convention rather than core's guess.
- **Naming what prov *creates* is legitimate but still shouldn't be a literal.**
  `synth_path`'s `index.{ext}` is an authoring default like `default_embed_format`,
  and belongs beside it in `WorkspaceConfig`.

## Orphan detection can't see disconnected islands

`validate::orphans` scans only "the directories the reachable set occupies (their
direct children), never descending into unreached subdirectories." So a subtree that
is internally well-linked but attached to *nothing* is invisible: in a real
workspace, `School/Archive/MATH113/` holds `math113.md` plus ~20 children all
correctly linked to each other, no `contents` entry anywhere points into the
directory, and `check` reports **zero** findings there while flagging 180 orphans
that happen to lie in already-reached directories.

This is §8 turned on itself: discovery is reachability-bounded, but *"what is
unreachable?"* is precisely the question a reachability bound cannot answer. The
current check therefore finds only orphans **adjacent** to the tree, which is a
much narrower claim than `Finding::Orphan`'s wording ("on disk but not linked into
the workspace") implies — and the gap is silent, which is the worst property for a
check to have. Any fix needs an unbounded scan, so it wants to be opt-in
(`check --unreached`?) rather than a default, and the finding's message should say
which of the two questions it answered. Deferred deliberately, not overlooked.

## Mutation

- **`delete` autofix.** `delete` now *diagnoses* inbound danglers; optionally
  offer to remove/rewrite them (careful — a link records intent).

## Link-syntax layer (this session's thread)

- ✅ **Workspace `LinkStyle`** — prov's analogue of diaryx's `LinkFormat`
  (`markdown_root` / `markdown_relative` / `plain_relative` / `plain_canonical`),
  read from the root's `link_format` frontmatter, honored by autofix (titled,
  style-native links). `link.rs` now has `format_link` + `path_to_title`; render
  brackets only *inside* `[label](…)`, matching diaryx.
- **Route create/rename through `LinkStyle` too.** They still emit bare relative
  paths directly; they should use `format_link(self.link_style(), …)` so *all*
  authoring is style-consistent (and `mv` becomes style-faithful — the earlier
  round-trip-faithfulness item folds into this).
- **Own the link-syntax layer in prov (don't publish a 3rd crate).** Having
  now read diaryx's `link_parser` (~1900 lines, well-tested: parse/canonicalize/
  format-in-4-styles/convert/relative/title), the clean end-state per DESIGN §9
  is prov *owning* this and diaryx depending on prov — not a speculative
  shared crate. **Decisions taken (this session):**
  - **Model — prov's `ReferenceStyle` is canonical; diaryx rewrites onto it.**
    prov's axes (`Wrapper` × `Addressing` × `LinkStyle`) already *subsume*
    diaryx's flat `LinkFormat`: each of its 4 variants is
    `Wrapper::Markdown × Addressing::Path × {one LinkStyle}`. diaryx maps its enum
    as a thin compat shim on its own side and deletes `link_parser.rs`. The
    id/alias/wikilink axes are prov-native, no diaryx equivalent.
  - **Bare paths — `resolve()` stays `bare = directory-relative`** (which already
    matches diaryx's legacy `Ambiguous` reading), so **no `PathType` machinery** is
    ported: the ambiguity is settled by committing to one meaning, not tagging it.
    Retire/redefine `plain_canonical`, whose current "bare = *root*-relative" claim
    is a latent bug — `path_text(PlainCanonical)` emits a root-relative bare path
    but `resolve()` reads bare as dir-relative, so those links resolve correctly
    only for documents at the workspace root.
  - **Migration wrinkle this creates.** diaryx's `plain_canonical` *means*
    bare-root-relative, which prov will no longer offer — so a diaryx workspace
    on `plain_canonical` can't just remap the enum; its links resolve differently
    under prov's resolver. `prov relink --to markdown_root` is the bridge
    (rewrites bare-root paths to `/`-prefixed), so the converter is the cutover
    tool, not merely a convenience.
  - **Scope — full port, including body `[text](path)` link resolution.** Two
    landable stages with a clean seam:
    - *Stage 1 (twig-independent):* the `plain_canonical` fix and balanced-paren
      path parsing (`find_closing_paren`) for frontmatter/longer strings still
      pending. ✅ The style *converter* landed as **per-file `convert`** (see
      "Config conversion" below), not a workspace-wide `relink` — the `link_format`
      axis is done; converting a diaryx `plain_canonical` workspace to
      `markdown_root` (the cutover bridge) is now `convert <root> link_format
      markdown_root -r`. Between these, diaryx can drop most of `link_parser.rs`.
    - ✅ *Stage 2 (body links) — done.* Real markdown/djot `[label](target)`
      links in body prose are now first-class alongside `[[wikilinks]]`.
      `content::link_spans` queries twig for `link`-node spans (code-aware:
      never a `[x](y)` inside a fence, an autolink, or non-link brackets);
      `link::scan_body_links` unifies those with the lexical wikilink scan into
      one `BodyLink { link: Link, span }` currency. Because twig hands back the
      exact span of each link, `Link::parse` reads each one in isolation — the
      **balanced-paren hazard is structurally absent** on the body side (Stage 1
      still needs `find_closing_paren` for frontmatter/longer strings). The three
      consumers (`census`/`check`, `title_scope`, the rename body-rewrite
      helpers) all moved onto `scan_body_links`, so in one pass: `check`
      diagnoses broken markdown/djot body links, backlinks include them, and
      `rename` re-relativizes them (wrapper-preserving — a markdown link stays
      markdown) while sparing id/external targets and code fences. Inline links
      only for now; reference-style/autolink and `image` nodes are a follow-up.
      Remaining Stage 1 (converter/`relink`, `find_closing_paren`,
      `plain_canonical` fix) is still what lets diaryx delete `link_parser.rs`.
