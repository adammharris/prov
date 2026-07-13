//! Documents — a plaintext file with an embedded metadata block and a body,
//! or a config file whose *entire content* is the metadata.
//!
//! The two shapes are one model: a config file is simply a document whose
//! metadata carrier is the whole file and whose body is empty. Both parse to
//! the same [`Document`], link through the same relations, and participate in
//! traversal, validation, and mutation identically — which is what lets a
//! workspace mix prose documents and config documents in one tree.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::meta::{self, Value};

/// The embed archetype a fenced metadata block was found in. Re-exported from
/// `fig`, which owns both detection (`fig::detect`) and the fence/format
/// coupling ([`EmbedType::inner_format`]).
pub use fig::EmbedType;

/// Where a document's metadata physically lives — recorded at parse time so a
/// write can preserve the original carrier exactly (a ```` ```fig ```` block is
/// never rewritten as `---` YAML; a bare `.yaml` file never grows fences).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaCarrier {
    /// A fenced block inside a host file (`---` YAML, `;;;` JSON,
    /// ```` ```fig ````, endmatter), with the prose body around it.
    Fenced(EmbedType),
    /// The entire file is the metadata (a config document); the body is empty.
    /// The format comes from the file extension.
    WholeFile(fig::Format),
}

impl MetaCarrier {
    /// The format the metadata is written in.
    pub fn format(&self) -> fig::Format {
        match self {
            MetaCarrier::Fenced(kind) => kind.inner_format(),
            MetaCarrier::WholeFile(format) => *format,
        }
    }
}

/// The whole-file metadata format implied by `path`'s extension, if any.
/// These are the extensions colophon treats as config documents.
///
/// Each extension is recognized only when its format feature is compiled in: a
/// `.json` file is a config document under the `json` feature, and an ordinary
/// (metadata-less) prose document without it. This keeps colophon from claiming
/// to read a format whose parser was left out of the build.
pub fn whole_file_format(path: &Path) -> Option<fig::Format> {
    match path.extension()?.to_str()? {
        #[cfg(feature = "yaml")]
        "yaml" | "yml" => Some(fig::Format::Yaml),
        #[cfg(feature = "json")]
        "json" => Some(fig::Format::Json),
        #[cfg(feature = "toml")]
        "toml" => Some(fig::Format::Toml),
        #[cfg(feature = "fig-lang")]
        "fig" | "figl" => Some(fig::Format::Fig),
        _ => None,
    }
}

/// Whether colophon can read `path` as text — a recognized body format
/// (Markdown/Djot/HTML) or a whole-file metadata format (YAML/JSON/…). Its
/// negation is an **opaque payload**: a file colophon treats as bytes (an image,
/// a PDF, a font, any binary) and never parses. An *attachment* is exactly a
/// whole-file metadata sidecar whose `content` points at such a payload, which
/// is how an arbitrary file gains workspace-linked metadata without being able
/// to carry frontmatter itself.
pub fn is_opaque_payload(path: &Path) -> bool {
    crate::content::ContentFormat::from_extension(path).is_none() && whole_file_format(path).is_none()
}

/// The canonical whole-file extension for a metadata `format` — the inverse of
/// [`whole_file_format`]. Used when materializing a whole-file metadata document
/// (a config/registry sidecar, or the metadata half of a *separated* document):
/// `yaml`, `json`, `figl`. A format whose feature is not compiled falls back to
/// `yaml` (the always-present default).
pub fn whole_file_extension(format: fig::Format) -> &'static str {
    match format {
        #[cfg(feature = "json")]
        fig::Format::Json => "json",
        #[cfg(feature = "toml")]
        fig::Format::Toml => "toml",
        #[cfg(feature = "fig-lang")]
        fig::Format::Fig => "figl",
        _ => "yaml",
    }
}

