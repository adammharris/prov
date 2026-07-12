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

use std::collections::BTreeMap;

use crate::content::ContentFormat;
use crate::document::EmbedStyle;
use crate::identity::Registration;
use crate::link::{Addressing, LinkStyle, ReferenceStyle, Wrapper};
use crate::meta::{Mapping, Value};

/// A per-relation reference-style override, as declared in a config document's
/// `relations` block. Each axis is optional and inherits the workspace default
/// ([`WorkspaceConfig::reference_style`]) when absent — so a block need only name
/// the axes it changes. This is the config-document form of
/// [`Relation::style`](crate::relation::Relation::style), and what lets links
/// going "down" (`contents`) differ from links going "up" (`part_of`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelationStyleConfig {
    /// The wrapper override (`markdown` / `wikilink`).
    pub wrapper: Option<Wrapper>,
    /// The addressing override (`path` / `id` / `alias`).
    pub target: Option<Addressing>,
    /// The `id`-wikilink label override.
    pub label: Option<bool>,
}

/// Where a document's stable ID is persisted — the identity-storage axis
/// (DESIGN §5). Orthogonal to *when* an ID is minted ([`Registration`]) and to
/// how references are spelled; this is purely the ID's *home*.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IdStorage {
    /// **Model B, registry only** (the default): IDs live solely in the registry
    /// document — authoritative, non-derivable, resolved by direct lookup.
    #[default]
    Registry,
    /// **Model B's escape hatch**: each document also carries its own ID in an
    /// `id` frontmatter field (a portable, self-describing shadow), and the
    /// registry is retained as a rebuildable cache + tombstone ledger. The ID
    /// travels with the file across copies and out-of-band moves.
    Frontmatter,
    /// **Frontmatter only**: the `id` field is the sole home; no registry
    /// document is written and resolution rebuilds the id→path map by scanning
    /// frontmatter. Maximally self-describing, but it forfeits tombstones (a
    /// deleted file takes its ID with it), so an ID can in principle be reminted.
    FrontmatterOnly,
}

impl IdStorage {
    /// Whether this mode writes the ID into each document's `id` frontmatter.
    pub fn stamps_frontmatter(self) -> bool {
        matches!(self, IdStorage::Frontmatter | IdStorage::FrontmatterOnly)
    }

    /// Whether this mode keeps a registry document (the authoritative store, or —
    /// under [`Frontmatter`](IdStorage::Frontmatter) — a rebuildable cache).
    pub fn keeps_registry(self) -> bool {
        matches!(self, IdStorage::Registry | IdStorage::Frontmatter)
    }

    /// Parse the `id_storage` config spelling; unknown → `None`.
    pub fn from_config_str(value: &str) -> Option<Self> {
        match value {
            "registry" => Some(Self::Registry),
            "frontmatter" => Some(Self::Frontmatter),
            "frontmatter_only" => Some(Self::FrontmatterOnly),
            _ => None,
        }
    }

    /// The `id_storage` config spelling.
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::Frontmatter => "frontmatter",
            Self::FrontmatterOnly => "frontmatter_only",
        }
    }
}

