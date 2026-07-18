```fig
part_of = [colophon](/README.md)
```
# colophon — design & vision

> A *self-describing plaintext workspace*: a set of documents whose structure
> lives in the documents' own embedded metadata, not in the filesystem layout or
> an app-private sidecar folder.

This document captures the reasoning behind colophon — what it is, the positions
it takes, the positions it deliberately leaves open, and why. It is the crate's
north star; when a decision is unclear, it should be resolvable from here.

---

## 1. The thesis

A colophon (the bibliographic term) is the note in which a book describes its own
making — the type, the paper, the press. The crate takes that literally: a
colophon workspace is one you can hand to *any* tool and it explains itself.
Follow the links declared in each document's metadata and the whole structure
unfolds, anchored by a distinguished root that describes the whole.

The one inversion that defines the crate:

> **The edges of the document graph are declared *inside* the nodes
> (frontmatter), not by the container (the filesystem).**

That single move is the entire value proposition. A filesystem has no
self-description (structure is imposed from outside, by directory nesting). A
database has a schema, but it lives outside the data. A colophon workspace keeps
the structure *in the documents*, in plaintext, in the open — portable, diffable,
and legible without the app that produced it.

## 2. Opinionated mechanism, flexible vocabulary

The crate is opinionated about *how* structure works and agnostic about *what*
the structure is called.

- **Opinionated mechanism** — links live in embedded metadata; parsing goes
  through `fig`; there is a canonical containment tree; identity is additive;
  integrity is a first-class, checkable property. These are not configurable.
- **Flexible vocabulary** — *which* frontmatter fields are links
  (`contents`/`part_of`, `links`, or an entirely different set), their
  cardinality, their inverses, and which one is the canonical tree. This is
  configured per workspace via `RelationSet`.
- **Flexible source** (planned) — where the graph comes from: frontmatter links,
  the filesystem tree, or a hybrid, behind a `StructureSource` seam. Same
  downstream graph, different intake.

Nothing about "diary", "journal", or even "contents" is baked into the core.
`RelationSet::diaryx()` is merely a preset; the test suite proves a `part`/`whole`
vocabulary works identically with zero diaryx assumptions.

### Where configuration starts and ends — *does colophon read it back?*

The line above ("mechanism fixed, vocabulary flexible") raises a recurring
question every new field forces: is *this* thing configurable, and how far? The
operative test is one question — **does colophon read the value back and reason
about it?** — and it sorts every field into three tiers:

1. **Mechanism — not configurable.** How structure works, that integrity is
   checkable, that identity is additive, and *the format of anything colophon
   itself maintains*. A colophon-maintained value must be machine-standard
   because colophon has to parse, compare, and rewrite it reliably — years later,
   on another tool, after a merge. The `sha256:` fixity digest, the opaque id, the
   registry, an RFC 3339 `updated` timestamp: all live here. They are
   standardized *precisely because* they are not for human eyes but for machine
   reasoning.
2. **Vocabulary & representation — configurable.** The *names and surface
   spellings* of those mechanisms: which fields are relations, the spanning one,
   reference styles, id storage, embed format, the *name* of the `updated` field,
   whether a feature is on. Configuring a workspace means re-spelling colophon's
   fixed mechanisms for your vault — never redefining them. Essentially all
   colophon config lives here.
3. **Content — not colophon's business.** Everything colophon merely *carries*:
   `title`, a user's own `date`, arbitrary frontmatter, the body. colophon never
   reasons about these, so there is nothing to standardize and nothing to
   configure — the user owns them completely, today, just by writing them.

The tension this resolves is the seductive middle option: a colophon-*maintained*
field that the user also *formats*. That combination is incoherent — the moment
colophon must round-trip a value it has to understand the format, and parsing
arbitrary user formats (ambiguous, locale-dependent, lossy) is exactly the
fragility an archive cannot afford. So the real boundary is:

> **colophon-maintained ⟹ colophon owns the format. User-formatted ⟹ the user
> maintains it.** There is no "colophon maintains a field whose format it does not
> understand."

