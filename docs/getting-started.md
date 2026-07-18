```fig
title = Getting Started with colophon
part_of = [colophon](/README.md)
```

# Getting Started with colophon

A beginner's guide to the `colophon` command line. By the end you'll have a small
workspace, understand how its structure is stored, and know every command you
need for day-to-day use.

> **What colophon is, in one sentence.** A *self-describing plaintext
> workspace*: a set of documents whose structure lives in the documents' own
> frontmatter, not in the folder layout or an app-private sidecar. Follow the
> links from a root document and the whole workspace unfolds. See
> [DESIGN.md](DESIGN.md) for the reasoning behind that idea.

> **The transcripts below are executable.** Every command block marked as part of
> the walkthrough is run in CI, in order, against a real workspace
> (`ci/check-getting-started.sh`). If the CLI changes so a command in this guide
> stops working, the build fails — so what you read here is what the current
> binary does. (Random IDs and absolute paths in the output will differ on your
> machine.)

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
  | `registry` | root → registry  | where stable IDs are recorded              |
  | `config`   | root → config    | where workspace settings live              |

- **The spanning tree.** Exactly one relation is the *spanning* relation —
  `contents`/`part_of` here. It is single-parent, and it is the workspace's
  discovery spine: every document has one path back to one **root**. Every other
  relation (like `links`) may be many-to-many, laid over the tree as a graph.

The root is just a document that nothing contains — it has no `part_of`.
colophon finds it by walking up from your current directory until it sees a
document with metadata and no `part_of` (an `index.md` or `README.md` wins
ties).

---

## 2. Install

colophon builds from source and needs two toolchains:

- **Rust** (`cargo`, 1.85 or newer) — to build colophon itself.
- **Zig** (0.16.0) — colophon's metadata parser (`fig`) and body parser
  (`twig-doc`) are Zig-backed and compile during the build. Both are ordinary
  crates.io dependencies, so there is nothing to clone alongside colophon.

```console
$ git clone https://github.com/adammharris/colophon
$ cd colophon
$ cargo build --release
```

The binary lands at `target/release/colophon`. Put it on your `PATH`, or invoke
it by full path. Every example below uses the command name `colophon`.

---

## 3. Create a workspace

`init` sets up a workspace: a self-describing root document plus a config
document that records your preferences. On a terminal it walks you through a
series of choices:

```console
$ colophon init my-vault
┌  colophon init
│
◇  Title ················ My Vault
◇  Author ··············· (blank)
◇  Content format ······· Markdown
◇  Embed type ··········· Character delimiters
◇  Config language ······ YAML
◇  Wrapper ·············· Markdown
◇  Identity ············· On demand
◇  References between documents ··· By path
◇  Path format ·········· Workspace-absolute
◇  Where IDs are stored ·· In each file (+ registry)
◇  Content checksums ····· Attachments
│
└  initialized /home/you/my-vault
```

Each prompt has a flag, so you can skip the interview entirely. Pass `--yes`
(`-y`) to take every default:

<!-- exec -->
```console
$ colophon init my-vault --yes
initialized /home/you/my-vault
  root: index.md — My Vault
  config: colophon.yaml — content markdown, embed delimited (character delimiters), language yaml, identity lazy, references path, markdown notation, root paths, id storage both, recycle bin, fixity attachments
next: colophon new <title> --in index.md
```

The prompts, in the order they're asked:

