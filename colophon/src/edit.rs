//! Format-preserving edits to a document's metadata, whatever carries it.
//!
//! [`MetaEditor`] dispatches on the document's [`MetaCarrier`]: a fenced block
//! is edited with fig's [`fig::Embed`] (fences and body untouched), a config
//! document with fig's [`fig::Editor`] (the whole file *is* the metadata).
//! Either way the edit is comment-preserving and byte-minimal — only the
//! changed node's bytes move — and the original carrier and format are never
//! rewritten into another.
//!
//! The workspace mutation ops build on this; the free functions are the
//! single-document surface (the CLI's `set`/`unset`).

use fig::{Embed, EmbedType, Segment};

use crate::document::MetaCarrier;
use crate::error::{Error, Result};
use crate::meta::Mapping;

/// The frontmatter archetype used to synthesize a metadata block for a document
/// that has none. YAML (`---`) is the convention when compiled in; otherwise the
/// first other format that is. Exactly one arm survives `cfg` stripping, and the
/// `compile_error!` in `lib.rs` guarantees at least one does.
fn default_embed_type() -> EmbedType {
    #[cfg(feature = "yaml")]
    return EmbedType::FrontmatterYaml;
    #[cfg(all(not(feature = "yaml"), feature = "json"))]
    return EmbedType::FrontmatterJson;
    #[cfg(all(not(feature = "yaml"), not(feature = "json"), feature = "fig-lang"))]
    return EmbedType::FrontmatterFig;
}

/// A comment-preserving editor over a document's metadata, generic over where
/// the metadata lives.
pub enum MetaEditor {
    /// Editing a fenced block inside a host file.
    Fenced(Embed),
    /// Editing a config document (the whole file is the metadata).
    Whole(fig::Editor),
}

impl MetaEditor {
    /// Open an editor over `text` for an existing carrier.
    pub fn open(text: &str, carrier: MetaCarrier) -> Result<Self> {
        Ok(match carrier {
            MetaCarrier::Fenced(kind) => MetaEditor::Fenced(Embed::open(text.as_bytes(), kind)?),
            MetaCarrier::WholeFile(format) => {
                MetaEditor::Whole(fig::Editor::open(text.as_bytes(), format)?)
            }
        })
    }

    /// Open an editor over `text`, creating the metadata block when the
    /// document has none: an explicit carrier is honored (an absent fenced
    /// block is synthesized in place), and `None` defaults to a fresh
    /// frontmatter block in [`default_embed_type`]'s archetype (`---` YAML
    /// when that feature is compiled in).
    pub fn open_or_init(text: &str, carrier: Option<MetaCarrier>) -> Result<Self> {
        Ok(match carrier {
            Some(MetaCarrier::WholeFile(format)) => {
                MetaEditor::Whole(fig::Editor::open(text.as_bytes(), format)?)
            }
            Some(MetaCarrier::Fenced(kind)) => {
                MetaEditor::Fenced(Embed::open_or_init(text.as_bytes(), kind)?)
            }
            None => MetaEditor::Fenced(Embed::open_or_init(text.as_bytes(), default_embed_type())?),
        })
    }

    /// Upsert `value` at `path` (the trailing segment must be a key).
    pub fn set_value(&mut self, path: &[Segment], value: impl Into<fig::Value>) -> Result<()> {
        match self {
            MetaEditor::Fenced(e) => e.set_value(path, value)?,
            MetaEditor::Whole(e) => e.set_value(path, value)?,
        }
        Ok(())
    }

    /// Replace the existing value at `path`.
    pub fn replace_value(&mut self, path: &[Segment], value: impl Into<fig::Value>) -> Result<()> {
        match self {
            MetaEditor::Fenced(e) => e.replace_value(path, value)?,
            MetaEditor::Whole(e) => e.replace_value(path, value)?,
        }
        Ok(())
    }

    /// Append `value` to the sequence at `path`.
    pub fn append_value(&mut self, path: &[Segment], value: impl Into<fig::Value>) -> Result<()> {
        match self {
            MetaEditor::Fenced(e) => e.append_value(path, value)?,
            MetaEditor::Whole(e) => e.append_value(path, value)?,
        }
        Ok(())
    }