/// The fenced-frontmatter carrier for `format` — the archetype a new document
/// gets when it inherits no parent block and the workspace default is `format`.
/// A format whose feature is not compiled falls back to YAML frontmatter (which
/// the default `yaml` feature always provides).
pub fn frontmatter_carrier(format: fig::Format) -> MetaCarrier {
    let embed = match format {
        #[cfg(feature = "json")]
        fig::Format::Json => EmbedType::FrontmatterJson,
        #[cfg(feature = "toml")]
        fig::Format::Toml => EmbedType::PlusToml,
        #[cfg(feature = "fig-lang")]
        fig::Format::Fig => EmbedType::FrontmatterFig,
        _ => EmbedType::FrontmatterYaml,
    };
    MetaCarrier::Fenced(embed)
}

/// The archetype *family* a workspace authors embedded metadata in — the
/// "embed type" the CLI's `init` prompts for, one level above the concrete
/// [`EmbedType`]. A family plus a metadata [`fig::Format`] resolves to a
/// carrier through [`embed_carrier`]: e.g. (`CodeBlock`, YAML) is a
/// ```` ```yaml ```` block, (`Delimited`, TOML) is a `+++` block, and
/// (`Separate`, JSON) is a whole-file `.json` sidecar. It is what the config
/// document records (via [`WorkspaceConfig`](crate::WorkspaceConfig)) so a
/// workspace stays self-describing about *how* its metadata is embedded, not
/// just which format it is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedStyle {
    /// Character-delimited frontmatter: `---` YAML, `+++` TOML, `;;;` JSON. A
    /// Markdown convention; the fig dialect has no delimiter form.
    Delimited,
    /// A typed fenced code block — ```` ```yaml ````, ```` ```toml ````,
    /// ```` ```json ````, ```` ```fig ```` — that renders as a visible block in
    /// Markdown or Djot.
    CodeBlock,
    /// An HTML `<script type="application/…">` data island (not rendered).
    HtmlScript,
    /// An HTML `<pre><code class="language-…">` visible code block.
    HtmlCode,
    /// Metadata kept in a sibling *whole-file* document, joined to a plain body
    /// file by a `content` attribute — neither file carries fences.
    Separate,
}

impl EmbedStyle {
    /// The `embed_type` config-document spelling for this style.
    pub fn as_config_str(self) -> &'static str {
        match self {
            EmbedStyle::Delimited => "delimited",
            EmbedStyle::CodeBlock => "code_block",
            EmbedStyle::HtmlScript => "html_script",
            EmbedStyle::HtmlCode => "html_code",
            EmbedStyle::Separate => "separate",
        }
    }

    /// Parse an `embed_type` config value. Unknown → `None` (keep the default).
    pub fn from_config_str(value: &str) -> Option<Self> {
        Some(match value {
            "delimited" => EmbedStyle::Delimited,
            "code_block" => EmbedStyle::CodeBlock,
            "html_script" => EmbedStyle::HtmlScript,
            "html_code" => EmbedStyle::HtmlCode,
            "separate" => EmbedStyle::Separate,
            _ => return None,
        })
    }
}

