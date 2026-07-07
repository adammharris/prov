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
  default.)

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
| Index store (id↔path registry) | `index` | ✅ `NoIndex` + `InMemoryIndex` + persistent `FileIndex` (sorted snapshot, tombstones, any fig format) |
| ID links (`colophon:<id>` targets) | `link`/`tree`/`validate`/`mutate` | ✅ resolve through the registry everywhere paths do; never rewritten by moves (the registry update is the maintenance); findings: `MalformedId` (check char), `DanglingId` (tombstoned vs never-issued) |
| Workspace composition + builder | `workspace` | ✅ type-flipping builder |
| Traverse (spanning tree from a root) | `tree` | ✅ `Workspace::tree`; missing/cyclic/unreadable targets are marked nodes |
| Scan (directory-driven discovery) | — | ⏳ waits on `StructureSource` |
| Mutation with link maintenance | `mutate` | ✅ first cut: `create`/`rename`/`delete` (parent entry, inverse links, re-relativization, labels kept; fig `Embed` edits). Remaining diaryx ops ⏳ |
| Validation | `validate` | ✅ findings: broken link, case mismatch, duplicate containment, missing inverse, unreadable. Autofix ⏳ |
| Storage adapter + executor | `fs`, `exec` | ✅ `StdFs` + dependency-free `block_on` |
| Link text + path arithmetic | `link` | ✅ labeled links, resolve/relative, lexical normalize |
| Single-document edits | `edit` | ✅ format-preserving `set`/`unset` over text |
| Multi-format embedded metadata | `document`/`meta` | ✅ read side (fig 2.1's `detect` + `split` *are* the fence layer); ⏳ format-preserving writes ride the mutation port |
| serde / fig backend split | — | ⏳ planned (feature gates) |
| `StructureSource` (filesystem intake) | — | ⏳ planned |

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
2. **How first-class is the filesystem `StructureSource`** — a genuine peer to
   frontmatter links, or an onboarding/migration convenience for users who have
   not adopted embedded links? This sets how much to invest in the abstraction.
3. **Is the internal colophon ID ever unified with the published permalink**, or
   do they stay two layers (internal minted opaque ID; external ARK permalink)?
   Model B keeps them separable; nothing yet forces them together.