    /// Delete the mapping entry at `path`.
    pub fn delete(&mut self, path: &[Segment]) -> Result<()> {
        match self {
            MetaEditor::Fenced(e) => e.delete(path)?,
            MetaEditor::Whole(e) => e.delete(path)?,
        }
        Ok(())
    }

    /// Remove the item at `index` from the sequence at `path`.
    pub fn remove_item(&mut self, path: &[Segment], index: usize) -> Result<()> {
        match self {
            MetaEditor::Fenced(e) => e.remove_item(path, index)?,
            MetaEditor::Whole(e) => e.remove_item(path, index)?,
        }
        Ok(())
    }

    /// Render the full document text with the edits applied.
    pub fn render(&mut self) -> Result<String> {
        Ok(match self {
            MetaEditor::Fenced(e) => e.render()?.to_string(),
            MetaEditor::Whole(e) => e.source()?.to_string(),
        })
    }
}

/// Parse a dotted key path (`a.b.0.c`) into fig path segments. An all-digit
/// segment indexes a sequence; anything else names a mapping key.
pub fn key_path(dotted: &str) -> Vec<Segment<'_>> {
    dotted
        .split('.')
        .map(|part| match part.parse::<usize>() {
            Ok(index) => Segment::Index(index),
            Err(_) => Segment::Key(part),
        })
        .collect()
}

/// Interpret a CLI-provided scalar: `true`/`false`, integers, floats, and
/// `null` become their typed values; everything else stays a string.
pub fn infer_scalar(s: &str) -> fig::Value {
    match s {
        "true" => fig::Value::Bool(true),
        "false" => fig::Value::Bool(false),
        "null" | "~" => fig::Value::Null,
        _ => {
            if let Ok(i) = s.parse::<i64>() {
                fig::Value::Int(i)
            } else if let Ok(f) = s.parse::<f64>() {
                fig::Value::Float(f)
            } else {
                fig::Value::Str(s.to_string())
            }
        }
    }
}

/// Upsert `dotted` to `value` in `text`'s metadata (carrier-aware), creating
/// a YAML frontmatter block when the document has none. Returns the full
/// re-rendered document text.
pub fn set_in_text(
    text: &str,
    carrier: Option<MetaCarrier>,
    dotted: &str,
    value: fig::Value,
) -> Result<String> {
    let mut editor = MetaEditor::open_or_init(text, carrier)?;
    let path = key_path(dotted);
    match path.last() {
        // fig's `set` upserts a trailing *key*; an index-terminated path is a
        // pure replacement (there is no "insert at absent index" to upsert).
        Some(Segment::Index(_)) => editor.replace_value(&path, value)?,
        _ => editor.set_value(&path, value)?,
    }
    editor.render()
}

/// Delete the entry at `dotted` from `text`'s metadata (carrier-aware).
/// Returns the full re-rendered document text. Errors when the document has
/// no metadata or the path does not exist.
pub fn unset_in_text(text: &str, carrier: Option<MetaCarrier>, dotted: &str) -> Result<String> {
    let carrier = carrier
        .ok_or_else(|| Error::Structure("document has no embedded metadata block".into()))?;
    let mut editor = MetaEditor::open(text, carrier)?;
    editor.delete(&key_path(dotted))?;
    editor.render()
}

