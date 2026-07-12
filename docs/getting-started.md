---
title: Getting Started with colophon
part_of: '[colophon](/README.md)'
---

# Getting Started with colophon

A beginner's guide to the `colophon` command line. By the end you'll have a small
workspace, understand how its structure is stored, and know every command you
need for day-to-day use.

> **What colophon is, in one sentence.** A *self-describing plaintext
> workspace*: a set of documents whose structure lives in the documents' own
> frontmatter, not in the folder layout or an app-private sidecar. Follow the
> links from a root document and the whole workspace unfolds. See
> [DESIGN.md](DESIGN.md) for the reasoning behind that idea.

---

## 1. The mental model

Three ideas carry everything else.

- **Documents** are plaintext files (`.md`, usually) with an embedded metadata
  block — YAML frontmatter between `---` fences:

  ```markdown
  ---
  title: Rust
  part_of: '[My Vault](/index.md)'
  ---

  # Rust

  Body prose goes here.
  ```

- **Relations** are the named links in that metadata. colophon ships with the
  *diaryx* vocabulary:

  | Relation   | Direction        | Meaning                                    |
  | ---------- | ---------------- | ------------------------------------------ |
  | `contents` | parent → child   | "this document contains these"             |
  | `part_of`  | child → parent   | the inverse — "this belongs to that"       |
  | `links`    | any → any        | a loose cross-reference (an *overlay* link) |
  | `registry` | root → registry  | where stable IDs are recorded (§7)         |
  | `config`   | root → config    | where workspace settings live (§8)         |

- **The spanning tree.** Exactly one relation is the *spanning* relation —
  `contents`/`part_of` here. It is single-parent, and it is the workspace's
  discovery spine: every document has one path back to one **root**. Every other
  relation (like `links`) may be many-to-many, laid over the tree as a graph.

The root is just a document that nothing contains — it has no `part_of`.
colophon finds it by walking up from your current directory until it sees a
`.md` file with metadata and no `part_of` (an `index.md` or `README.md` wins
ties).

---

## 2. Install

colophon builds from source and needs two toolchains:

- **Rust** (`cargo`) — to build colophon itself.
- **Zig** — colophon's metadata parser (`fig`) and body parser (`twig`) are
  Zig-backed and compile during the build.

`twig` is currently a path dependency, so clone it *next to* colophon:

```console
$ git clone https://github.com/adammharris/twig
$ git clone <colophon-repo> colophon
$ cd colophon
$ cargo build --release
```

The binary lands at `target/release/colophon`. Put it on your `PATH`, or invoke
it by full path. Every example below uses the command name `colophon`.

---

## 3. Create a workspace

`init` sets up a workspace: a self-describing root document plus a config
document that records your preferences. On a terminal it walks you through a
handful of choices:

```console
$ colophon init my-vault
┌  colophon init
│
◇  Title ·············· My Vault
◇  Author ············· (blank)
◇  Metadata format ···· YAML
◇  Content format ····· Markdown
◇  Link style ········· Markdown, workspace-absolute
◇  Identity ··········· On demand — an ID on link-by-id or publish
◇  Links between documents ··· By path
│
└  initialized /home/you/my-vault
   root: index.md — My Vault
   config: colophon.yaml — metadata yaml, content markdown, link style markdown_root, identity lazy, links path
   next: colophon new <path> --parent index.md
```