/// Resolve an [`EmbedStyle`] + metadata `format` to the carrier a new document
/// should get. `Separate` maps to a whole-file sidecar in `format`; every other
/// style maps to the concrete [`EmbedType`] for that `(style, format)` pair.
/// `None` for a combination that has no archetype — notably `Delimited` + fig
/// (the dialect has no `---`-style delimiter) and any fenced style paired with a
/// format fig cannot fence (`Zon`).
pub fn embed_carrier(style: EmbedStyle, format: fig::Format) -> Option<MetaCarrier> {
    use EmbedType as E;
    use fig::Format as F;
    // JSON's three dialects share one fenced/frontmatter archetype.
    let is_json = matches!(format, F::Json | F::Jsonc | F::Json5);
    let kind = match style {
        EmbedStyle::Separate => return Some(MetaCarrier::WholeFile(format)),
        EmbedStyle::Delimited => match format {
            F::Yaml => E::FrontmatterYaml,
            F::Toml => E::PlusToml,
            _ if is_json => E::FrontmatterJson,
            _ => return None,
        },
        EmbedStyle::CodeBlock => match format {
            F::Yaml => E::FencedYaml,
            F::Toml => E::FencedToml,
            F::Fig => E::FrontmatterFig,
            _ if is_json => E::FencedJson,
            _ => return None,
        },
        EmbedStyle::HtmlScript => match format {
            F::Yaml => E::HtmlScriptYaml,
            F::Toml => E::HtmlScriptToml,
            F::Fig => E::HtmlScriptFig,
            _ if is_json => E::HtmlScriptJson,
            _ => return None,
        },
        EmbedStyle::HtmlCode => match format {
            F::Yaml => E::HtmlCodeYaml,
            F::Toml => E::HtmlCodeToml,
            F::Fig => E::HtmlCodeFig,
            _ if is_json => E::HtmlCodeJson,
            _ => return None,
        },
    };
    Some(MetaCarrier::Fenced(kind))
}

/// A parsed document: its path, its embedded metadata, and its body text.
///
/// Metadata is stored as a dynamic [`Value`] (a mapping, or [`Value::Null`] when
/// the document has no frontmatter) because link fields are configurable and
/// therefore accessed dynamically.
#[derive(Debug, Clone)]
pub struct Document {
    /// Path this document was read from (workspace-relative or absolute — the
    /// caller decides; colophon does not interpret it here).
    pub path: PathBuf,
    /// Parsed embedded metadata.
    pub meta: Value,
    /// Everything outside the metadata block (the host prose). Empty for a
    /// config document.
    pub body: String,
    /// Where the metadata was found, or `None` when the document has no
    /// (well-formed) metadata. Preserved on write.
    pub carrier: Option<MetaCarrier>,
}

impl Document {
    /// Parse a document from its full text.
    ///
    /// If `path` has a config extension (`.yaml`, `.yml`, `.json`, `.fig`,
    /// `.figl`), the entire text is the metadata and the body is empty.
    /// Otherwise the embedded metadata block is auto-detected via
    /// `fig::detect` — any archetype fig knows (`---` YAML, `;;;` JSON,
    /// ```` ```fig ````, ```` ```endmatter ````) — and parsed in that
    /// archetype's inner format. If there is no (well-formed) block, `meta`
    /// is [`Value::Null`] and the whole text is the body. An unterminated
    /// opening fence is treated as no metadata — we do not guess where it
    /// ends.
    pub fn parse(path: impl Into<PathBuf>, text: &str) -> Result<Self> {
        let path = path.into();
        if let Some(format) = whole_file_format(&path) {
            let meta = meta::parse_value(text, format)?;
            return Ok(Self {
                path,
                meta,
                body: String::new(),
                carrier: Some(MetaCarrier::WholeFile(format)),
            });
        }
        let (meta, body, carrier) = match fig::detect(text) {
            Some(kind) => match fig::split(text, kind) {
                Some((content, body)) => (
                    meta::parse_value(content, kind.inner_format())?,
                    body.to_owned(),
                    Some(MetaCarrier::Fenced(kind)),
                ),
                // Detected by its open delimiter but with no matching close:
                // recognized-but-malformed degrades to "no metadata".
                None => (Value::Null, text.to_owned(), None),
            },
            None => (Value::Null, text.to_owned(), None),
        };
        Ok(Self { path, meta, body, carrier })
    }

    /// The document's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `true` if the document declares any embedded metadata mapping.
    pub fn has_meta(&self) -> bool {
        self.meta.as_mapping().is_some()
    }

    /// The raw `content` attribute — the relative path to a *separated*
    /// document's body file — or `None` for an ordinary (combined) document
    /// whose body is [`self.body`](Document::body). A separated document is a
    /// whole-file metadata document (`.yaml`/`.json`/`.figl`) that points at its
    /// prose body in a sibling file, keeping both halves plain text and linked.
    pub fn content_attr(&self) -> Option<&str> {
        self.meta.get("content").and_then(Value::as_str)
    }

