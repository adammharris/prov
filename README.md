```fig
title = colophon
author = adammharris
created = 2026-07-06
contents = [[Design](docs/DESIGN.md), [Getting Started](docs/getting-started.md)]
```

# colophon

A *self-describing plaintext workspace*: a set of documents whose structure lives in the documents' own embedded metadata (frontmatter), not in the filesystem layout or an app-private sidecar folder.

The name is the point. A *colophon* is the note in which a book describes its own making — the type, the paper, the press. A colophon workspace is one you can hand to any tool and it explains itself: follow the links in the metadata and the whole structure unfolds, with a distinguished root that describes the whole.

## Layout

- **`colophon/`** — the library. Documents, relations, identity, and the workspace seam.
- **`colophon-cli/`** — a thin command-line companion (the installed binary is `colophon`).

## Filesystem

colophon is generic over the small async [`colophon::Storage`](colophon/src/fs.rs) trait, which mirrors the slice of `std::fs` the scan/traverse/mutate engine needs. Implement it over `std::fs`, `tokio::fs`, or a browser filesystem (OPFS/IndexedDB) — the workspace never learns which.

## Status

Works for simple workspaces.

Working toward 1.0 now that Twig (Zig dependency) has reached 1.0.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.