The corollary — **store canonical, render pretty**: human-friendly formatting is
a *presentation* concern (a viewer, an export), never a storage one. colophon
stores the canonical machine value (RFC 3339 `…Z`); a UI displays it however the
reader likes. This is why "let users choose the timestamp format" never becomes a
storage-config question: a user who wants a pretty date authors a `date` field
colophon never reads (tier 3), while the maintained `updated` field (tier 1/2 —
fixed format, configurable *name*) stays trustworthy. The two pulls stop fighting
because they are aimed at two different fields.

So: **config exists to rename and re-spell colophon's mechanisms — never to
reformat its machine-state (fixed) or to manage your content (already yours).**

## 3. Structure: a spanning tree with an overlay graph

Containment is a **tree**, not a general DAG — but that is a feature, not a
limitation, and the distinction is subtle enough to state precisely.

- Exactly one relation is marked **spanning**: single-parent, and the inverse of
  a "contains" relation. This is the workspace's *discovery spine*. Because every
  node has a unique path to a unique root, the question "what describes this
  workspace?" has one unambiguous answer. A pure DAG loses that — multiple
  parents, multiple roots, ambiguous discovery — and with it the self-describing
  property that motivates the whole crate.
- Every **other** relation may be many-to-many. Multi-membership ("this note
  belongs to two projects") is expressed through non-spanning relations, i.e. the
  overlay graph.

So the honest model is: **a single-parent containment tree (the backbone) with an
arbitrary reference graph laid over it.** The materialized index (§6) is what
makes the overlay edges as fast and first-class to query as the tree — so the
tree is a spine, never a ceiling. Nobody should *feel* limited to a hierarchy.

Framing rule: it is never "tree vs DAG." It is "one spanning relation + N
overlay relations," where cardinality is per-relation config and exactly one
relation is designated spanning.

## 4. Identity is a strictly-additive layer

The load-bearing architectural commitment:

> **The graph, traversal, and mutation layers operate on paths and never require
> an ID. Identity is a resolver + a registry bolted *on top* of a fully
> functional path-only workspace.**

This is what makes identity genuinely optional rather than "technically a no-op."
A move rewrites path-based frontmatter links because that is inherent to a linked
workspace; *if* an identity layer is present, it also updates `id → path` in the
registry — an optional step, gated on whether a registry exists. Nothing in
containment, traversal, ordering, or validation ever dereferences an ID.

In the type system: `Workspace<FS, Id = NoIdentity, Ix = NoIndex>`. Paths-only is
the *absence* of the subsystem — it monomorphizes out, produces no sidecar
artifact, and (crucially, see §5) writes no ID into any document. Opting in flips
a type parameter via one builder line.

### Derived vs registered — the minimal authoritative set

An ID is registered only when something creates a **durable, out-of-location
reference** to a document. Everything else stays derived and costs nothing. So:

> **The registry contains exactly the set of IDs that something external depends
> on being stable.**

This is the same idea as the original "IDs are rederivable unless published" plan,
seen from the publish side. Publishing (a `permalink`) and linking-by-ID are the
registration triggers; the other ~95% of files nothing points at pay no
identity-maintenance tax. It also shrinks the dangerous, merge-critical,
must-not-lose surface to its minimum.

### Model B — IDs are minted at registration, not derived

Two coherent schemes were considered:

- **A — derived-then-frozen.** Every file has a deterministic ID (a hash of its
  path); registration snapshots it. Requires collision handling and makes
  "stable" a happy accident of not-moving.
- **B — minted at registration (chosen).** Unregistered files have *no* opaque ID
  at all — they are addressed by path (`[./notes/file]`). The opaque ID is *born*
  the moment a document is linked-by-id or published.

B is chosen because the model falls out clean:

- Every opaque ID is authoritative **by construction** — there are no derived
  opaque IDs to reconcile, no path-hash collision dance.
- "Rederivable unless published" becomes literally true: unregistered = addressed
  by path (nothing stored); registered = minted, in the registry.
- It cleanly separates the two identity layers: the **internal colophon ID**
  (minted opaque, for in-workspace stable links) and the **published permalink**
  (an ARK blade in diaryx, for external URLs). Publishing implies registration;
  linking-by-id implies registration; they are distinct events, and the internal
  ID need not equal the permalink.

The one cost — a UI cannot show a stable short handle for a file until it is
linked — is judged negligible.

### The registration lifecycle

`Registration { on_create, on_link, on_publish }` is the dial:

- **OFF** — paths only.
- **LAZY** (recommended default) — register on link-by-id or publish.
- **EAGER** — also register on create. This regrows the registry to *every* file,
  forfeiting the minimal-authoritative-set benefit; it is a legitimate choice for
  users who want stable-identity-from-birth, and it is one flag.

Registration needs **two paths**, and both must exist:

1. **Eager** — when colophon itself authors an ID reference, it registers
   atomically.
2. **Reconciling** — a validation pass scans for `[[colophon:id]]` references that
   arrived out-of-band (paste, `git merge`, another editor) and registers/repairs
   them. This reuses the validation module (§7).

**Known hazard:** the one unrecoverable case is an out-of-band edit that inserts a
raw ID reference and then moves the target *before* colophon ever reconciles. The
durable reference was created behind colophon's back; nothing can save it. The
mitigation is reconcile-on-load, and this is documented as a limitation rather
than pretended airtight.

## 5. The index: one artifact, two natures

The ID registry, the materialized graph, and the resolution cache all want to be
the *same* artifact — one `IndexStore` that colophon keeps consistent as part of
its normal mutation job, serialized (via `fig`) to any supported format and
stored anywhere. That convergence is elegant, but it hides a sharp edge that the
design must respect:

> **The index fuses two natures. The graph/resolution parts are a *derived
> cache* — a pure function of the documents, always rebuildable, harmless when
> stale. The `id → path` registry (under model B, where IDs live only in the
> index) is *authoritative, non-derivable state* — it cannot be rebuilt from the
> documents.**

Consequences the implementation must honor:

- **Structurally separate the two even inside one store.** A `derived` section
  (disposable, blow-away-and-regenerate) and a `registry` section (durable,
  backed up, merge-critical). Fusing them into one undifferentiated blob loses
  the cache's safety valve.
- **The registry's write belongs in the same unit as the documents it describes.**
  This falls straight out of the two natures. A derived cache may lag: if it is
  stale, rebuild it. Authoritative state may not — a mutation that maintains three
  documents' links but loses its `id → path` update leaves every `colophon:<id>`
  reference to the moved document resolving to nothing, and *nothing in the
  workspace can repair it*, because the mapping was never in the documents to
  begin with. So a mutation stages its registry write into the same `ChangeSet`
  (`colophon/src/change.rs`) as its document edits, and the two land or fail
  together. The corollary is the honest half: the frontmatter *shadow* copy
  (`id_storage: frontmatter`) is derived — idempotently re-stamped from the
  registry on any later run — so it is deliberately left outside that unit. What
  can be rebuilt need not be transactional; what cannot, must be.
- **A single central index file is a merge/write-contention hotspot.** Every
  mutation on every device touches it, so every sync touches it — re-concentrating
  exactly the contention that per-file frontmatter avoids. This is why
  `IndexStore` is a trait: sync can back the registry with something
  non-file-shaped (per-doc sidecars, an append-only log, a Durable Object). When
  the store *is* a file, its on-disk format must be designed for clean diffs
  (stable/sorted ordering, one record per line).
- **Optional escape hatch.** A "stamp IDs back into frontmatter" operation gives
  the index's cleanliness as the working model *plus* a durable, portable,
  rebuildable backup. If frontmatter carries a shadow copy of the ID, the registry
  becomes rebuildable again — i.e. back to a pure cache. (This trades away some of
  model B's document-cleanliness; it is a per-deployment choice, not a crate-level
  default.) **Implemented** as the `id_storage` axis (`IdStorage`):
  `frontmatter` keeps the shadow copy *and* the cache; `frontmatter_only` drops
  the registry entirely and rebuilds id→path from a scan (`Workspace::scan_ids`),
  at the cost of tombstones. The ID then travels with the file — copy- and
  out-of-band-move-robust — since a move no longer needs a registry update.

The materialization also serves performance: today the graph is derived by
walking the spanning relation from a root on demand; an optional materialized
index (id/path → node, adjacency, precomputed inverses) serves callers doing many
queries — an LSP, a static-site builder, a TUI — without re-walking.

## 6. Wikilinks and positioning

The user-facing payoff of registered identity is stable, location-independent
links: `[[colophon:ajp7eq|My file]]`. Authoring such a link, or publishing, *is*
the registration event.

This is deliberately Obsidian-shaped, with one decisive difference:

> **Obsidian, except the user owns what `.obsidian/` used to own.**

Obsidian's link-rewrite-on-rename, its graph, its block IDs — all of that
intelligence lives *in the app*, and the state lives in an opaque dotfolder the
user cannot read with another tool. colophon inverts exactly that: the same
superpowers (stable IDs, backlinks, rename-safety), but the identity state is
*data the user owns* — in their tree, in any `fig` format, versioned with their
content. colophon is that vault intelligence as a portable, embeddable library.

Ownership alone is not enough, though: a readable registry in an unlinked
dotfolder is still `.obsidian/` with a nicer file format. The property that
actually distinguishes a colophon workspace is **reachability** — the root
document links its registry through the `registry` relation, so following the
links from the root discovers the identity state like everything else. Where
the registry lives is a fact about the workspace, declared in the workspace;
the registry document self-describes (`title`, `part_of` back to the root) and
can be a bare config file (`registry.yaml`, fig-native, …) or the frontmatter
of an ordinary prose document — it is a first-class node, validated by `check`
like any other link.

## 7. Serialization and embedded formats

- **`fig` value tree is the common currency.** Access is dynamic (link fields are
  configurable, so a fixed struct will not do). The parse/serialize paths are
  serde-free — they walk `fig`'s native tree — which keeps serde out of the call
  graph and out of a WASM binary. This mirrors the proven approach in
  `diaryx_core`'s `yaml` module (including the `width(1)` block-layout fix for
  fig 2.0's flow-style default).
- **`fig` and `serde` both behind features.** `fig` has its own `serde` feature,
  so targeting the value tree as the common currency makes the backend a
  build-time choice the core never sees. `fig` is already published in multiple
  places; shipping it natively is defensible (the "fig + colophon" ecosystem),
  while a `serde` backend keeps the door open for those who do not want the Zig
  toolchain `fig`'s build requires.
- **Multi-format embedded metadata.** The crate is agnostic about the *format*
  of the embedded block — YAML (`---`), JSON (`;;;`), fig-native
  (```` ```fig ````), endmatter — anything `fig` recognizes. The **fence layer**
  turned out to live in `fig` itself, not colophon: `fig::detect` sniffs the
  archetype (fig 2.1, upstreamed from this project's needs), `fig::split`
  separates content from body, and `EmbedType::inner_format` couples each fence
  style to its format so invalid combinations are unspellable. colophon records
  the detected `EmbedType` on every `Document` so writes **preserve the original
  format and layout** (never rewrite a ```` ```fig ```` block as YAML). colophon
  feature gates (`yaml`, `json`, `fig`, …) forward to the corresponding `fig`
  feature. A useful consequence: the sidecar index need not match the document
  format — documents can be YAML while the index is fig-native for parse speed.

## 8. Validation is the sleeper feature

Integrity-checking with autofix is the rarest, most reusable asset in this space
and should be a loud, first-class feature — not a footnote. The model: a set of
`ValidationError` variants (broken spanning-parent, broken contains-reference,
orphan, cycle-where-disallowed, missing backlink, dangling/unregistered ID) plus
warnings and an autofixer. It returns findings; it does not panic. It also hosts
the reconcile pass from §4 (an unregistered `[[colophon:id]]` reference is just
another finding with an autofix: register it, or flag it if it cannot resolve).

**Discovery is reachability-bounded.** The orphan check does not scan the whole
subtree — it inspects only the directories a linked document already occupies,
and never recursively. A subdirectory nothing links into (a vendored tree, a
nested colophon workspace, a `scratch/` folder) is neither read nor reported, so
`check` stays quiet about files that were never opted in. This is the same
"invisible unless attached" rule §3's reachability applies to files, extended to
directories: a directory enters scope only through an explicit act that links
into it (`new`, `adopt`, `attach`, a `mirror` import), after which `check` keeps
it honest — and scope grows with the links. The deliberate trade is that a
document dropped into a not-yet-linked folder is invisible rather than flagged;
the alternative (flagging every stray file anywhere beneath the root) makes
colophon unusable inside a larger repo. The recursive filesystem walk survives
only where it is an *explicit* import — `content_documents`/`plan_mirror` for
`init --adopt mirror`, and `attach --all --recursive` — never in steady-state
validation.

**The title index is bounded too.** It is built *lazily* — only when a
`[[alias]]` link is actually encountered, so a path/id workspace (the diaryx
default) never scans at all — and when it is built, it is scoped to the reached
directories: a cheap path/id pre-pass (`title_scope`) collects the directories
the tree occupies, and only those are indexed. So an alias resolves within the
workspace without reading `target/`, a vendored tree, or a nested workspace at
the repo root, and a same-titled document in an unreached subtree cannot collide
with a workspace title. The one case that cannot be bounded is an **alias-addressed
spanning** relation: descending the tree then needs every title up front (the
chicken-and-egg the flat scan avoids), so `title_scope` reports it and the build
falls back to the full whole-tree scan. (The frontmatter-id registry, §5, still
scans whole-tree — bounding it has the same spanning-id coupling and is a separate
step.)

## 9. Extraction discipline & status

colophon is being extracted from `diaryx_core`. Guiding rules:

- **Read + write from the start.** The valuable, hard half is safe restructuring
  with link maintenance, which `diaryx_core` already does across ~18 mutation
  ops. There is no read-only milestone; a release waits until diaryx *can* depend
  on colophon.
- **A beautiful API that forces a diaryx rewrite beats an ugly one that changes
  nothing.** Design the seams for their own sake; let real diaryx usage carve the
  ergonomics.
- **Guard the public surface.** Every diaryx-specific concern — ARK minting, the
  publish/audience/gate/theme config, config migrations — stays behind the
  profile and the `IdentityPolicy` / `IndexStore` / `StructureSource` traits, so
  none of it can calcify into the public API. The dual document model
  (`diaryx_core`'s path-based frontmatter vs id-based sync records) reconciles
  here: id-based vs path-based becomes a choice of policy + resolver, not two
  parallel type hierarchies.
- **Sequence.** Extract in place → diaryx depends on a local `colophon` → dogfood
  until the seams (IndexStore, format layer, registration) are proven → publish
  last.

### Current status

The pure layers are real and tested; the filesystem-driven engine is staked but
not yet ported.

| Area | Module | Status |
| --- | --- | --- |
| Embedded metadata (parse/serialize, dynamic value) | `meta` | ✅ implemented + tested (format-parametric) |
| Document splitting (frontmatter fence) | `document` | ✅ all fig archetypes via `fig::detect` (`---`, `;;;`, ```` ```fig ````, endmatter); `EmbedType` recorded per document |
| Relation vocabulary + edge/child extraction | `relation` | ✅ implemented + tested |
| Identity policy + registration triggers | `identity` | ✅ betanumeric+check minter (ARK lineage, no shoulder), `Trigger` events, `Workspace::register` (idempotent, policy-gated), mint-by-rejection |
| Index store (id↔path registry) | `index` | ✅ `NoIndex` + `InMemoryIndex` + persistent `FileIndex` — records live under the `registry` key of a *workspace document* (bare config file or markdown frontmatter alike), tombstones as `id: null`, block layout, per-record preserving upserts |
| Identity storage axis (§5 escape hatch) | `config`/`workspace`/CLI | ✅ `IdStorage`, spelled `id_storage: both` (**default**: stamp each doc's own `id` field + keep the registry as a rebuildable cache) · `registry` (only in the registry document) · `frontmatter` (no registry; id→path rebuilt by `Workspace::scan_ids`, tombstones forfeited). `init` prompts frontmatter vs registry; `--id-storage frontmatter-only` reaches the third. Frontmatter storage makes identity move/copy-robust — the ID travels with the file |
| Registry reachability | `relation`/`workspace` | ✅ the root links its registry via the `registry` relation (in the diaryx preset); `Workspace::registry_path` discovers it by following the link — never an app-private sidecar path |
| Config files as documents | `document`/`edit` | ✅ `.yaml`/`.yml`/`.json`/`.fig`/`.figl` parse as whole-file-metadata documents (`MetaCarrier::WholeFile`); carrier-aware `MetaEditor` edits both shapes preserving comments/format |
| Config vocabulary + homes | `config`/CLI | ✅ one nested namespace (`docs/config-vocab.md`) with two homes — nested under `colophon:` in the root's frontmatter (the description home) or top-level in the dedicated config document (the policy home), precedence *config doc > root block > default*; the `config`/`registry`/`recycle_bin` **pointer relations** stay top-level as structure. Reference style is the orthogonal `references: { notation (markdown\|wikilink\|bare) × path_style (root\|relative\|canonical), target, label }` (internally `LinkStyle` extended to the 2×3 cross-product); `metadata: { format, embed }`; `id_storage: registry\|frontmatter\|both`; a `spec` version marker. `colophon config <k> <v>` addresses nested axes by dotted key and refuses to write a setting `check` would flag |
| ID links (`colophon:<id>` targets) | `link`/`tree`/`validate`/`mutate` | ✅ resolve through the registry everywhere paths do; never rewritten by moves (the registry update is the maintenance); findings: `MalformedId` (check char), `DanglingId` (tombstoned vs never-issued) |
| Workspace composition + builder | `workspace` | ✅ type-flipping builder |
| Traverse (spanning tree from a root) | `tree` | ✅ `Workspace::tree`; missing/cyclic/unreadable targets are marked nodes |
| Scan (directory-driven discovery) | `workspace`/`intake` | ✅ the orphan check is **reachability-bounded** (`direct_child_files` over reached directories, §8) — quiet inside a larger repo; the recursive `Workspace::content_documents` survives only for explicit imports (`plan_mirror` → a `StructurePlan`, `attach --all --recursive`) |
| Mutation with link maintenance | `mutate` | ✅ `create`/`rename`/`delete`/`recycle`/`restore`/`empty_bin`/`adopt`/`separate`/`combine`/`duplicate` (parent entry, inverse links, re-relativization, labels kept; fig `Embed` edits). `recycle` is the recoverable delete (see the recycle-bin row); `delete` is the hard one. `adopt` links an *existing* file both ways without touching its body — the onboarding complement of `create` (`docs/init-adoption.md`, Phase 1), driving `init --adopt` and the orphan autofix. `duplicate` copies a node as a fresh sibling under the same parent (fresh name, **no** cloned ID or children — a shallow copy, so no child gains a second parent), copying a separated node's body file too. **Non-goals vs diaryx** (`convert_to_index`/`convert_to_leaf`, `attach_and_move_entry_to_parent`): these reify diaryx's *directory-shaped* containment — a node earns children by becoming a folder, attaching moves the file into the parent's directory. colophon's containment is link-shaped (§3, §8): a node gains contents in place and `adopt` links without moving, so there is nothing to convert between and no move-on-attach. The external id-sync hooks (`sync_*_metadata`) are folded into the per-op index maintenance behind the `IndexStore` seam (§9) |
| Validation | `validate` | ✅ findings: broken link, case mismatch, duplicate containment, missing inverse, unreadable, malformed/dangling id, ambiguous alias, **id mismatch** + **unregistered id** (the frontmatter-storage reconcile pair), **orphan** (a content document on disk nothing reachable links to — the onboarding signal, `docs/init-adoption.md`), **fixity mismatch** (see the fixity row), **config issue** (a key in either config surface — the root's `colophon:` block or the linked config document — that `WorkspaceConfig::apply` silently ignores: a misspelled key that resembles a real axis, or a recognized axis with a value colophon can't parse; `config::diagnose` is the shared judgment, so `colophon config <key> <value>` refuses to *write* the same, and near-miss detection leaves user-owned fields alone per §2), **config spec-ahead** (a surface declares a `spec` newer than this build understands, so newer settings may be ignored — `config::spec_ahead`, shared with the CLI's proactive warning). Autofix: missing inverse ✅; id mismatch → trust the registry (rewrite frontmatter) ✅; unregistered id → adopt into the registry ✅; fixity mismatch → re-stamp to the current bytes (confirmation-gated) ✅; orphan + config-issue + config-spec-ahead + body-link findings stay diagnosis-only |
| Fixity (content checksums) | `fixity`, `attach`, `validate` | ✅ the archival integrity question link-checking cannot answer — *are the bytes still the bytes?* A dependency-free, NIST-vector-tested SHA-256 (WASM-clean, spelled `sha256:<hex>` so an auditor verifies it with `sha256sum` — tool-agnostic like everything else). `check` grows a bit-rot pass over the reachable set, emitting `FixityMismatch`; it honors any recorded hash regardless of the fixity *setting* (which governs what is written), never false-alarming on an unrecorded or unrecognized digest. Config axis `fixity: off \| attachments \| all` (default **attachments**). `attachments` — attachment sidecars record a `content_hash` of their bytes (frictionless: a payload is never edited, so a change is unambiguously corruption). `all` — documents additionally hash their *body* (never frontmatter, so colophon's own link maintenance never disturbs it); because a body is editable, `colophon edit` opens `$EDITOR` and restamps on save, and an out-of-band edit is a re-stampable finding rather than a hard error (`Workspace::restamp_fixity`) |
| Storage adapter + executor | `fs`, `exec` | ✅ `StdFs` + dependency-free `block_on`. The port **declares durability capabilities** (`Capabilities { atomic_replace, durable_sync, native_transactions }`, pessimistic by default) so the crash-safety layer adapts to each backend rather than assuming — `StdFs` reports `LOCAL_FS`, and the OPFS/IndexedDB adapters will report their own (IndexedDB's `native_transactions` earning it a bypass of the journal). `sync` (fsync file + parent dir, the dir step `#[cfg(unix)]`) and `write_atomic` (write-temp → sync → rename → sync, degrading honestly where atomic rename is absent) are the two primitives; every `FileOp::Write` lands through `write_atomic` |
| Crash-atomic change sets + journal | `change`, `journal` | ✅ **error atomicity** (in-memory unwind on any failed write) **and crash atomicity**. Per file: `write_atomic` means no document is caught half-written even by a power cut. For the whole set: a checksummed write-ahead journal — a single transient root dotfile (`.colophon-journal`), written and flushed *before* any document (the commit point), removed on success — that `recover` (run by `check`) rolls forward idempotently after a crash. An interrupted set therefore always resolves consistent: fully-before on a caught error, fully-after on a crash. The registry write still rides the same set (§5). Remaining: an IndexedDB backend delegating to its native transaction instead of the journal |
| Recycle bin (recoverable delete) | `mutate`, `relation`, `config` | ✅ a first-class, **reachable** member (a `recycle_bin` pointer relation off the root, discovered by `Workspace::recycle_bin_path` — the registry's anti-`.obsidian/` move, §5/§6), not an app-private folder. `recycle` moves a document (bytes verbatim) into a visible `recyclebin/` and records a tombstone in its self-describing index (validated by `check`); the bytes park under an *unreached* `recyclebin/items/` so §8's orphan check ignores them. `restore` reverses it losslessly (bytes back, parent re-linked, ID re-registered); `empty_bin` is the only hard purge, always explicit. All three are one journaled ChangeSet. Config axis `recycle_bin` (on by default — the safe archival posture — opt-out per workspace); the CLI routes `rm` to the bin unless `--purge`, and adds `restore`/`empty-bin` |
| Link text + path arithmetic | `link` | ✅ labeled links, resolve/relative, lexical normalize |
| Single-document edits | `edit` | ✅ format-preserving `set`/`unset` over text; `colophon edit` opens `$EDITOR` and, on a real content change, calls `Workspace::record_content_update` — one crash-safe write that restamps the fixity checksum (under `all`) and stamps the configured `updated` field (empty = off) with an RFC 3339 UTC instant the CLI supplies (the library stays clockless, DESIGN §2). Gated on actual change (byte-compared across the editor), so an open-and-quit stamps nothing |
| Multi-format embedded metadata | `document`/`meta` | ✅ read side (fig 2.1's `detect` + `split` *are* the fence layer); ⏳ format-preserving writes ride the mutation port |
| serde / fig backend split | — | ⏳ planned (feature gates) |
| Filesystem intake (`mirror` import) | `intake` | ✅ `plan_mirror` → `StructurePlan` (previewable) → `apply_plan` folds a directory tree into the containment tree, synthesizing folder-notes for bare dirs and reusing `create`/`adopt`; drives `init --adopt mirror` (`docs/init-adoption.md`, Phase 2). The `StructureSource` *trait* (frontmatter/hybrid sources) is deferred until a second source needs it |
| Route addressing (`mkdir -p` for containment) | `route` | ✅ `plan_route` → `RoutePlan` (previewable) → `apply_route` walks a route (`Daily/2026/2026-07` — each segment the *title* of a child of the last) from a start document and synthesizes the segments that don't resolve, reusing `intake`'s `SynthNode` + `create_titled`. Drives `colophon new --under <route> -p [--layout nested\|flat]`, whose point is the recurring-entry workflow (a daily note whose month index doesn't exist yet on the 1st). Vocabulary-neutral by construction — no date, no "daily", nothing diaryx (§2): the shell supplies the policy, colophon the one part a shell can't express. `Layout` governs *file placement only*, never the graph. Resolution is bounded (only the children of nodes on the route are read) and does **not** trip §8's alias-spanning hazard, since it descends from a known node rather than needing every title up front |

## 10. Open questions

1. ~~**Does the ID registry ever need to survive without its documents?**~~
   **Answered: yes, minimally — tombstones, not history.** Deleting a document
   retires its ID to a tombstone (`id: null` in the snapshot): the ID stops
   resolving but is never forgotten, so mint-by-rejection can never reissue it
   and a dangling `colophon:` reference stays *diagnosable* ("that document was
   deleted" vs "never issued here"). This is cheaper than an append-only log —
   the registry stays a sorted, diff-friendly snapshot — while still refusing to
   let an ID silently change meaning. Full history/event-log stores remain
   possible behind `IndexStore` (e.g. for sync), but the file-backed default
   does not need them.
2. ~~**How first-class is the filesystem `StructureSource`**~~ **Answered: a
   genuine peer, realized concretely first.** The filesystem is a real structure
   source — `init --adopt mirror` folds a whole directory tree into the
   containment tree (synthesizing folder-notes), not merely a flat convenience
   (`intake.rs`, `docs/init-adoption.md` Phase 2). But it landed as concrete
   methods (`plan_mirror`/`apply_plan`), not a `StructureSource` trait: with one
   implementation the trait would be premature. The abstraction (peer
   frontmatter/hybrid sources) is deferred until a second source demands it — so
   the answer is "first-class in capability, un-abstracted until it pays for
   itself."
3. **Is the internal colophon ID ever unified with the published permalink**, or
   do they stay two layers (internal minted opaque ID; external ARK permalink)?
   Model B keeps them separable; nothing yet forces them together.
