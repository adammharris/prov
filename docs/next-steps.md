# Next steps — working notes

Deferred items from the identity / wikilink / link-syntax work, so we don't lose
them. Not curated design (that's `DESIGN.md`); this is a scratch backlog.

## Identity & backlinks

- **Step 4 — gated malformed-id autofix.** The one document-repairing heal: when
  the census finds a malformed `colophon:<id>` near an edge the registry resolves
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

- **Authoring `[[colophon:id]]` wikilinks.** The write side of the original
  idea #2: mint via `Trigger::Link`, drop the target into body prose. Closes the
  loop — the whole census/rename/backlink stack was built to support this.

## Autofix (DESIGN §8 — the sleeper feature)

Principle established: **autofix edits metadata only, never body prose** — a
`[[…]]` that is really code (`[[inf] * n for _ in range(m)]]`) must never be
"repaired", and structure-aware body editing is a later layer. So body-link
findings are diagnosis-only; frontmatter findings are fixable.

- ✅ **Missing inverse** — `suggest_fix` / `apply_fix` + interactive
  `colophon check --fix`. Adds the back-link, style-matched (absolute vs
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

The library colophon was waiting on to parse file bodies now exists:
[`twig`](https://github.com/adammharris/twig), a sister Zig-backed project
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
  (`twig/bindings/rust/twig/src/lib.rs`). colophon's `content::render_html`/
  `code_spans` are direct calls into that — no subprocess. `colophon render
  <file>` (colophon-cli, same feature) exercises rendering end-to-end.
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
  not done: needs a `colophon-cli` rebuilt with `--features content` to
  actually take effect — not a default feature yet, since it pulls in the
  path-dependent `twig` (no released version to depend on by default).
  Whether it should become default once `twig` is published is open.
- **`twig`'s AST query surface is still bigger than what's exposed at the C
  boundary.** Its Zig-level `ast/select.zig` (CSS-lite selectors) and
  `ast/reader.zig` (`pathOf`, arbitrary node/kind access) have everything
  needed to resolve `link`/`url` nodes directly, not just the fixed
  code-kind set `twig_document_code_spans` hardcodes. A generic "query by
  kind" or selector-based C ABI export would let colophon resolve real
  markdown/djot links from body prose instead of re-deriving them from
  `[[…]]` text — a bigger lift than this pass, left for when that's needed.

## Workspace config (the `config` relation)

Established: **workspace config is a reachable, self-describing document linked
from the root via a well-known `config` relation** — the registry's §6
reachability move, applied to policy. Lazily materialized (`colophon config <k>
<v>` creates + links `colophon.yaml` on first write); absent config = all
defaults. `link_format` precedence: config doc > root frontmatter (diaryx compat)
> default.

- ✅ `config` relation + `config_path`/`config_get`; CLI `config` get/set/print
  with `ensure_config` bootstrap; autofix + `find_root` read from it.
- ✅ **Typed `WorkspaceConfig`** (`config.rs`): `link_format`, `identity`,
  `id_links`, `embed_format`, with `paths_only()`/`stable_ids()` presets and
  `apply`/`from_meta`/`to_mapping` round-trip. The CLI builds the whole
  workspace from it, so **Diaryx and Obsidian are each just a config** —
  verified: `colophon id` refuses under Diaryx / mints under Obsidian;
  `colophon new` authors id links under Obsidian and a move leaves them
  untouched (registry does the maintenance). `colophon config` prints all knobs.
- ✅ **id-link authoring** (`Workspace::authored_target`): `create` and autofix
  author `colophon:<id>` (registering the target) when `id_links` is on and
  identity registers on a link, else a path in the link style. `create` mints
  IDs → `cmd_new` bootstraps the registry first when it will mint.
- ✅ **`default_embed_format`** wired into `create` (new-doc archetype default).
- **More config knobs.** `content_format` — the body-parser dependency this
  waited on now exists (see "Body parsing (`twig`)" above; `ContentFormat` is
  ready to be the knob's value type, it just isn't threaded into
  `WorkspaceConfig` yet); `vocabulary` (a named `RelationSet` preset, later a
  full spec).
- **`colophon config preset diaryx|obsidian`** — write a whole preset via
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

## Mutation

- **`delete` autofix.** `delete` now *diagnoses* inbound danglers; optionally
  offer to remove/rewrite them (careful — a link records intent).

## Link-syntax layer (this session's thread)

- ✅ **Workspace `LinkStyle`** — colophon's analogue of diaryx's `LinkFormat`
  (`markdown_root` / `markdown_relative` / `plain_relative` / `plain_canonical`),
  read from the root's `link_format` frontmatter, honored by autofix (titled,
  style-native links). `link.rs` now has `format_link` + `path_to_title`; render
  brackets only *inside* `[label](…)`, matching diaryx.
- **Route create/rename through `LinkStyle` too.** They still emit bare relative
  paths directly; they should use `format_link(self.link_style(), …)` so *all*
  authoring is style-consistent (and `mv` becomes style-faithful — the earlier
  round-trip-faithfulness item folds into this).
- **Own the link-syntax layer in colophon (don't publish a 3rd crate).** Having
  now read diaryx's `link_parser` (~730 lines, well-tested: parse/canonicalize/
  format-in-4-styles/convert/relative/title), the clean end-state per DESIGN §9
  is colophon *owning* this and diaryx depending on colophon — not a speculative
  shared crate. Port the remaining pieces colophon still lacks: balanced-paren
  path parsing (`find_closing_paren`), `convert_link(s)` for a workspace-wide
  style migration (`colophon relink --to <style>`), and the `PathType`/ambiguous
  legacy handling. Then diaryx can drop its copy.