    /// `true` when this document is an **attachment sidecar**: a whole-file
    /// metadata document whose `content` points at an [opaque
    /// payload](is_opaque_payload) rather than a prose body. Recognized two ways,
    /// so a hand-written sidecar need not be verbose: an explicit `attachment:
    /// true` flag (what [`Workspace::attach`](crate::Workspace::attach) writes),
    /// **or** a `content` target whose extension colophon cannot read as text.
    ///
    /// A *separated prose* document (`content` → a `.md`/`.dj`/`.html` body) is
    /// deliberately **not** an attachment: its body is a colophon document in its
    /// own right, scanned for links and titles; an attachment's payload is bytes
    /// colophon never opens.
    pub fn is_attachment(&self) -> bool {
        match self.content_attr() {
            None => false,
            Some(content) => {
                self.meta.get("attachment").and_then(Value::as_bool) == Some(true)
                    || is_opaque_payload(Path::new(content))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "yaml")]
    #[test]
    fn parses_yaml_frontmatter_and_body() {
        let text = "---\ntitle: Root\ncontents:\n- a.md\n---\n# Body\n\nhello\n";
        let doc = Document::parse("index.md", text).unwrap();
        assert_eq!(doc.meta.get("title").and_then(Value::as_str), Some("Root"));
        assert_eq!(doc.body, "# Body\n\nhello\n");
        assert_eq!(doc.carrier, Some(MetaCarrier::Fenced(EmbedType::FrontmatterYaml)));
        assert!(doc.has_meta());
    }

    #[cfg(feature = "fig-lang")]
    #[test]
    fn parses_fig_fenced_frontmatter() {
        let text = "```fig\ntitle = colophon\ncontents = [docs/design.md]\n```\n# Body\n";
        let doc = Document::parse("README.md", text).unwrap();
        assert_eq!(
            doc.meta.get("title").and_then(Value::as_str),
            Some("colophon")
        );
        assert_eq!(doc.body, "# Body\n");
        assert_eq!(doc.carrier, Some(MetaCarrier::Fenced(EmbedType::FrontmatterFig)));
        assert!(doc.has_meta());
    }

    #[cfg(feature = "json")]
    #[test]
    fn parses_json_frontmatter() {
        let text = ";;;\n{\"title\": \"Root\"}\n;;;\nbody\n";
        let doc = Document::parse("note.md", text).unwrap();
        assert_eq!(doc.meta.get("title").and_then(Value::as_str), Some("Root"));
        assert_eq!(doc.carrier, Some(MetaCarrier::Fenced(EmbedType::FrontmatterJson)));
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn parses_yaml_endmatter() {
        let text = "# Body first\n```endmatter\ntitle: Tail\n```\n";
        let doc = Document::parse("note.md", text).unwrap();
        assert_eq!(doc.meta.get("title").and_then(Value::as_str), Some("Tail"));
        assert_eq!(doc.body, "# Body first\n");
        assert_eq!(doc.carrier, Some(MetaCarrier::Fenced(EmbedType::EndmatterYaml)));
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn a_config_file_is_a_document_whose_content_is_all_metadata() {
        let text = "title: ID registry\npart_of: index.md\nregistry:\n  abc: a.md\n";
        let doc = Document::parse("registry.yaml", text).unwrap();
        assert_eq!(doc.meta.get("title").and_then(Value::as_str), Some("ID registry"));
        assert_eq!(
            doc.meta.get("part_of").and_then(Value::as_str),
            Some("index.md")
        );
        assert_eq!(doc.body, "");
        assert_eq!(doc.carrier, Some(MetaCarrier::WholeFile(fig::Format::Yaml)));
        assert!(doc.has_meta());
    }

    #[cfg(feature = "fig-lang")]
    #[test]
    fn a_fig_config_file_parses_the_dialect() {
        let text = "title = settings\npart_of = index.md\n";
        let doc = Document::parse("settings.figl", text).unwrap();
        assert_eq!(doc.meta.get("title").and_then(Value::as_str), Some("settings"));
        assert_eq!(doc.carrier, Some(MetaCarrier::WholeFile(fig::Format::Fig)));
    }

    #[test]
    fn embed_style_config_str_round_trips() {
        for style in [
            EmbedStyle::Delimited,
            EmbedStyle::CodeBlock,
            EmbedStyle::HtmlScript,
            EmbedStyle::HtmlCode,
            EmbedStyle::Separate,
        ] {
            assert_eq!(EmbedStyle::from_config_str(style.as_config_str()), Some(style));
        }
        assert_eq!(EmbedStyle::from_config_str("nonsense"), None);
    }

    #[test]
    fn embed_carrier_resolves_style_and_format_to_a_carrier() {
        use fig::Format;
        let fenced = |k| Some(MetaCarrier::Fenced(k));
        // Delimited: the three delimiter formats, but the fig dialect has none.
        assert_eq!(embed_carrier(EmbedStyle::Delimited, Format::Yaml), fenced(EmbedType::FrontmatterYaml));
        assert_eq!(embed_carrier(EmbedStyle::Delimited, Format::Toml), fenced(EmbedType::PlusToml));
        assert_eq!(embed_carrier(EmbedStyle::Delimited, Format::Json), fenced(EmbedType::FrontmatterJson));
        assert_eq!(embed_carrier(EmbedStyle::Delimited, Format::Fig), None);
        // Code block: fig lands in the ```fig block; the rest in ```lang blocks.
        assert_eq!(embed_carrier(EmbedStyle::CodeBlock, Format::Fig), fenced(EmbedType::FrontmatterFig));
        assert_eq!(embed_carrier(EmbedStyle::CodeBlock, Format::Yaml), fenced(EmbedType::FencedYaml));
        // HTML islands, both shapes.
        assert_eq!(embed_carrier(EmbedStyle::HtmlScript, Format::Json), fenced(EmbedType::HtmlScriptJson));
        assert_eq!(embed_carrier(EmbedStyle::HtmlCode, Format::Toml), fenced(EmbedType::HtmlCodeToml));
        // Separate is a whole-file sidecar in the chosen format (any format).
        assert_eq!(embed_carrier(EmbedStyle::Separate, Format::Yaml), Some(MetaCarrier::WholeFile(Format::Yaml)));
        assert_eq!(embed_carrier(EmbedStyle::Separate, Format::Fig), Some(MetaCarrier::WholeFile(Format::Fig)));
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let doc = Document::parse("note.md", "# Just a note\n").unwrap();
        assert!(doc.meta.is_null());
        assert_eq!(doc.body, "# Just a note\n");
        assert_eq!(doc.carrier, None);
        assert!(!doc.has_meta());
    }

    #[test]
    fn unterminated_fence_is_not_frontmatter() {
        let text = "---\ntitle: oops\nno closing fence\n";
        let doc = Document::parse("x.md", text).unwrap();
        assert!(doc.meta.is_null());
        assert_eq!(doc.body, text);
        assert_eq!(doc.carrier, None);
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn crlf_fences_are_handled() {
        let text = "---\r\ntitle: Root\r\n---\r\nbody\r\n";
        let doc = Document::parse("x.md", text).unwrap();
        assert_eq!(doc.carrier, Some(MetaCarrier::Fenced(EmbedType::FrontmatterYaml)));
        assert_eq!(doc.body, "body\r\n");
        // Exact scalar — fig ≥ 2.1.1 treats \r\n as a single line break.
        assert_eq!(doc.meta.get("title").and_then(Value::as_str), Some("Root"));
    }
}