| Prompt                        | Flag           | Default                       | Options                                                       |
| ----------------------------- | -------------- | ----------------------------- | ------------------------------------------------------------- |
| **Title**                     | `--title`      | the directory's name          | any text                                                      |
| **Author**                    | `--author`     | omitted                       | any text                                                      |
| **Content format**            | `--content`    | `markdown`                    | `markdown` (`.md`), `djot` (`.dj`), `html` (`.html`)          |
| **Embed type**                | `--embed`      | the content's first style     | `delimited`, `code-block`, `html-script`, `html-code`, `separate` — narrowed by content format |
| **Config language**           | `--meta`       | `yaml`                        | `yaml`, `json`, `toml`, `fig` — narrowed by embed type        |
| **Wrapper**                   | `--wrapper`    | `markdown`                    | `markdown` (`[Title](target)`), `wikilink` (`[[target]]`)     |
| **Identity**                  | `--identity`   | `lazy`                        | `off` (a.k.a. `none`), `lazy`, `eager` — see [§9](#9-stable-ids-optional) |
| **References between docs**   | `--reference`  | `path`                        | `path`, `id`, `alias`, `split` — `id`/`split` need identity   |
| **Path format**               | `--link-style` | `markdown-root`               | `markdown-root`, `markdown-relative`, `plain-relative`, `plain-canonical` (only when references are by path) |
| **Where IDs are stored**      | `--id-storage` | `frontmatter`                 | `registry`, `frontmatter` — only when identity is on          |
| **Content checksums**         | `--fixity`     | `payloads`                    | `off`, `payloads` (attachments), `full` (also bodies)         |

The root-shaping choices come first; the rest are **workspace preferences**, all
written into a config document (`colophon.yaml`, linked from the root) so the
workspace records how it wants to be authored — see [§10](#10-workspace-config).
The **content format** sets the root file's extension and body grammar. The
**embed type** picks the carrier the config language is written in — frontmatter
delimiters, a fenced code block, an HTML data island, or a separate sidecar — and
gates which config languages fit (bare delimiters don't suit `fig`).

Setting some flags and being prompted for the rest works too. `--reference id`
needs identity, so it's rejected with `--identity off`:

```console
$ colophon init my-vault --content djot --reference id --yes
initialized /home/you/my-vault
  root: index.dj — My Vault
  config: colophon.yaml — content djot, embed code_block (typed code block), language yaml, identity lazy, references id, id storage both, recycle bin, fixity attachments
next: colophon new <title> --in index.dj
```

With no directory argument, `init` initializes the current directory. It refuses
to run where a workspace root already exists, so re-running it by mistake is
safe. Look at the root it wrote:

<!-- exec -->
```console
$ cd my-vault
$ cat index.md
---
title: My Vault
config: colophon.yaml
---

# My Vault
```

---

## 4. Grow the tree with `new`

`new` takes the new document's **title** as its positional argument and the
parent to hang it under as `--in` (`-i`). It derives a readable filename from the
title (a slug plus the content extension) and wires up *both* directions of the
spanning link — the parent gains a `contents` entry, the child gets a `part_of`
back.

<!-- exec -->
```console
$ colophon new "Rust" --in index.md
created rust.md (in index.md)
$ colophon new "Zig" --in index.md
created zig.md (in index.md)
```

Override the derived filename with `--as <path>` (an exact path) or just its
extension with `--ext`. Look at what `new` wrote:

<!-- exec -->
```console
$ cat index.md
---
title: My Vault
config: colophon.yaml
contents:
- '[Rust](/rust.md)'
- '[Zig](/zig.md)'
---

# My Vault
$ cat rust.md
---
title: Rust
part_of: '[My Vault](/index.md)'
---
```

The links are ordinary Markdown links written into the metadata. Nothing about
the structure lives in the filesystem — move these files to another machine and
they still describe the same tree.

---

## 5. See the workspace

`tree` prints the containment tree, discovered by following `contents` from the
root:

<!-- exec -->
```console
$ colophon tree
index.md — My Vault
├── rust.md — Rust
└── zig.md — Zig
```

`show` summarizes one document — its title, spanning children, and overlay
links:

<!-- exec -->
```console
$ colophon show index.md
index.md
  title: My Vault
  contents (2 children):
    - [Rust](/rust.md)
    - [Zig](/zig.md)
  config:
    - colophon.yaml
```

More single-document readers:

| Command                    | Prints                                             |
| -------------------------- | -------------------------------------------------- |
| `colophon meta FILE`       | the raw metadata block (no fences)                 |
| `colophon get FILE KEY`    | one field by dotted path (`title`, `contents.0`)   |
| `colophon links FILE`      | every link as `relation⇥target`                    |
| `colophon body FILE`       | everything *outside* the metadata block            |
| `colophon backlinks FILE`  | who links *to* this document, across the workspace |

<!-- exec -->
```console
$ colophon backlinks index.md
rust.md	part_of	path
zig.md	part_of	path
```

---

## 6. Edit metadata

`set` and `unset` change a field while preserving the file's formatting,
comments, and metadata format. `set` even creates the block if a document has
none.

<!-- exec -->
```console
$ colophon set rust.md summary "Notes on the Rust language"
$ colophon get rust.md summary
Notes on the Rust language
$ colophon unset rust.md summary
```

Values are typed by inference: `true`/`false`, integers, floats, and `null`
become those types; everything else is a string. Dotted keys address nested
fields and sequence indices (`contents.0`).

### Body prose and `render`

The *body* is everything after the frontmatter. colophon can render a
Markdown/Djot body to HTML, and it understands code — a `[[…]]` inside a code
span is treated as code, never as a link:

<!-- exec -->
```console
$ printf '\n# Rust\n\nInline `let x = [[1,2],[3,4]];` is code, not a link.\n' >> rust.md
$ colophon render rust.md
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

<!-- exec -->
```console
$ colophon mv rust.md rust-lang.md
moved rust.md -> rust-lang.md
$ colophon tree
index.md — My Vault
├── rust-lang.md — Rust
└── zig.md — Zig
```

`rm` removes a document's parent entry and, by default, moves the file to the
workspace **recycle bin** (recoverable with `colophon restore`). Pass `--purge`
for an immediate hard delete. It refuses to orphan children unless you pass
`--force`, and warns about any links left dangling:

<!-- exec -->
```console
$ colophon rm zig.md
moved zig.md to the recycle bin (restore with `colophon restore`)
```

---

## 8. Check integrity

`check` walks from the root and reports problems: broken links, case mismatches,
duplicate containment, a child missing its `part_of` inverse, dangling IDs, and
documents on disk that nothing links to (orphans). It exits non-zero when it
finds anything, so it fits in CI. Right now the workspace is consistent:

<!-- exec -->
```console
$ colophon check
ok: no findings
```

Break the inverse on purpose to see a finding — and `--fix`, which walks the
*fixable* findings interactively and applies safe, metadata-only repairs (today:
the missing inverse, and adopting an orphan). It never edits body prose, so code
that merely looks like a link is never "repaired":

<!-- exec allow-fail -->
```console
$ colophon unset rust-lang.md part_of
$ colophon check
index.md: child rust-lang.md does not declare part_of back to it
1 finding(s)
$ printf 'y\n' | colophon check --fix
⚑  index.md: child rust-lang.md does not declare part_of back to it
   → declare part_of → index.md in rust-lang.md
   apply? [y]es / [n]o / [a]ll / [q]uit: applied 1 fix(es); 0 finding(s) need attention
```

<!-- exec -->
```console
$ colophon check
ok: no findings
```

Broken *body* wikilinks are reported but not auto-fixed. Note that a body
wikilink like `[[index.md]]` resolves **relative to the file it's in** — from
`sub/rust.md` that means `sub/index.md`. Write `[[/index.md]]` (from the root) or
`[[../index.md]]` (relative) to point at the real root.

---

## 9. Stable IDs (optional)

Paths change; sometimes you want a link that *doesn't* break on a move. colophon
can mint a stable ID for a document and resolve it back to a path — the "the app
owns your links" trick, except the identity data is a plain file in your own tree.

Two independent settings control this (§10):

- **`identity`** — *when* a document earns a stable ID: `none`/`off` (never),
  `lazy` (on a link-by-id or publish — the recommended default), or `eager`
  (every document at creation).
- **`references.target`** — *what a reference addresses*: `path`, `id`, or
  `alias`. Set it to `id` and colophon authors structural links *by ID*, so a
  move rewrites no links at all (the registry tracks the new path). Only
  meaningful when `identity` isn't off. The `init` **References between
  documents** prompt sets this.

Even with `references.target: path`, `lazy` identity (the default) means you can
mint an ID on demand and paste a durable reference by hand:

<!-- exec -->
```console
$ colophon config identity lazy
set identity = lazy in colophon.yaml
$ colophon id rust-lang.md
initialized registry.yaml (linked from index.md)
id:s5jpwxz
```

The ID survives a move — the registry follows the file:

<!-- exec -->
```console
$ id=$(colophon id rust-lang.md)
$ colophon mv rust-lang.md notes/rust.md
moved rust-lang.md -> notes/rust.md
$ colophon resolve "$id"
notes/rust.md
```

The first `id` bootstraps a `registry` document (`registry.yaml`, or
`.json`/`.figl` matching your metadata format) beside the root and links it from
the root's metadata via the `registry` relation — so the identity state is
*reachable*, discovered by following links like everything else, not hidden in a
dotfolder. IDs are written `id:<id>`; deleting a document *tombstones* its ID (it
stops resolving but is never reissued), so a stale `id:` reference stays
diagnosable.

With `identity: off`, `colophon id` politely refuses — there is nothing to mint.

---

## 10. Workspace config

Settings live in a config document linked from the root via the `config`
relation — same reachability move as the registry. `init` writes this document
(`colophon.yaml`) with the preferences you chose; afterwards `colophon config`
reads and writes it. Keys are grouped into a small nested vocabulary
(`docs/config-vocab.md`); a policy setting can also live in the root's
`colophon:` frontmatter block. `colophon check` flags any key colophon would
silently ignore (a typo, or an unrecognized value).

<!-- exec -->
```console
$ colophon config
spec: 1
content_format: markdown
metadata:
  format: yaml
  embed: delimited
references:
  notation: markdown
  path_style: root
  target: path
  label: false
id_storage: both
updated: ''
identity: lazy
fixity: attachments
recycle_bin: true
$ colophon config references.target id
set references.target = id in colophon.yaml
```

The knobs (dotted keys address nested axes):

| Key                       | Values                                                          | Meaning                                          |
| ------------------------- | -------------------------------------------------------------- | ------------------------------------------------ |
| `references.notation`     | `markdown`, `wikilink`, `bare`                                 | the syntactic form links are written in          |
| `references.path_style`   | `root`, `relative`, `canonical`                                | how a *path* target is resolved                  |
| `references.target`       | `path`, `id`, `alias`                                           | what a reference addresses                        |
| `references.label`        | `true`/`false`                                                 | whether an id/alias link carries a `\|Title`      |
| `identity`                | `none` (or `off`), `lazy`, `eager`                             | when a document earns a stable ID                |
| `id_storage`              | `registry`, `frontmatter`, `both`                              | where a stable ID lives                          |
| `metadata.format`         | `yaml`, `json`, `toml`, `fig`                                  | config language for newly created documents      |
| `metadata.embed`          | `delimited`, `code_block`, `html_script`, `html_code`, `separate` | how that config language is embedded          |
| `content_format`          | `markdown`, `djot`, `html`                                     | the body grammar the workspace is authored in    |
| `fixity`                  | `off`, `attachments`, `all`                                    | how far content-checksum coverage extends        |
| `recycle_bin`             | `true`/`false`                                                 | route a delete to the recoverable bin            |
| `updated`                 | *a field name*                                                 | the machine-maintained "last updated" field      |

The two `init` identity prompts map onto these keys: **Identity** sets
`identity`, and **References between documents** sets `references.target`. With
`identity: lazy` + `references.target: id`, structural links are by ID and a move
rewrites nothing — the registry does the work.

**Making config explicit.** Every key has a default, so a workspace with a
minimal (or no) config document still runs — it just relies on those defaults. If
you would rather see and edit every setting, `colophon config --setup` writes the
full effective config into `colophon.yaml` (creating and linking it if needed),
filling in the keys you have not set while preserving the ones you have:

<!-- exec -->
```console
$ colophon config --setup
wrote 9 explicit setting(s) to colophon.yaml
```

**Config that won't take effect.** colophon reads config back by exact key and
value, so a misspelled key or an unrecognized value is silently ignored (the
default stands). `colophon check` reports each one; and any command that opens the
workspace prints a one-line reminder if your config has such a setting — or a
`spec` newer than your colophon understands. Set `COLOPHON_QUIET=1` to silence
these reminders.

---

## Command reference

| Command                         | What it does                                             |
| ------------------------------- | -------------------------------------------------------- |
| `init [DIR] [flags]`            | create a workspace root (interactive; every prompt has a flag) |
| `new TITLE --in P`              | create a child document, linking both directions         |
| `mv FROM TO [--in P]`           | move/rename, maintaining every affected link             |
| `reparent PATH --in P`          | change a document's parent, leaving the file put         |
| `rm PATH [--force] [--purge]`   | delete (to recycle bin by default), removing the parent's entry |
| `restore PATH` / `empty-bin`    | recover a binned document / purge the bin                |
| `attach FILE [--in P]`          | give a non-document file a metadata sidecar, linked in    |
| `tree [ROOT]`                   | print the containment tree                               |
| `explore [FILE]`                | walk the graph interactively                             |
| `check [ROOT] [--fix]`          | report (and optionally repair) integrity problems        |
| `show FILE`                     | summarize a document                                     |
| `meta / get / links / body`     | read metadata or body                                    |
| `set FILE KEY VALUE` / `unset`  | edit a metadata field, format-preserving                 |
| `edit FILE`                     | open in `$EDITOR`, restamping fixity/`updated` on save    |
| `render FILE`                   | render the body to HTML                                  |
| `duplicate FILE`                | copy a document as a fresh sibling                       |
| `convert FILE AXIS VALUE`       | re-spell a document's links (`notation` / `path_style`)  |
| `id FILE` / `resolve ID`        | mint / look up a stable ID                               |
| `backlinks FILE`                | list inbound links                                       |
| `config [KEY [VALUE]]`          | read/write workspace settings                            |

Run `colophon <command> --help` for the full options of any command.

---

## Known limitations

colophon is young. Things a beginner will hit:

- **`mv` doesn't yet honor the reference style.** A move currently rewrites the
  parent's link as a *relative* path even when your `references.path_style` is
  `root`. The link still resolves; only its style changes. (`new` and
  `check --fix` do respect the style.)
- **The root must be unambiguous.** If a directory has two documents with
  metadata and no `part_of`, colophon can't tell which is the root and reports
  an ambiguity. Keep a single root per workspace (name it `index.md`).
- **One vocabulary for now.** The CLI uses the built-in diaryx relation set
  (`contents`/`part_of`/`links`/…). Custom vocabularies exist in the library but
  aren't yet exposed as a CLI flag.

For where the project is headed, see [DESIGN.md](DESIGN.md) and
[next-steps.md](next-steps.md).
