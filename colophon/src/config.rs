//! Workspace configuration — the typed policy a standalone/CLI workspace reads
//! from its **config document** (the `config`-relation target from the root,
//! DESIGN §6's reachability move applied to policy).
//!
//! Programmatic embedders never need this: they configure the [`Workspace`]
//! directly through the builder (`.link_style`, `.identity`, …), which is why
//! the type-level identity/index choice lives there. `WorkspaceConfig` is the
//! **data** shape that lets a workspace configure *itself* — so the same tool
//! serves a Diaryx-style vault and an Obsidian-style one purely by what the
//! config document declares:
//!
//! - [`WorkspaceConfig::paths_only`] — path links, identity off (pure paths).
//! - [`WorkspaceConfig::stable_ids`] — stable IDs minted lazily (registry +
//!   backlinks), portable links for the path-based parts.
//!
//! Each field maps to a config-document key ([`apply`](WorkspaceConfig::apply) /
//! [`to_mapping`](WorkspaceConfig::to_mapping)); unset keys keep their default,
//! and layering root-frontmatter then the config document gives the precedence
//! *config document > root frontmatter > default*.
//!
//! [`Workspace`]: crate::workspace::Workspace

use crate::content::ContentFormat;
use crate::document::EmbedStyle;
use crate::identity::Registration;
use crate::link::LinkStyle;
use crate::meta::{Mapping, Value};

/// The workspace-wide policy a config document declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// How path links (`part_of`/`contents`/`links`) are written.
    pub link_format: LinkStyle,
    /// When a document earns a stable ID — the identity registration triggers.
    pub identity: Registration,
    /// Whether colophon *authors* durable structural links as `colophon:<id>`
    /// (registering the target) rather than a path — the Obsidian-style,
    /// move-stable link. Ignored unless identity registers on a link.
    pub id_links: bool,
    /// The metadata format new documents get when they inherit no parent block
    /// — a *default* for authoring, never a workspace constraint (§7).
    pub default_embed_format: fig::Format,
    /// How that metadata is *embedded* — delimiters, a fenced code block, an
    /// HTML island, or a separate sidecar. Together with `default_embed_format`
    /// it selects the carrier a fresh root/document is authored in; recorded so
    /// the workspace is self-describing about its embedding convention. Like
    /// `default_embed_format`, an authoring default rather than a constraint:
    /// existing documents keep whatever carrier they already have.
    pub embed_style: EmbedStyle,
    /// The body-prose grammar the workspace is authored in (Markdown/Djot/HTML)
    /// — the format `render` and code-aware link scanning assume, and the
    /// intended default for new documents.
    pub content_format: ContentFormat,
}

impl Default for WorkspaceConfig {
    /// The standalone default: portable markdown-root links, identity available
    /// lazily (IDs minted only on a durable link-by-id or publish, §4), and
    /// path links (id-linking is opt-in).
    fn default() -> Self {
        Self {
            link_format: LinkStyle::default(),
            identity: Registration::LAZY,
            id_links: false,
            default_embed_format: fig::Format::Yaml,
            embed_style: EmbedStyle::Delimited,
            content_format: ContentFormat::Markdown,
        }
    }
}

impl WorkspaceConfig {
    /// Diaryx-style: path links, no identity — nothing mints an ID, so the
    /// workspace is addressed purely by path (the Adam's-Archive shape).
    pub fn paths_only() -> Self {
        Self {
            link_format: LinkStyle::MarkdownRoot,
            identity: Registration::OFF,
            id_links: false,
            default_embed_format: fig::Format::Yaml,
            embed_style: EmbedStyle::Delimited,
            content_format: ContentFormat::Markdown,
        }
    }

    /// Obsidian-style: stable IDs minted lazily (link-by-id or publish), and
    /// colophon authors structural links *by* id — so a move rewrites nothing,
    /// the registry keeps them resolving. Portable path links for the rest.
    pub fn stable_ids() -> Self {
        Self {
            link_format: LinkStyle::MarkdownRoot,
            identity: Registration::LAZY,
            id_links: true,
            default_embed_format: fig::Format::Yaml,
            embed_style: EmbedStyle::Delimited,
            content_format: ContentFormat::Markdown,
        }
    }

    /// Overlay the recognized keys present in `meta` onto this config; absent
    /// keys keep their current value. Apply root frontmatter first, then the
    /// config document, so the config document wins.
    pub fn apply(&mut self, meta: &Value) {
        if let Some(style) =
            meta.get("link_format").and_then(Value::as_str).and_then(LinkStyle::from_config_str)
        {
            self.link_format = style;
        }
        if let Some(registration) =
            meta.get("identity").and_then(Value::as_str).and_then(registration_from_str)
        {
            self.identity = registration;
        }
        if let Some(id_links) = meta.get("id_links").and_then(Value::as_bool) {
            self.id_links = id_links;
        }
        if let Some(format) =
            meta.get("embed_format").and_then(Value::as_str).and_then(format_from_str)
        {
            self.default_embed_format = format;
        }
        if let Some(style) =
            meta.get("embed_type").and_then(Value::as_str).and_then(EmbedStyle::from_config_str)
        {
            self.embed_style = style;
        }
        if let Some(content) =
            meta.get("content_format").and_then(Value::as_str).and_then(ContentFormat::from_config_str)
        {
            self.content_format = content;
        }
    }

