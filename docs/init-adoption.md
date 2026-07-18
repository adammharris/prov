```fig
part_of = [colophon](/README.md)
```
# `init` over existing content — detection & adoption

> Working design. What `colophon init` should do when the target directory
> already holds markdown (with or without frontmatter), an existing tree, or just
> unrelated files. Complements DESIGN §9 (extraction) and answers open question
> #2 (how first-class is the filesystem `StructureSource`).

## The gap today

`cmd_init` is **greenfield-only**:

1. It bails *only* if `index.{md,dj,html,yaml,…}` exists.
2. Otherwise it runs the interview and writes exactly two files — the root
   `index.<content-ext>` and `colophon.<meta-ext>` — and ignores everything else
   in the directory.

Two concrete failures follow in a non-empty folder:

- **Silent orphans.** Existing content files are never linked into the tree, and
  `validate.rs` has *no* orphan/unreachable finding, so `check` (which walks
  *from the root*) never mentions them. The structure silently omits most of the
  folder — the exact opposite of "hand it to any tool and it unfolds."
- **Config clobber.** The config write is unconditional and only `index.*` is
  guarded, so a pre-existing `colophon.yaml` / `registry.yaml` is overwritten
  with no check.

There is also a latent `find_root` interaction: existing files with frontmatter
but no `part_of` are all root *candidates*; `init` writing `index.md` wins the
tie-break but leaves the others as silent competing roots.

## Position

Running `init` over existing content **is** the filesystem-intake / migration
story. We commit to making it first-class:

- **Directory tree is a real structure source.** `init` can mirror the folder
  hierarchy into the containment tree (the "import an Obsidian/diaryx vault"
  path), behind a `StructureSource` seam — not merely a convenience.
- **Interactive by default.** On a terminal, `init` *detects* the situation and
  asks; non-interactive runs (`--yes`, no tty) that would need to touch existing
  files **refuse** unless an explicit `--adopt` opts in.