/// Re-emit `mapping` as a fresh metadata block of archetype `target`, placed in
/// `target`'s canonical position around the plain `body` (before it for
/// frontmatter, after it for endmatter) — the reconstruction a *format
/// conversion* performs. Unlike the comment-preserving edits above, this
/// deliberately rebuilds the block: a conversion crosses formats (a YAML comment
/// has no JSON home), so only the values survive.
///
/// The content is rendered by colophon's canonical [`serialize_mapping`] — the
/// same serializer behind `colophon meta --format`, so a converted block's
/// sequence and scalar layout matches the rest of the codebase (fig's per-key
/// [`Embed`] splice path renders some formats, notably fig sequences,
/// differently). The block's fences and placement come from fig, by synthesizing
/// an empty `target` block around `body` and splicing the serialized content into
/// its content slot.
///
/// The content is spliced verbatim — the same bytes colophon's reader
/// ([`Document::parse`](crate::Document::parse), via [`fig::split`]) hands back to
/// the format parser, which does not HTML-decode a `<pre><code>` island. Writing
/// what that reader expects keeps a converted value round-tripping through
/// `colophon get`/`check` rather than acquiring stray `&lt;` entities.
///
/// [`serialize_mapping`]: crate::meta::serialize_mapping
pub fn reformat_block(body: &str, mapping: &Mapping, target: EmbedType) -> Result<String> {
    let mut inner = crate::meta::serialize_mapping(mapping, target.inner_format())?;
    // The content slot sits between the opening fence's trailing newline and the
    // closing fence, so the content must end in exactly one newline for the close
    // fence to land on its own line.
    if !inner.ends_with('\n') {
        inner.push('\n');
    }
    // Synthesize an empty `target` block in its canonical place around `body`,
    // then replace its (empty) content slot with the serialized content: fig owns
    // the fences and placement, we own what goes between them.
    let rendered = Embed::open_or_init(body.as_bytes(), target)?
        .render()?
        .to_string();
    let content = Embed::extract(&rendered, target)?.region().content;
    let mut out = String::with_capacity(rendered.len() + inner.len());
    out.push_str(&rendered[..content.start]);
    out.push_str(&inner);
    out.push_str(&rendered[content.end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carrier_of(path: &str, text: &str) -> Option<MetaCarrier> {
        crate::document::Document::parse(path, text)
            .unwrap()
            .carrier
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn set_preserves_comments_and_format() {
        let text = "---\n# keep me\ntitle: Old\n---\nbody\n";
        let out =
            set_in_text(text, carrier_of("x.md", text), "title", infer_scalar("New")).unwrap();
        assert_eq!(out, "---\n# keep me\ntitle: New\n---\nbody\n");
    }

    #[cfg(feature = "fig-lang")]
    #[test]
    fn set_in_a_fig_block_stays_fig() {
        let text = "```fig\ntitle = colophon\n```\nbody\n";
        let out = set_in_text(
            text,
            carrier_of("x.md", text),
            "title",
            infer_scalar("renamed"),
        )
        .unwrap();
        assert!(out.starts_with("```fig\n"), "fence preserved: {out}");
        assert!(
            out.contains("title = renamed"),
            "fig dialect preserved: {out}"
        );
        assert!(out.ends_with("```\nbody\n"));
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn set_edits_a_bare_config_document() {
        let text = "# workspace registry\ntitle: ID registry\nregistry:\n  abc: a.md\n";
        let out = set_in_text(
            text,
            carrier_of("registry.yaml", text),
            "registry.abc",
            infer_scalar("moved/a.md"),
        )
        .unwrap();
        assert!(out.contains("# workspace registry"), "comment kept: {out}");
        assert!(out.contains("abc: moved/a.md"), "{out}");
        assert!(!out.contains("---"), "no fences grown: {out}");
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn set_creates_a_block_when_none_exists() {
        let out = set_in_text("just a body\n", None, "title", infer_scalar("T")).unwrap();
        assert!(out.starts_with("---\ntitle: T\n---\n"), "{out}");
        assert!(out.ends_with("just a body\n"));
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn unset_removes_only_the_named_key() {
        let text = "---\ntitle: T\ndraft: true\n---\nbody\n";
        let out = unset_in_text(text, carrier_of("x.md", text), "draft").unwrap();
        assert_eq!(out, "---\ntitle: T\n---\nbody\n");
        assert!(unset_in_text("no meta\n", None, "x").is_err());
    }

    #[test]
    fn scalars_are_inferred() {
        assert_eq!(infer_scalar("true"), fig::Value::Bool(true));
        assert_eq!(infer_scalar("42"), fig::Value::Int(42));
        assert_eq!(infer_scalar("4.5"), fig::Value::Float(4.5));
        assert_eq!(infer_scalar("null"), fig::Value::Null);
        assert_eq!(infer_scalar("hello"), fig::Value::Str("hello".into()));
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn dotted_paths_mix_keys_and_indices() {
        let text = "---\ncontents:\n- a.md\n- b.md\n---\n";
        let out = set_in_text(
            text,
            carrier_of("x.md", text),
            "contents.1",
            infer_scalar("c.md"),
        )
        .unwrap();
        assert!(out.contains("- a.md\n- c.md"), "{out}");
    }
}