    /// A fresh config with `meta`'s recognized keys applied over the defaults.
    pub fn from_meta(meta: &Value) -> Self {
        let mut config = Self::default();
        config.apply(meta);
        config
    }

    /// This config as config-document metadata keys (`link_format`, `identity`).
    pub fn to_mapping(&self) -> Mapping {
        let mut map = Mapping::new();
        map.insert("link_format".into(), Value::String(self.link_format.as_config_str().into()));
        map.insert("identity".into(), Value::String(registration_str(self.identity).into()));
        map.insert("id_links".into(), Value::Bool(self.id_links));
        map.insert("embed_format".into(), Value::String(format_str(self.default_embed_format).into()));
        map.insert("embed_type".into(), Value::String(self.embed_style.as_config_str().into()));
        map.insert("content_format".into(), Value::String(self.content_format.as_config_str().into()));
        map
    }
}

/// Parse the `embed_format` config value into a metadata format (only the
/// compiled-in formats are recognized; others → `None`, keeping the default).
fn format_from_str(value: &str) -> Option<fig::Format> {
    match value {
        "yaml" | "yml" => Some(fig::Format::Yaml),
        #[cfg(feature = "json")]
        "json" => Some(fig::Format::Json),
        #[cfg(feature = "toml")]
        "toml" => Some(fig::Format::Toml),
        #[cfg(feature = "fig-lang")]
        "fig" => Some(fig::Format::Fig),
        _ => None,
    }
}

/// The `embed_format` config spelling for a metadata format.
fn format_str(format: fig::Format) -> &'static str {
    match format {
        #[cfg(feature = "json")]
        fig::Format::Json => "json",
        #[cfg(feature = "toml")]
        fig::Format::Toml => "toml",
        #[cfg(feature = "fig-lang")]
        fig::Format::Fig => "fig",
        _ => "yaml",
    }
}

/// Parse the `identity` config value into a registration trigger set.
fn registration_from_str(value: &str) -> Option<Registration> {
    match value {
        "off" => Some(Registration::OFF),
        "lazy" => Some(Registration::LAZY),
        "eager" => Some(Registration::EAGER),
        _ => None,
    }
}

/// The `identity` config spelling for a registration trigger set. A custom
/// combination (not one of the three presets) is reported as its nearest name.
fn registration_str(registration: Registration) -> &'static str {
    match registration {
        Registration::OFF => "off",
        Registration::EAGER => "eager",
        _ => "lazy",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Trigger;

    #[test]
    fn presets_encode_the_two_styles() {
        // Diaryx: no identity, path links. Obsidian: identity + id-linking.
        assert_eq!(WorkspaceConfig::paths_only().identity, Registration::OFF);
        assert!(!WorkspaceConfig::paths_only().id_links);
        assert!(WorkspaceConfig::stable_ids().identity.fires_on(Trigger::Link));
        assert!(WorkspaceConfig::stable_ids().id_links);
    }

    #[test]
    fn round_trips_through_a_metadata_mapping() {
        let config = WorkspaceConfig {
            link_format: LinkStyle::PlainRelative,
            identity: Registration::EAGER,
            id_links: true,
            default_embed_format: fig::Format::Yaml,
            embed_style: EmbedStyle::CodeBlock,
            content_format: ContentFormat::Djot,
        };
        let back = WorkspaceConfig::from_meta(&Value::Mapping(config.to_mapping()));
        assert_eq!(back, config);
    }

    #[test]
    fn apply_overlays_only_present_keys_so_the_config_document_wins() {
        // Default: markdown_root + lazy.
        let mut config = WorkspaceConfig::default();

        // Root frontmatter sets only link_format (diaryx compat).
        let mut root = Mapping::new();
        root.insert("link_format".into(), Value::String("plain_canonical".into()));
        config.apply(&Value::Mapping(root));
        assert_eq!(config.link_format, LinkStyle::PlainCanonical);
        assert_eq!(config.identity, Registration::LAZY, "identity untouched");

        // The config document then overrides identity, link_format preserved.
        let mut doc = Mapping::new();
        doc.insert("identity".into(), Value::String("off".into()));
        config.apply(&Value::Mapping(doc));
        assert_eq!(config.identity, Registration::OFF);
        assert_eq!(config.link_format, LinkStyle::PlainCanonical);
    }
}