- **Adoption writes into documents, but only on opt-in and after a preview.**
  Because structure lives in documents (DESIGN §1), adopting a file means adding
  `part_of` frontmatter to it and `contents` to its parent. That mutation is
  never silent: previewed, idempotent, git-reversible, additive (never removes a
  user's `tags`/`aliases`/etc.).

## Detect first, then branch

Replace the single `index.*` guard with a **classification** of the target
directory, computed before the interview:

| Class | Signature | Behavior |
|---|---|---|
| **A. Greenfield** | empty, or only non-content files | Current behavior: write root + config, ignore the rest. |
| **B. Loose content** | content files, **none** carrying `part_of`/`contents` | Choose/mint a root, then offer **adoption** of the loose files (interactive; `--adopt` non-interactively). |
| **C. Already structured** | ≥1 file with `part_of`/`contents` (a diaryx/colophon-shaped tree) | **Do not mint a competing `index.md`.** Detect the existing root; attach **policy only** (write `colophon.<ext>` linked from it). Offer to adopt any still-loose files. |
| **D. Initialized** | a config document already present | Refuse; point to `config` / `check`. `--force` re-runs the interview. |

Cross-cutting rules (fix the footguns for every class):

- **Never overwrite** an existing `colophon.<ext>` / `registry.<ext>` /
  `index.<ext>` without `--force`.
- **Prefer an existing root** — an `index`/`readme` with frontmatter, or a lone
  no-`part_of` document — over minting `index.md`. When several no-`part_of`
  candidates exist: prompt to pick (interactive) or error listing them (batch).
- **Infer format from the adopted root.** When an existing file becomes the root,
  read its content grammar / embed style / metadata format from the file instead
  of asking — the interview shrinks to policy questions.

## The revised flow

1. Resolve + canonicalize the target dir.
2. **Classify** (below).
3. Class **D** → refuse (unless `--force`).
4. **Select the root**: existing candidate (C, or B/A with `index`/`readme`) vs
   mint `index.<ext>`.
5. **Interview for policy** (identity, id-storage, reference style, …). Content /
   embed / metadata questions are skipped when the root is adopted (inferred).
6. Class **B/C** with loose files → **adoption**: pick a strategy, show a
   **preview** (proposed tree + the exact files that gain `part_of` + the root's
   new `contents`), confirm.
7. **Apply**: write config (guarded), mint the root if needed, apply adoption as
   mutations (reusing `create`/link-maintenance), bootstrap the registry if
   identity will mint.
8. Summary + `next:` hint.

### Classification detail

Walk the target dir (recursively for the tree case). Collect:

- content files (`ROOT_EXTS`, per subdir), and which carry frontmatter;
- whole-file metadata docs (`META_EXTS`);
- whether any file declares a **structural** link (`part_of`/`contents`) → C;
- whether a **config document** exists (a whole-file doc with config keys, or one
  linked via the `config` relation) → D;
- **root candidates** (metadata + no `part_of`; `index`/`readme` stems preferred);
- the **directory shape** (which subdirs hold content) for the mirror plan.

## Adoption strategies

Two mappings from filesystem → containment tree, offered at the adoption prompt:

- **`mirror` (folder-as-node) — default for nested trees.** Every directory
  becomes a containment node: its `index`/`readme` if present, otherwise a
  **synthesized** folder-note index. A folder node's `contents` = its child files
  + child folder nodes; each child's `part_of` points up. A faithful, deep mirror
  where following links reproduces the filesystem — at the cost of minting index
  docs for bare folders.
- **`flat` — default for a single flat directory.** Only directories that already
  have an index become nodes; loose files attach to the nearest ancestor index.
  Mints nothing extra; loses per-folder grouping.
- **`none`** — init the root + config only; leave files loose (they show up as
  orphans in `check`, see below).

Rules that hold for every strategy:

- **Additive frontmatter only.** Add `part_of` (and `contents` on parents);
  preserve every existing field. A file that already has the right `part_of` is
  left untouched (idempotent → safe re-run / incremental adopt).
- **Titles** come from existing `title` frontmatter, else the first H1, else the
  filename titleized.
- **Body `[[wikilinks]]` are overlay references, not containment.** Adoption does
  not touch them; `check` resolves/reports them later. (A future pass can register
  them as overlay links / stamp ids — out of scope here.)

## `StructureSource` seam

Adoption is the first real caller of the plan-then-apply intake. As built
(`intake.rs`), the plan is a concrete value produced by `plan_mirror` (no trait
yet — see below):

```rust
struct StructurePlan {
    // folder-notes to create for bare directories, parents-first
    synthesized: Vec<SynthNode>,   // { path, parent, title }
    // existing files to link under a node
    adoptions: Vec<Adoption>,      // { child, parent }
}
```

- **`plan_mirror` + `apply_plan`** are the `mirror` mapping — the concrete
  `FilesystemSource`. `plan_mirror` only reads (previewable); `apply_plan` reuses
  `create` (synthesized notes) and `adopt` (existing files), so all the
  link-maintenance and identity hooks come for free.
- A **`StructureSource` trait** would abstract over alternative intakes — a
  `FrontmatterSource` (links already declared in docs; the class-C tree that
  needs no restructure) or a **hybrid** (filesystem fills the gaps frontmatter
  leaves). Deferred until a second source exists: one implementation behind a
  trait is premature abstraction.

`init --adopt mirror` = `plan_mirror` → (`apply_plan`) mutation batch. The
directory walk reuses `content_documents` (itself modeled on `scan_ids_dir` /
`scan_titles`).

## Companion: an orphan finding in `validate`

Add an `Orphan` finding — a content file on disk not reachable from the root.
This is independently valuable and it powers the rest:

- makes "you have N unadopted files" a **checkable** signal, not a one-time
  `init` message;
- gives an incremental `colophon adopt` (adopt files that appeared later) its
  work-list;
- is the honest "what's missing" answer DESIGN §8 wants from the sleeper feature.

Its autofix is exactly an adoption step (attach under a chosen parent), so it
shares the Phase-1 machinery.

## Flags

- `--adopt[=mirror|flat|none]` — non-interactive opt-in + strategy (default
  strategy chosen by tree shape).
- `--root <file>` — adopt an existing file as the root instead of minting one.
- `--dry-run` — classify, print the plan, write nothing.
- `--force` — permit overwriting an existing config/root (re-init).
- `--yes` — take defaults; still needs `--adopt` before it will touch existing
  files.

## Phasing

- **Phase 0 — safe & honest (small, ship first).** ✅ **Done.** `init`
  classifies the directory (`classify_dir` → `DirState`) and branches:
  greenfield initializes as before; an initialized workspace or an existing
  structured tree is refused with guidance (the tree case names its root and
  points at `colophon config`); loose content is confirmed interactively, noted
  non-interactively, and refused under `--yes` (so a script never orphans
  silently). The old narrow `index.*` guard is subsumed, and a `colophon.*`
  config is no longer clobbered. `validate` gained `Finding::Orphan` — a content
  document on disk that nothing reachable from the checked root links to —
  computed by diffing `Workspace::content_documents` against the walk's reachable
  set (census targets + separated-node `content` bodies + the start). No
  file-rewriting yet; the orphan is diagnosis-only (`suggest_fix` → `None`).
- **Phase 1 — flat adoption.** ✅ **Done.** `Workspace::adopt(child, parent)`
  (`mutate.rs`) authors both directions — the child's inverse up, the parent's
  spanning entry down — over an existing file without creating, moving, or
  rewriting its body. Additive and idempotent; refuses a contested parent
  (mirrors `suggest_fix` declining a claimed child); registers the parent when the
  workspace authors id links. `init` gained `--adopt flat|none` (and `mirror`,
  which errors as not-yet-built): `flat` links every loose document under the new
  root; the interactive path offers adopt / leave-unlinked / cancel. `Orphan`
  autofix rides the same primitive — `colophon check --fix` offers to adopt each
  orphan under the root. A path-only fix no longer bootstraps a spurious empty
  registry (the post-fix `ensure_registry` is now gated on the index being dirty).
  Still deferred: a dry-run/preview before writing, and per-file parent choice
  (batch adoption always uses the root).
- **Phase 2 — directory-tree import.** ✅ **Done.** `Workspace::plan_mirror`
  (`intake.rs`) walks the directory tree and returns a `StructurePlan` — the
  folder-notes to synthesize (parents-first) and the files to adopt, computed
  without touching disk so it can be previewed; `apply_plan` realizes it, reusing
  `create` for the folder-notes and `adopt` for the files. Every directory
  holding content becomes a node — its own `index`/`readme` when present, else a
  synthesized `index.<ext>` titled after the folder — so following links
  reproduces the filesystem. `create` gained a crate-internal `create_titled` so
  a synthesized note's title and its parent's spanning-entry *label* are authored
  in one step (no stale `[index]` labels). `init --adopt mirror` runs it (and the
  interactive menu offers "import the folder tree" whenever the loose files span
  subdirectories); a **separated** root — where folder-note grammar inheritance
  breaks — is refused by `plan_mirror` and `init` falls back to flat with a note.
  The `StructureSource` *trait* is deferred: with one concrete source
  (`FilesystemSource`, i.e. these two methods) an abstraction would be premature —
  it lands when a second source (frontmatter-only or hybrid) needs it. Still
  deferred: a `--dry-run` preview of the `StructurePlan`, and per-file parent
  choice.

## Rejected / non-goals

- **Sidecar-only tree** (record containment in the registry without editing
  files) — contradicts DESIGN §1 (structure lives in documents). Rejected.
- **Silent adoption / silent overwrite** — every file mutation is opt-in and
  previewed; existing config/root files are guarded.
- **Rewriting body wikilinks during adoption** — overlay-link registration is a
  later, separate pass.
