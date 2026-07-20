---
title: prov
author: adammharris
created: 2026-07-06
contents:
- '[Design](docs/DESIGN.md)'
- '[Spec](/docs/spec.md)'
- '[Getting Started](docs/getting-started.md)'
- '[Config Vocab](/docs/config-vocab.md)'
- '[Init Adoption](/docs/init-adoption.md)'
- '[Next Steps](/docs/next-steps.md)'
- '[Reference Styles](/docs/reference-styles.md)'
config: prov.yaml
---

# prov

[![CI](https://img.shields.io/github/actions/workflow/status/diaryx-org/prov/ci.yml?branch=main)](https://github.com/diaryx-org/prov/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/prov.svg)](https://crates.io/crates/prov)
[![docs.rs](https://img.shields.io/docsrs/prov)](https://docs.rs/prov)
[![license](https://img.shields.io/crates/l/prov.svg)](#license)

A *self-describing plaintext workspace*: a set of documents whose structure lives in the documents' own embedded metadata (frontmatter), not in the filesystem layout or an app-private sidecar folder.

The name says what it is: **prov** — *Plaintext Records, Organized & Verifiable* — and, not by accident, the usual short form of *provenance*. A prov workspace is one you can hand to any tool and it explains itself: follow the links in the metadata and the whole structure unfolds, with a distinguished root that describes the whole.

## Layout

- **`prov/`** — the library. Documents, relations, identity, and the workspace seam.
- **`prov-cli/`** — a thin command-line companion (the installed binary is `prov`).

## Filesystem

prov is generic over the small async [`prov::Storage`](prov/src/fs.rs) trait, which mirrors the slice of `std::fs` the scan/traverse/mutate engine needs. Implement it over `std::fs`, `tokio::fs`, or a browser filesystem (OPFS/IndexedDB) — the workspace never learns which.

## Output conventions

The `prov` CLI keeps its two output streams cleanly separated, so it composes:

- **stdout** — the *machine value*: the identifier(s) of the object the command produced or read, one per line, undecorated. Empty when there is genuinely no result (`empty-bin`, a `--dry-run`).
- **stderr** — the *human narration*: `created …`, `moved …`, warnings, previews, `ok: no findings`.
- **exit code** — success or failure.

So `2>/dev/null` silences the chatter without eating data, and `$(prov new 'Title')` captures a bare path you can pipe or open. The contract is the **result, not the action**: an idempotent `new -p` that finds the document already there still prints its path, while a `--dry-run` prints nothing to stdout (nothing was created).

| Command | stdout |
| --- | --- |
| `init` | the root document's path |
| `new` | the created node's path (idempotent no-op included) |
| `attach` | each sidecar node path, one per line |
| `mv` | the destination path |
| `reparent` | the document's path |
| `duplicate` | the copy's path |
| `edit`, `set`, `unset` | the edited document's path |
| `restore` | the restored document's path |
| `convert` | each rewritten document's path, one per line |
| `empty-bin` | *(nothing — a bulk purge names no object)* |
| `config <key> <value>` | the value now in effect |
| `meta`, `get`, `body`, `render`, `links`, `tree`, `backlinks`, `id`, `resolve`, `config`, `check` | the requested data (findings, values, edges) |

## Status

Works for simple workspaces.

Working toward 1.0 now that Twig (Zig dependency) has reached 1.0.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.