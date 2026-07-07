//! Identity — a strictly-additive layer over a path-only workspace.
//!
//! Everything below is optional. The graph and mutation layers operate on
//! paths and never require an ID. This module only decides **when** a document
//! earns a stable ID (the trigger set) and **what** that ID looks like (the
//! mint). *Where* IDs are stored is [`crate::index`].
//!
//! The default is [`NoIdentity`] — identity off, no ID ever written. The
//! recommended lazy policy registers an ID only when something durably refers
//! to a document (a link-by-id or a publish), keeping the authoritative set as
//! small as possible.
//!
//! ## The ID scheme
//!
//! Colophon's internal IDs share their lineage with diaryx's ARK blades
//! (`diaryx_ark`) but carry no NAAN or shoulder — they are workspace-internal,
//! not published permalinks (DESIGN §4's two identity layers). An ID is
//! [`BLADE_RANDOM_LEN`] random characters from the 28-character betanumeric
//! [`ALPHABET`] (no vowels — no accidental words; no `0`/`1`/`l` — no
//! ambiguity) plus one NOID-style check character, so a typo'd ID is *detected*
//! rather than silently resolving to nothing. Minting is random (opaque for
//! free), with uniqueness enforced by rejection against the index — including
//! its tombstones, so a deleted document's ID is never reissued.

use std::path::Path;

/// A stable, opaque document identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub String);

impl Id {
    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The 28-character betanumeric alphabet shared with diaryx's ARK blades: no
/// vowels (avoids accidental words), no `0`/`1`/`l` (ambiguous), includes `y`.
pub const ALPHABET: &[u8; 28] = b"bcdfghjkmnpqrstvwxyz23456789";

/// Number of symbols in [`ALPHABET`] — the radix for the check character.
pub const RADIX: usize = ALPHABET.len();

/// Random characters per ID (excluding the check character). 28^6 ≈ 481M —
/// collision-free in practice for a workspace, enforced absolutely by
/// mint-with-rejection.
pub const BLADE_RANDOM_LEN: usize = 6;

/// Total ID length: the random body plus one check character.
pub const BLADE_LEN: usize = BLADE_RANDOM_LEN + 1;

/// The 0-based ordinal of `c` in [`ALPHABET`], or `None` if absent.
#[inline]
fn ordinal(c: char) -> Option<usize> {
    if c.is_ascii() {
        ALPHABET.iter().position(|&b| b as char == c)
    } else {
        None
    }
}

/// The NOID-style check character over `body`: each character contributes
/// `ordinal × (1-based position)` (characters outside the alphabet contribute
/// 0 but still advance the position); the sum modulo [`RADIX`] indexes the
/// check character. Identical math to `diaryx_ark::check_char`.
pub fn check_char(body: &str) -> char {
    let mut sum: usize = 0;
    for (i, c) in body.chars().enumerate() {
        sum += ordinal(c).unwrap_or(0) * (i + 1);
    }
    ALPHABET[sum % RADIX] as char
}

/// Whether `id` is a well-formed colophon ID: correct length, alphabet-only,
/// and a matching trailing check character. This is what catches a typo'd
/// `colophon:` link before it dangles silently.
pub fn verify(id: &str) -> bool {
    if id.len() != BLADE_LEN || !id.bytes().all(|b| ALPHABET.contains(&b)) {
        return false;
    }
    let (body, check) = id.split_at(BLADE_RANDOM_LEN);
    check.chars().next() == Some(check_char(body))
}

/// Which events cause a document to be assigned (registered) an ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registration {
    /// Register every document at creation time (eager).
    pub on_create: bool,
    /// Register when a document is first referenced by ID (e.g. a wikilink).
    pub on_link: bool,
    /// Register when a document is published.
    pub on_publish: bool,
}

impl Registration {
    /// Never register — identity is effectively off.
    pub const OFF: Self = Self { on_create: false, on_link: false, on_publish: false };
    /// Register only on a durable reference (link-by-id or publish). Recommended.
    pub const LAZY: Self = Self { on_create: false, on_link: true, on_publish: true };
    /// Register every document the moment it is created.
    pub const EAGER: Self = Self { on_create: true, on_link: true, on_publish: true };

    /// Whether any trigger is active.
    pub fn is_active(&self) -> bool {
        self.on_create || self.on_link || self.on_publish
    }
}

/// The registration event a caller is asking about (see
/// [`crate::workspace::Workspace`]'s `register`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A document was created.
    Create,
    /// Something is about to link to the document by ID.
    Link,
    /// The document is being published.
    Publish,
}

impl Registration {
    /// Whether this trigger set fires for `event`.
    pub fn fires_on(&self, event: Trigger) -> bool {
        match event {
            Trigger::Create => self.on_create,
            Trigger::Link => self.on_link,
            Trigger::Publish => self.on_publish,
        }
    }
}

/// A policy deciding when to register documents and how their IDs are minted.
pub trait IdentityPolicy {
    /// The registration trigger set for this policy.
    fn registration(&self) -> Registration;