Identity is **two independent choices** (see [§9](#9-stable-ids-optional)): when a
document earns a stable ID, and whether colophon writes its structural links by
ID or by path. The second prompt only appears when identity isn't off. The
choices:

| Prompt              | Default                     | Options                                              |
| ------------------- | --------------------------- | ---------------------------------------------------- |
| **Title**           | the directory's name        | any text                                             |
| **Author**          | omitted                     | any text, or blank to leave it out                   |
| **Metadata format** | `yaml` (`---` frontmatter)  | `yaml`, `json` (`;;;`), `fig` (```` ```fig ````)     |
| **Content format**  | `markdown`                  | `markdown` (`.md`), `djot` (`.dj`), `html` (`.html`) |
| **Link style**      | `markdown-root`             | see [§10](#10-workspace-config)                      |
| **Identity**        | `lazy` (on demand)          | `off` (paths only), `lazy`, `eager` — see [§9](#9-stable-ids-optional) |
| **Links between documents** | `path`              | `path`, `id` (survive moves) — shown only when identity ≠ `off` |

The root-shaping choices come first; the rest are **workspace
preferences**. All of them are written into a config document (`colophon.yaml`,
or `colophon.json` / `colophon.figl` if you chose that metadata format — linked
from the root) so the workspace records how it wants to be authored — see
[§10](#10-workspace-config). The **content format** also sets the root file's
extension and body grammar; `twig` (colophon's body parser) handles all three.

Every choice is also a flag, so you can skip the prompts. Pass `--yes` (`-y`) to
take all defaults, or set some and be prompted for the rest:

```console
$ colophon init my-vault --content djot --identity lazy --links id --yes
initialized /home/you/my-vault
  root: index.dj — My Vault
  config: colophon.yaml — metadata yaml, content djot, link style markdown_root, identity lazy, links id
next: colophon new <path> --parent index.dj
```

Flags: `--title`, `--author`, `--meta <yaml|json|fig>`, `--content
<markdown|djot|html>`, `--link-style <markdown-root|markdown-relative|plain-relative|plain-canonical>`,
`--identity <off|lazy|eager>`, `--links <path|id>`, `--yes`. (`--links id` needs
identity, so it's rejected with `--identity off`.) With no arguments, `init` initializes
the current directory. It refuses to run where a workspace root already exists,
so it's safe to re-run by mistake.

```console
$ cd my-vault && cat index.md
---
title: My Vault
config: colophon.yaml
---

# My Vault
```

---

## 4. Grow the tree with `new`

`new` creates a document *and* wires up both directions of the spanning link —
the parent gains a `contents` entry, the child gets a `part_of` back.

```console
$ colophon new topics/rust.md --parent index.md
created topics/rust.md (in index.md)

$ colophon new topics/zig.md --parent index.md
created topics/zig.md (in index.md)
```

`new` creates intermediate folders (`topics/`) as needed. Look at what it wrote:

```console
$ cat index.md
---
title: My Vault
contents:
- '[rust](/topics/rust.md)'
- '[zig](/topics/zig.md)'
---

# My Vault

$ cat topics/rust.md
---
title: rust
part_of: '[My Vault](/index.md)'
---
```

The links are ordinary Markdown links written into the metadata. Nothing about
the structure lives in the filesystem — move `index.md` to another machine with
these two files and it still describes the same tree.

---

## 5. See the workspace

`tree` prints the containment tree, discovered by following `contents` from the
root:

```console
$ colophon tree
index.md — My Vault
├── topics/rust.md — rust
└── topics/zig.md — zig
```

`show` summarizes one document — its title, spanning children, and overlay
links:

```console
$ colophon show index.md
index.md
  title: My Vault
  contents (2 children):
    - [rust](/topics/rust.md)
    - [zig](/topics/zig.md)
```

More single-document readers:

| Command                    | Prints                                             |
| -------------------------- | -------------------------------------------------- |
| `colophon meta FILE`       | the raw metadata block (no fences)                 |
| `colophon get FILE KEY`    | one field by dotted path (`title`, `contents.0`)   |
| `colophon links FILE`      | every link as `relation⇥target`                    |
| `colophon body FILE`       | everything *outside* the metadata block            |
| `colophon backlinks FILE`  | who links *to* this document, across the workspace |

```console
$ colophon backlinks index.md
topics/rust.md	part_of	path
topics/zig.md	part_of	path
```

---

## 6. Edit metadata

`set` and `unset` change a field while preserving the file's formatting,
comments, and metadata format. `set` even creates the block if a document has
none.

```console
$ colophon set topics/rust.md summary "Notes on the Rust language"
$ colophon get topics/rust.md summary
Notes on the Rust language

$ colophon unset topics/rust.md summary
```

Values are typed by inference: `true`/`false`, integers, floats, and `null`
become those types; everything else is a string. Dotted keys address nested
fields and sequence indices (`contents.0`).

### Body prose and `render`

The *body* is everything after the frontmatter. colophon can render a
Markdown/Djot body to HTML, and it understands code — a `[[…]]` inside a code
span is treated as code, never as a link:

```console
$ colophon render topics/rust.md
<h1>Rust</h1>
<p>Inline <code>let x = [[1,2],[3,4]];</code> is code, not a link.</p>
```

`render` picks the grammar from the extension: `.md`/`.markdown` → Markdown,
`.dj`/`.djot` → Djot, `.html`/`.htm` → HTML.

---

## 7. Restructure safely: `mv` and `rm`

This is colophon's payoff. `mv` moves a file **and rewrites every link that
pointed at it** — the parent's `contents` entry, the moved file's own relative
links, overlay links, and body wikilinks across the whole workspace.

```console
$ colophon mv topics/rust.md topics/rust-lang.md
moved topics/rust.md -> topics/rust-lang.md

$ colophon tree
index.md — My Vault
├── topics/rust-lang.md — rust
└── topics/zig.md — zig
```

`rm` deletes a document and removes its parent's `contents` entry. It refuses to
orphan children unless you pass `--force`, and warns about any links left
dangling:

```console
$ colophon rm topics/zig.md
deleted topics/zig.md
```

---

## 8. Check integrity

`check` walks from the root and reports problems: broken links, case
mismatches, duplicate containment, a child missing its `part_of` inverse, and
dangling IDs. It exits non-zero when it finds anything, so it fits in CI.

```console
$ colophon check
index.md: child topics/rust.md does not declare part_of back to it
1 finding(s)
```

`--fix` walks the *fixable* findings interactively and applies safe,
metadata-only repairs (today: the missing inverse). It never edits body prose —
so code that merely looks like a link is never "repaired."

```console
$ colophon check --fix
⚑  index.md: child topics/rust.md does not declare part_of back to it
   → declare part_of → index.md in topics/rust.md
   apply? [y]es / [n]o / [a]ll / [q]uit: y
applied 1 fix(es); 0 finding(s) need attention
```

Broken *body* wikilinks are reported but not auto-fixed. Note that a body
wikilink like `[[index.md]]` resolves **relative to the file it's in** — from
`topics/rust.md` that means `topics/index.md`. Write `[[/index.md]]` (from the
root) or `[[../index.md]]` (relative) to point at the real root.

---

## 9. Stable IDs (optional)

Paths change; sometimes you want a link that *doesn't* break on a move. colophon
can mint a stable ID for a document and resolve it back to a path — the "the app
owns your links" trick, except the identity data is a plain file in your own tree.

Two independent settings control this (§10):

- **`identity`** — *when* a document earns a stable ID: `off` (never), `lazy`
  (on a link-by-id or publish — the recommended default), or `eager` (every
  document at creation).
- **`id_links`** — *whether colophon authors structural links by ID* rather than
  by path. Only meaningful when `identity` isn't off; with it on, a move rewrites
  no links at all (the registry tracks the new path). The `init` **Links between
  documents** prompt sets this.

Even with `id_links` off, `lazy` identity means you can mint an ID on demand and
paste a durable reference by hand. Turn identity on at `init` (`--identity lazy`,
optionally `--links id`), or later with `colophon config identity lazy`:

```console
$ colophon config identity lazy
set identity = lazy in colophon.yaml

$ colophon id topics/rust-lang.md
initialized registry.yaml (linked from index.md)
colophon:ydbqj4g

$ colophon mv topics/rust-lang.md notes/rust.md
$ colophon resolve colophon:ydbqj4g
notes/rust.md          # the ID still points at the file after the move
```

The first `id` bootstraps a `registry` document (`registry.yaml`, or
`.json`/`.figl` matching your metadata format) beside the root and links it from
the root's metadata via the `registry` relation — so the identity state is
*reachable*, discovered by following links like everything else, not hidden in a
dotfolder. Deleting a document *tombstones* its ID (it stops resolving but is
never reissued), so a stale `colophon:` reference stays diagnosable.

With `identity: off`, `colophon id` politely refuses — there is nothing to mint.

---

## 10. Workspace config

Settings live in a config document linked from the root via the `config`
relation — same reachability move as the registry. `init` writes this document
(`colophon.yaml`) with the preferences you chose; afterwards `colophon config`
reads and writes it.

```console
$ colophon config                 # print the effective settings
link_format: markdown_root
identity: lazy
id_links: false
embed_format: yaml
content_format: markdown

$ colophon config id_links true   # change one setting
set id_links = true in colophon.yaml
```

The knobs:

| Key             | Values                                                       | Meaning                                              |
| --------------- | ------------------------------------------------------------ | ---------------------------------------------------- |
| `link_format`   | `markdown_root`, `markdown_relative`, `plain_relative`, `plain_canonical` | how colophon writes path links     |
| `identity`      | `off`, `lazy`, `eager`                                        | when a document earns a stable ID                    |
| `id_links`      | `true`/`false`                                               | author structural links *by ID* instead of by path   |
| `embed_format`  | `yaml`, `json`, `fig`                                         | metadata format for newly created documents          |
| `content_format`| `markdown`, `djot`, `html`                                   | the body grammar the workspace is authored in        |

The two `init` identity prompts map straight onto these keys: **Identity** sets
`identity`, and **Links between documents** sets `id_links` (`path` → `false`,
`id` → `true`). With `identity: lazy` + `id_links: true`, structural links are by
ID and a move rewrites nothing — the registry does the work.

---

## Command reference

| Command                         | What it does                                             |
| ------------------------------- | -------------------------------------------------------- |
| `init [DIR] [flags]`            | create a workspace root (interactive; `--title/--author/--meta/--content/--yes`) |
| `new PATH --parent P`           | create a child document, linking both directions         |
| `mv FROM TO`                    | move/rename, maintaining every affected link             |
| `rm PATH [--force]`             | delete, removing the parent's entry                      |
| `tree [ROOT]`                   | print the containment tree                               |
| `check [ROOT] [--fix]`          | report (and optionally repair) integrity problems        |
| `show FILE`                     | summarize a document                                     |
| `meta / get / links / body`     | read metadata or body                                    |
| `set FILE KEY VALUE` / `unset`  | edit a metadata field, format-preserving                 |
| `render FILE`                   | render the body to HTML                                  |
| `id FILE` / `resolve ID`        | mint / look up a stable ID                               |
| `backlinks FILE`                | list inbound links                                       |
| `config [KEY [VALUE]]`          | read/write workspace settings                            |

Run `colophon <command> --help` for the full options of any command.

---

## Known limitations

colophon is young ("works for simple workspaces"). Things a beginner will hit:

- **No directory scanning yet.** colophon only sees documents *reachable from
  the root* by following `contents`. A `.md` file you never link into the tree
  is invisible to `tree` and `check`. Always attach new documents with `new`
  (or a hand-written `part_of`).
- **`mv` doesn't yet honor `link_format`.** A move currently rewrites the
  parent's link as a *relative* path even when your `link_format` is
  `markdown_root`. The link still resolves; only its style changes. (`new` and
  `check --fix` do respect the style.)
- **The root must be unambiguous.** If a directory has two `.md` files with
  metadata and no `part_of`, colophon can't tell which is the root and reports
  an ambiguity. Keep a single root per workspace (name it `index.md`).
- **One vocabulary for now.** The CLI uses the built-in diaryx relation set
  (`contents`/`part_of`/`links`/…). Custom vocabularies exist in the library but
  aren't yet exposed as a CLI flag.

For where the project is headed, see [DESIGN.md](DESIGN.md) and
[next-steps.md](next-steps.md).
