```fig
title = prov
author = adammharris
created = 2026-07-06
contents = [
  [Design](docs/DESIGN.md),
  [Getting Started](docs/getting-started.md),
  [Config Vocab](/docs/config-vocab.md),
  [Init Adoption](/docs/init-adoption.md),
  [Next Steps](/docs/next-steps.md),
  [Reference Styles](/docs/reference-styles.md)
]
prov
> spec = 1
> content_format = markdown
> id_storage = both
> updated = ''
> identity = lazy
> fixity = attachments
> recycle_bin = true
> metadata = { format = fig, embed = code_block }
> references = { notation = markdown, path_style = root, target = path, label = false }
```

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

## Status

Works for simple workspaces.

Working toward 1.0 now that Twig (Zig dependency) has reached 1.0.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.