/// The workspace-wide policy a config document declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// How path links (`part_of`/`contents`/`links`) are written.
    pub link_format: LinkStyle,
    /// When a document earns a stable ID — the identity registration triggers.
    pub identity: Registration,
    /// Whether colophon *authors* durable structural links by id (registering
    /// the target) rather than a path — the Obsidian-style, move-stable link.
    /// Ignored unless identity registers on a link. A convenience over the
    /// richer `reference_*` axes; superseded by `reference_target` when set.
    pub id_links: bool,
    /// The default reference **wrapper** (`markdown` / `wikilink`) — `None`
    /// derives markdown, the diaryx-shaped default. Overridden per relation by
    /// [`Relation::style`](crate::relation::Relation::style).
    pub reference_wrapper: Option<Wrapper>,
    /// The default reference **addressing** (`path` / `id` / `alias`) — `None`
    /// derives from `id_links` (id when set, else path).
    pub reference_target: Option<Addressing>,
    /// Whether id links carry a `|Title` label (an `id` wikilink) or a `[Title]`
    /// (a markdown id link) — `None` derives `false` (a bare id link).
    pub reference_label: Option<bool>,
    /// Per-relation reference-style overrides, keyed by relation name — the
    /// config-document form of [`Relation::style`](crate::relation::Relation::style).
    /// Each entry overlays the workspace default for that relation only, letting
    /// `contents` (down) and `part_of` (up) carry different styles. Empty means
    /// every relation inherits the default. Resolve with
    /// [`resolved_relation_styles`](Self::resolved_relation_styles).
    pub relation_styles: BTreeMap<String, RelationStyleConfig>,
    /// Where a document's stable ID is persisted — registry, frontmatter shadow,
    /// or frontmatter only (DESIGN §5). Independent of the `identity` trigger.
    pub id_storage: IdStorage,
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
            reference_wrapper: None,
            reference_target: None,
            reference_label: None,
            relation_styles: BTreeMap::new(),
            id_storage: IdStorage::Registry,
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
            reference_wrapper: None,
            reference_target: None,
            reference_label: None,
            relation_styles: BTreeMap::new(),
            id_storage: IdStorage::Registry,
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
            reference_wrapper: None,
            reference_target: None,
            reference_label: None,
            relation_styles: BTreeMap::new(),
            id_storage: IdStorage::Registry,
            default_embed_format: fig::Format::Yaml,
            embed_style: EmbedStyle::Delimited,
            content_format: ContentFormat::Markdown,
        }
    }

    /// The effective workspace-default [`ReferenceStyle`] — the fallback for any
    /// relation without its own override. Composes the explicit `reference_*`
    /// axes over the legacy `link_format`/`id_links` inputs, so an existing
    /// config (which sets neither `reference_*` key) behaves exactly as before.
    pub fn reference_style(&self) -> ReferenceStyle {
        let derived_addressing = if self.id_links { Addressing::Id } else { Addressing::Path };
        ReferenceStyle {
            wrapper: self.reference_wrapper.unwrap_or(Wrapper::Markdown),
            addressing: self.reference_target.unwrap_or(derived_addressing),
            label: self.reference_label.unwrap_or(false),
            path_style: self.link_format,
        }
        .normalized()
    }

    /// The declared per-relation overrides resolved to full [`ReferenceStyle`]s,
    /// each partial overlaid on the workspace default ([`reference_style`]) and
    /// normalized. Feed the result to
    /// [`RelationSet::with_styles`](crate::relation::RelationSet::with_styles) to
    /// build the workspace's relation vocabulary from a config document. Empty
    /// when no relation declares an override — every relation then inherits the
    /// default.
    ///
    /// [`reference_style`]: Self::reference_style
    pub fn resolved_relation_styles(&self) -> BTreeMap<String, ReferenceStyle> {
        let base = self.reference_style();
        self.relation_styles
            .iter()
            .map(|(name, over)| {
                let style = ReferenceStyle {
                    wrapper: over.wrapper.unwrap_or(base.wrapper),
                    addressing: over.target.unwrap_or(base.addressing),
                    label: over.label.unwrap_or(base.label),
                    path_style: base.path_style,
                }
                .normalized();
                (name.clone(), style)
            })
            .collect()
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
        if let Some(wrapper) =
            meta.get("reference_wrapper").and_then(Value::as_str).and_then(Wrapper::from_config_str)
        {
            self.reference_wrapper = Some(wrapper);
        }
        if let Some(target) =
            meta.get("reference_target").and_then(Value::as_str).and_then(Addressing::from_config_str)
        {
            self.reference_target = Some(target);
        }
        if let Some(label) = meta.get("reference_label").and_then(Value::as_bool) {
            self.reference_label = Some(label);
        }
        // Per-relation style overrides: `relations: { <name>: { style: { … } } }`.
        // Each axis present overlays that relation's entry; absent axes keep
        // whatever the entry (or, later, the workspace default) already holds.
        if let Some(relations) = meta.get("relations").and_then(Value::as_mapping) {
            for (name, spec) in relations {
                let Some(style) = spec.get("style").and_then(Value::as_mapping) else {
                    continue;
                };
                let entry = self.relation_styles.entry(name.clone()).or_default();
                if let Some(wrapper) =
                    style.get("wrapper").and_then(Value::as_str).and_then(Wrapper::from_config_str)
                {
                    entry.wrapper = Some(wrapper);
                }
                if let Some(target) =
                    style.get("target").and_then(Value::as_str).and_then(Addressing::from_config_str)
                {
                    entry.target = Some(target);
                }
                if let Some(label) = style.get("label").and_then(Value::as_bool) {
                    entry.label = Some(label);
                }
            }
        }
        if let Some(storage) =
            meta.get("id_storage").and_then(Value::as_str).and_then(IdStorage::from_config_str)
        {
            self.id_storage = storage;
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
        if let Some(wrapper) = self.reference_wrapper {
            map.insert("reference_wrapper".into(), Value::String(wrapper.as_config_str().into()));
        }
        if let Some(target) = self.reference_target {
            map.insert("reference_target".into(), Value::String(target.as_config_str().into()));
        }
        if let Some(label) = self.reference_label {
            map.insert("reference_label".into(), Value::Bool(label));
        }
        if !self.relation_styles.is_empty() {
            let mut relations = Mapping::new();
            for (name, over) in &self.relation_styles {
                let mut style = Mapping::new();
                if let Some(wrapper) = over.wrapper {
                    style.insert("wrapper".into(), Value::String(wrapper.as_config_str().into()));
                }
                if let Some(target) = over.target {
                    style.insert("target".into(), Value::String(target.as_config_str().into()));
                }
                if let Some(label) = over.label {
                    style.insert("label".into(), Value::Bool(label));
                }
                let mut relation = Mapping::new();
                relation.insert("style".into(), Value::Mapping(style));
                relations.insert(name.clone(), Value::Mapping(relation));
            }
            map.insert("relations".into(), Value::Mapping(relations));
        }
        map.insert("id_storage".into(), Value::String(self.id_storage.as_config_str().into()));
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
            reference_wrapper: Some(Wrapper::Wikilink),
            reference_target: Some(Addressing::Id),
            reference_label: Some(true),
            relation_styles: BTreeMap::from([
                (
                    "contents".to_string(),
                    RelationStyleConfig {
                        wrapper: Some(Wrapper::Wikilink),
                        target: Some(Addressing::Alias),
                        label: None,
                    },
                ),
                (
                    "part_of".to_string(),
                    RelationStyleConfig {
                        wrapper: Some(Wrapper::Markdown),
                        target: Some(Addressing::Id),
                        label: Some(false),
                    },
                ),
            ]),
            id_storage: IdStorage::Frontmatter,
            default_embed_format: fig::Format::Yaml,
            embed_style: EmbedStyle::CodeBlock,
            content_format: ContentFormat::Djot,
        };
        let back = WorkspaceConfig::from_meta(&Value::Mapping(config.to_mapping()));
        assert_eq!(back, config);
    }

    #[test]
    fn per_relation_styles_resolve_over_the_workspace_default() {
        // The diaryx up≠down example: a workspace default of `id`, with `contents`
        // (down) overridden to a nominal alias wikilink and `part_of` (up) to a
        // bare markdown id link — each partial overlaying the default.
        let mut cfg = WorkspaceConfig::default();
        let mut doc = Mapping::new();
        doc.insert("reference_target".into(), Value::String("id".into()));
        let mut contents_style = Mapping::new();
        contents_style.insert("wrapper".into(), Value::String("wikilink".into()));
        contents_style.insert("target".into(), Value::String("alias".into()));
        let mut part_of_style = Mapping::new();
        part_of_style.insert("target".into(), Value::String("id".into()));
        let mut contents = Mapping::new();
        contents.insert("style".into(), Value::Mapping(contents_style));
        let mut part_of = Mapping::new();
        part_of.insert("style".into(), Value::Mapping(part_of_style));
        let mut relations = Mapping::new();
        relations.insert("contents".into(), Value::Mapping(contents));
        relations.insert("part_of".into(), Value::Mapping(part_of));
        doc.insert("relations".into(), Value::Mapping(relations));
        cfg.apply(&Value::Mapping(doc));

        let styles = cfg.resolved_relation_styles();
        let down = styles.get("contents").expect("contents style");
        assert_eq!(down.wrapper, Wrapper::Wikilink);
        assert_eq!(down.addressing, Addressing::Alias);

        let up = styles.get("part_of").expect("part_of style");
        // Inherits the default wrapper (markdown), keeps its own id target.
        assert_eq!(up.wrapper, Wrapper::Markdown);
        assert_eq!(up.addressing, Addressing::Id);
    }

    #[test]
    fn reference_style_composes_overrides_over_legacy_inputs() {
        // No reference_* keys: derives from link_format + id_links (back-compat).
        let legacy = WorkspaceConfig { id_links: true, ..WorkspaceConfig::default() };
        let s = legacy.reference_style();
        assert_eq!(s.wrapper, Wrapper::Markdown);
        assert_eq!(s.addressing, Addressing::Id);
        assert!(!s.label);

        // Explicit wikilink + alias overrides, read from a config document.
        let mut cfg = WorkspaceConfig::default();
        let mut doc = Mapping::new();
        doc.insert("reference_wrapper".into(), Value::String("wikilink".into()));
        doc.insert("reference_target".into(), Value::String("id".into()));
        doc.insert("reference_label".into(), Value::Bool(true));
        cfg.apply(&Value::Mapping(doc));
        let s = cfg.reference_style();
        assert_eq!(s.wrapper, Wrapper::Wikilink);
        assert_eq!(s.addressing, Addressing::Id);
        assert!(s.label);

        // markdown + alias is normalized to wikilink + alias.
        let mut cfg = WorkspaceConfig::default();
        cfg.reference_target = Some(Addressing::Alias);
        assert_eq!(cfg.reference_style().wrapper, Wrapper::Wikilink);
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
