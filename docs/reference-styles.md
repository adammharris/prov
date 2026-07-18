```fig
part_of = [colophon](/README.md)
```
# Reference styles

How a durable reference — a relation target in metadata, or (eventually) a body
link — is *spelled*. colophon exposes every option as configuration; a curated
frontend (diaryx) picks one. This is the identity-vs-readability dial made
explicit.

## The axes

A reference style is a **notation**, an **addressing** (`target`), an optional
**label**, and — for path targets — a **path_style** (`docs/config-vocab.md`,
`references:` block). Notation and path resolution are orthogonal: notation says
*how the reference is delimited* (bracketed markdown, wikilink, or bare), path
style says *how a path is resolved* (root / relative / canonical).

| notation | addressing | written form | durable (move/rename-safe)? | readable raw? |
|---|---|---|---|---|
| `markdown` | `path` | `[Title](/notes/a.md)` (path per `path_style`) | ❌ rewritten on move | ✅ |
| `bare` | `path` | `/notes/a.md` (path per `path_style`) | ❌ rewritten on move | ✅ |
| `markdown` | `id` | `[Title](id:ajp7eq)` | ✅ | ✅ (title shown) |
| `wikilink` | `id` (no label) | `[[id:ajp7eq]]` | ✅ | ❌ opaque id |
| `wikilink` | `id` (label) | `[[id:ajp7eq\|Title]]` | ✅ | ✅ |
| `wikilink` | `alias` | `[[Title]]` | ❌ nominal, index-resolved | ✅ |
| `wikilink` | `path` | `[[notes/a.md]]` | ❌ rewritten on move | ✅ |

- **`id`** addresses by the durable `id:<id>` handle. Authoring one *registers*
  the target (the link-by-id trigger, DESIGN §4). Survives moves untouched — the
  registry update is the maintenance.
- **`alias`** addresses by the target's title/name, resolved nominally through a
  title index. Readable, but not move/rename-safe, and it never registers. The
  weakest-but-prettiest option.
- **`path`** is the classic diaryx form; `path_style` chooses the rendering
  (`root` = `/a.md`, `relative` = `../a.md`, `canonical` = `a.md`), and `notation`
  chooses whether it is bracketed (`markdown`) or bare (`bare`).
- The **label** on an `id` wikilink (`|Title`) is a *cached copy* of the target's
  title — cosmetic, refreshable. `check` can flag a stale one (`StaleLabel`,
  staged) and refresh it, turning "fallible cache" into "maintained cache."

The `id:` scheme replaces the older `colophon:` spelling (de-branded, shorter,
still explicit and diagnosable, and — unlike a `colophon://` URL — it does not
collide with `is_external`'s `://` check). `colophon:` is still recognized on
read for backward compatibility.

`alias` implies `wikilink` (neither `markdown` nor `bare` can address by bare
name), so an `alias` request always renders as a wikilink.

## Up ≠ down: style is per-relation

Style is resolved **per relation**, with the workspace default as the fallback.
"Different links going down (`contents`) vs up (`part_of`)" is just two relations
each carrying their own style — no new concept, because a link is stored as *two
independent fields in two files* (`A.contents → B` and `B.part_of → A`), and each
side is authored in its own relation's style. On read the resolver is
style-agnostic: it takes whatever it finds and resolves it.

```yaml
# workspace default (root `colophon:` block / config document)
references:
  notation: wikilink
  target: id
  label: true

relations:
  contents: { notation: wikilink, target: alias }   # DOWN: reads like a TOC
  part_of:  { notation: markdown, target: id }       # UP: durable bookkeeping
```

### Consequences (chosen deliberately, not stumbled into)

1. **Registration follows the `id` direction.** Whichever direction is `id`-style
   is the link-by-id that registers. With `part_of: id` + `contents: alias`, every
   non-root node registers *its parent* → internal nodes get IDs, pure leaves may
   not.
2. **Bidirectional reconcile tie-break.** When a stored inverse pair disagrees,
   the `id` side is durable and the `alias`/`path` side is fallible — trust the
   `id` side.
3. **Don't make both spanning directions `alias`.** Then nothing is durable,
   nothing registers, and a title rename dangles the structure both ways. `check`
   should warn.

## Alias resolution

A nominal (`alias`) reference is resolved through a **title index** — a derived
`name → document` map (`title.rs`) built by a flat filesystem scan of the
workspace, deliberately independent of link resolution so alias links can
themselves be *spanning* (`contents: alias`) without a chicken-and-egg. A
document is indexed under both its `title` and its file stem, so `[[My File]]`
(by title) and `[[my-file]]` (by stem) both find it.

Resolution outcomes (`Workspace::resolve_link_with`, threaded through `tree` and
`check`):

- **Unique** — the one document with that name; resolves like any path.
- **Ambiguous** — several documents share the name; surfaced as
  `Target::AmbiguousAlias` / `NodeKind::AmbiguousAlias` / a
  `Finding::AmbiguousAlias` from `check`. A nominal link cannot choose, so this
  is a diagnosable error rather than a silent pick.
- **Unknown** — no document claims it; falls through to a path, so it reads as a
  missing/broken link exactly as before aliases existed.

Only *alias-shaped* targets (a bare name — no path separator, no extension) are
looked up; paths and `id:` targets are never diverted.

## Implementation status

- ✅ `ReferenceStyle` renderer + parsing (`link.rs`); the config-facing
  `Notation` (`markdown`/`wikilink`/`bare`) and `PathStyle`
  (`root`/`relative`/`canonical`) axes compose to the internal `Wrapper` +
  extended `LinkStyle` (the 2×3 cross-product); `id:` scheme with legacy
  `colophon:` read.
- ✅ Per-relation `style` on `Relation` (`relation.rs`).
- ✅ Workspace default + config keys (`config.rs`), authoring seam
  (`authored_target`) resolves per-relation style.
- ✅ `alias` **resolution** via the title index (`title.rs`), wired through
  `tree` and `check` (unique / ambiguous / unknown).
- ✅ Per-relation styles declared in a config surface: a `relations:
  { <name>: { notation, path_style, target, label } }` block, each axis optional
  and overlaid on the workspace default (`RelationStyleConfig` +
  `WorkspaceConfig::resolved_relation_styles` in `config.rs`;
  `RelationSet::with_styles` in `relation.rs`; wired through the CLI's workspace
  builder).
- ✅ `colophon init` surfaces the model *wrapper-first*: `--wrapper`
  (markdown / wikilink) and `--link-style` set the workspace `notation`/
  `path_style`, then `--reference` picks the addressing (`path` / `id` / `alias`
  / `split` — the up≠down diaryx shape). The choices write the `references`
  defaults and, for `split`, the `relations` block. `id`/`split` are gated on
  `--identity` ≠ none.
- ⏳ **Staged:** `StaleLabel` finding + label refresh in `validate.rs`.
  Body-prose reference restyle during the `mutate` port.