    /// Mint a fresh ID for the document at `path`. Only called when a trigger
    /// fires, so a disabled policy need never produce a meaningful value.
    /// Uniqueness is the *caller's* job (mint-with-rejection against the
    /// index); a mint may repeat.
    fn mint(&mut self, path: &Path) -> Id;
}

/// Identity disabled — the default. Paths only; no ID is ever minted or written.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoIdentity;

impl IdentityPolicy for NoIdentity {
    fn registration(&self) -> Registration {
        Registration::OFF
    }

    fn mint(&mut self, _path: &Path) -> Id {
        // Unreachable in practice: `OFF` fires no triggers.
        Id(String::new())
    }
}

/// The bundled minting policy: betanumeric + check IDs from a seeded PRNG.
///
/// The PRNG is xorshift64 — *not* cryptographic, and not claimed to be: these
/// are opaque internal handles whose uniqueness is enforced by rejection, not
/// by entropy. Keeping the state a plain `u64` keeps the policy (and any
/// workspace carrying it) `Clone`/`Debug`, and a fixed seed makes tests
/// deterministic. A deployment wanting stronger opacity (or ARK permalinks,
/// like diaryx) implements [`IdentityPolicy`] itself.
#[derive(Debug, Clone)]
pub struct Minter {
    registration: Registration,
    state: u64,
}

impl Minter {
    /// Register only on a durable reference (the recommended default),
    /// randomizing from `seed`.
    pub fn lazy(seed: u64) -> Self {
        Self::with(Registration::LAZY, seed)
    }

    /// Register every document at creation, randomizing from `seed`.
    pub fn eager(seed: u64) -> Self {
        Self::with(Registration::EAGER, seed)
    }

    /// Register on a custom trigger set, randomizing from `seed`.
    pub fn with(registration: Registration, seed: u64) -> Self {
        Self {
            registration,
            // xorshift64 has a single fixed point at 0; nudge it off.
            state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed },
        }
    }

    /// The next PRNG byte, rejection-sampled so the alphabet mapping stays
    /// uniform (256 is not a multiple of 28; bytes ≥ 252 are discarded).
    fn next_alphabet_char(&mut self) -> char {
        const LIMIT: u8 = (256 / RADIX as u16 * RADIX as u16) as u8; // 252
        loop {
            // xorshift64 step, then take the top byte (the weakest bits of
            // xorshift are the low ones).
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            let b = (self.state >> 56) as u8;
            if b < LIMIT {
                return ALPHABET[(b as usize) % RADIX] as char;
            }
        }
    }
}

impl IdentityPolicy for Minter {
    fn registration(&self) -> Registration {
        self.registration
    }

    fn mint(&mut self, _path: &Path) -> Id {
        let mut body: String = (0..BLADE_RANDOM_LEN).map(|_| self.next_alphabet_char()).collect();
        body.push(check_char(&body));
        Id(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_identity_is_off() {
        assert!(!NoIdentity.registration().is_active());
    }

    #[test]
    fn lazy_registers_on_link_and_publish_only() {
        let r = Minter::lazy(1).registration();
        assert!(!r.fires_on(Trigger::Create));
        assert!(r.fires_on(Trigger::Link));
        assert!(r.fires_on(Trigger::Publish));
    }

    #[test]
    fn eager_registers_on_create() {
        assert!(Minter::eager(1).registration().fires_on(Trigger::Create));
    }

    #[test]
    fn mints_verified_distinct_opaque_ids() {
        let mut p = Minter::eager(42);
        let a = p.mint(Path::new("a.md"));
        let b = p.mint(Path::new("b.md"));
        assert_ne!(a, b);
        for id in [&a, &b] {
            assert_eq!(id.as_str().len(), BLADE_LEN);
            assert!(verify(id.as_str()), "{id}");
        }
    }

    #[test]
    fn same_seed_is_deterministic() {
        let a = Minter::lazy(7).mint(Path::new("x"));
        let b = Minter::lazy(7).mint(Path::new("y"));
        assert_eq!(a, b, "path does not participate in the mint");
    }

    #[test]
    fn verify_rejects_typos() {
        let id = Minter::lazy(3).mint(Path::new("x")).0;
        assert!(verify(&id));
        // Flip one body character to another alphabet character.
        let mut chars: Vec<char> = id.chars().collect();
        chars[0] = if chars[0] == 'b' { 'c' } else { 'b' };
        let typo: String = chars.iter().collect();
        assert!(!verify(&typo), "{typo}");
        // Wrong length, wrong alphabet.
        assert!(!verify("bcd"));
        assert!(!verify("aeiouAy"));
    }

    #[test]
    fn check_char_matches_the_ark_lineage() {
        // Independently computed: ordinals b=0,c=1,d=2,f=3,g=4,h=5 weighted by
        // position 1..=6 → 0+2+6+12+20+30 = 70; 70 % 28 = 14 → ALPHABET[14] = 't'.
        assert_eq!(check_char("bcdfgh"), 't');
    }
}
