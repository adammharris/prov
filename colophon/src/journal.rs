//! The write-ahead journal — what makes a whole [`ChangeSet`](crate::ChangeSet)
//! crash-atomic, not just each file in it.
//!
//! [`crate::change`] already lands every document write atomically (via
//! [`Storage::write_atomic`]) and unwinds the set in memory on any *error*. The
//! one failure that leaves behind — a `kill -9` or a power cut *between* two of a
//! set's writes — is what this closes. The mechanism is the classic write-ahead
//! log, specialized to the one shape colophon's change sets take: a sequence of
//! whole-file writes, renames, and removes, each self-contained.
//!
//! ## The protocol
//!
//! Before touching a single document, [`ChangeSet::apply`](crate::ChangeSet::apply)
//! writes this journal — the complete list of intended ops — and flushes it. That
//! flush is the **commit point**. Because the journal is itself written through
//! [`Storage::write_atomic`], it appears whole or not at all, so a crash leaves
//! the disk in exactly one of two states:
//!
//! - **No journal** (the crash beat the commit point). No document write had
//!   started yet either, so the workspace is untouched — nothing to recover.
//! - **A whole journal** (the crash came after the commit point). Some, all, or
//!   none of the document writes may have landed. [`recover`] replays the journal
//!   forward — idempotently, so already-applied ops are no-ops — bringing the
//!   workspace to the fully-applied state, then deletes the journal.
//!
//! So an interrupted change set always resolves to a *consistent* workspace:
//! either fully before it (the commit point was never reached) or fully after it
//! (recovery rolled it forward). The one honesty worth stating plainly:
//!
//! > Which of the two an interruption yields depends on whether colophon kept
//! > control. An **error** returned mid-apply is unwound in memory — the
//! > workspace ends up fully *before*. A **crash** loses that chance, so recovery
//! > rolls the journaled set fully *forward* instead. Both endpoints are
//! > consistent; they are simply different consistent states, and colophon does
//! > not pretend a lost-power mutation didn't happen when its intent was already
//! > durably on disk.
//!
//! ## Format
//!
//! A compact, length-prefixed binary encoding with a magic header and a trailing
//! checksum. The journal is ephemeral machine state, not a document the user
//! owns, so it is not `fig` and not meant to be read by hand — and binary keeps
//! opaque payloads (an attached photo staged for a write) exact without escaping.
//! The checksum is belt-and-suspenders: `write_atomic` already makes the journal
//! all-or-nothing, so a torn *write* is impossible, but bit-rot on the way back
//! is not, and a journal that cannot be trusted must be refused loudly rather
//! than replayed into corruption.

use std::path::{Path, PathBuf};

use crate::change::FileOp;
use crate::error::{Error, Result};
use crate::fs::Storage;

/// The workspace-root-relative name of the journal file. A single transient
/// dotfile: it exists only between a change set's commit point and its
/// completion, so in steady state the workspace carries no journal at all, and
/// no dotfolder is spawned to hold one. It survives a crash solely so [`recover`]
/// can find it, and is removed the moment recovery (or a clean apply) finishes.
pub const JOURNAL_NAME: &str = ".colophon-journal";

/// The magic prefix stamped on every journal, embedding a one-byte format
/// version (`1`). A file that does not start with this is not a journal colophon
/// wrote — or is one from an incompatible future version — and is refused rather
/// than guessed at.
const MAGIC: &[u8; 8] = b"COLOJRN1";

/// Whether `path` names the journal (or its `write_atomic` staging sibling).
/// Used so a fault-injecting test backend can leave the journal's own writes
/// alone and fail only the document writes it means to.
#[cfg(test)]
pub(crate) fn is_journal_path(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains("colophon-journal"))
}

/// Serialize a change set's ops into journal bytes: `MAGIC`, the op count, each
/// op, then a checksum over everything preceding it.
pub(crate) fn encode(ops: &[FileOp]) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&(ops.len() as u64).to_le_bytes());
    for op in ops {
        match op {
            FileOp::Write { path, bytes } => {
                buf.push(0);
                put_path(&mut buf, path)?;
                put_bytes(&mut buf, bytes);
            }
            FileOp::Rename { from, to } => {
                buf.push(1);
                put_path(&mut buf, from)?;
                put_path(&mut buf, to)?;
            }
            FileOp::Remove { path } => {
                buf.push(2);
                put_path(&mut buf, path)?;
            }
        }
    }
    let checksum = fnv1a(&buf);
    buf.extend_from_slice(&checksum.to_le_bytes());
    Ok(buf)
}

/// Parse journal bytes back into ops, verifying the magic and the checksum. A
/// mismatch is an [`Error::Structure`] — a journal that cannot be trusted is
/// refused, never partially replayed.
pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<FileOp>> {
    let corrupt = |what: &str| Error::Structure(format!("journal is corrupt: {what}"));

    if bytes.len() < MAGIC.len() + 8 + 8 || &bytes[..MAGIC.len()] != MAGIC {
        return Err(corrupt("not a colophon journal (bad header)"));
    }
    let body_end = bytes.len() - 8;
    let stored = u64::from_le_bytes(bytes[body_end..].try_into().unwrap());
    if fnv1a(&bytes[..body_end]) != stored {
        return Err(corrupt("checksum mismatch"));
    }

    let mut cur = Cursor { bytes: &bytes[..body_end], at: MAGIC.len() };
    let count = cur.take_u64()?;
    let mut ops = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let op = match cur.take_u8()? {
            0 => FileOp::Write { path: cur.take_path()?, bytes: cur.take_bytes()?.to_vec() },
            1 => FileOp::Rename { from: cur.take_path()?, to: cur.take_path()? },
            2 => FileOp::Remove { path: cur.take_path()? },
            other => return Err(corrupt(&format!("unknown op tag {other}"))),
        };
        ops.push(op);
    }
    if cur.at != cur.bytes.len() {
        return Err(corrupt("trailing bytes after the last op"));
    }
    Ok(ops)
}

/// The outcome of a [`recover`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovered {
    /// No journal was present — steady state, the common case.
    Nothing,
    /// A journal was found and its `ops` ops were rolled forward, then it was
    /// removed. The workspace was interrupted mid-change and is now consistent.
    Applied(usize),
}

/// Finish any change set a crash left journaled at `root`, rolling the workspace
/// forward to the fully-applied state, then remove the journal.
///
/// The recovery entry point: an `open` or a `check` runs it so an interrupted
/// mutation heals before anything reads the workspace. A no-op when no journal is
/// present, so it is cheap to call unconditionally. Replay is idempotent — a
/// write already landed is simply rewritten, a rename already done is recognized
/// and skipped — so recovering the *same* journal twice (a crash *during*
/// recovery) is safe.
pub async fn recover<FS: Storage>(fs: &FS, root: &Path) -> Result<Recovered> {
    let journal = root.join(JOURNAL_NAME);
    let bytes = match fs.read(&journal).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Recovered::Nothing),
        Err(e) => return Err(e.into()),
    };
    let ops = decode(&bytes)?;
    for op in &ops {
        replay(fs, root, op).await?;
    }
    fs.remove_file(&journal).await?;
    Ok(Recovered::Applied(ops.len()))
}

/// Re-apply one journaled op, tolerant of it having already landed before the
/// crash — this is what makes rolling a journal forward idempotent.
async fn replay<FS: Storage>(fs: &FS, root: &Path, op: &FileOp) -> Result<()> {
    match op {
        // Whole-file writes are idempotent by nature: writing the intended bytes
        // again reaches the same state whether or not the crash beat this op.
        FileOp::Write { path, bytes } => {
            let full = root.join(path);
            ensure_parent(fs, &full).await?;
            fs.write_atomic(&full, bytes).await?;
        }
        // A remove of a file already gone is the state we wanted, not a failure.
        FileOp::Remove { path } => {
            let full = root.join(path);
            match fs.remove_file(&full).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        // The one op that is not naturally idempotent: after it lands, the source
        // is gone and the destination present, so a blind re-rename would fail.
        // Recover by state — move it if the source is still there, accept it as
        // done if only the destination is, and refuse only if *neither* exists,
        // which no honest interruption of this set can produce.
        FileOp::Rename { from, to } => {
            let (from_full, to_full) = (root.join(from), root.join(to));
            if fs.try_exists(&from_full).await? {
                ensure_parent(fs, &to_full).await?;
                fs.rename(&from_full, &to_full).await?;
            } else if fs.try_exists(&to_full).await? {
                // Already renamed before the crash — nothing to redo.
            } else {
                return Err(Error::Structure(format!(
                    "journal replay: neither {} nor {} exists — cannot complete the rename",
                    from_full.display(),
                    to_full.display()
                )));
            }
        }
    }
    Ok(())
}

async fn ensure_parent<FS: Storage>(fs: &FS, full: &Path) -> Result<()> {
    if let Some(dir) = full.parent() {
        fs.create_dir_all(dir).await?;
    }
    Ok(())
}

// ---- encoding helpers ----

fn put_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// Encode a workspace-relative path as UTF-8. colophon addresses documents by
/// UTF-8 paths throughout; a non-UTF-8 path cannot arise from its own mutations,
/// so refusing one here is a real invariant, not a lost capability.
fn put_path(buf: &mut Vec<u8>, path: &Path) -> Result<()> {
    let s = path.to_str().ok_or_else(|| {
        Error::Structure(format!("journal cannot encode non-UTF-8 path: {}", path.display()))
    })?;
    put_bytes(buf, s.as_bytes());
    Ok(())
}

/// A forward-only reader over the journal body, bounds-checking every take so a
/// truncated or malformed record surfaces as an error rather than a panic.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn short() -> Error {
        Error::Structure("journal is corrupt: unexpected end of data".into())
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self.at.checked_add(n).ok_or_else(Self::short)?;
        let slice = self.bytes.get(self.at..end).ok_or_else(Self::short)?;
        self.at = end;
        Ok(slice)
    }

    fn take_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn take_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn take_bytes(&mut self) -> Result<&[u8]> {
        let len = self.take_u64()? as usize;
        self.take(len)
    }

    fn take_path(&mut self) -> Result<PathBuf> {
        let bytes = self.take_bytes()?;
        let s = std::str::from_utf8(bytes)
            .map_err(|_| Error::Structure("journal is corrupt: non-UTF-8 path".into()))?;
        Ok(PathBuf::from(s))
    }
}

/// FNV-1a, 64-bit — a small, deterministic, dependency-free checksum. It guards
/// against bit-rot in a journal read back after a crash; it is not, and need not
/// be, cryptographic.
fn fnv1a(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::block_on;
    use crate::fs::StdFs;

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("colophon-journal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(root: &Path, rel: &str) -> Option<String> {
        std::fs::read_to_string(root.join(rel)).ok()
    }

    // ---- encoding ----

    #[test]
    fn a_change_set_round_trips_through_the_journal() {
        let ops = vec![
            FileOp::Write { path: "child.md".into(), bytes: b"hello".to_vec() },
            FileOp::Rename { from: "a.md".into(), to: "sub/a.md".into() },
            FileOp::Remove { path: "gone.md".into() },
        ];
        let bytes = encode(&ops).unwrap();
        assert_eq!(decode(&bytes).unwrap(), ops);
    }

    #[test]
    fn binary_payloads_survive_the_journal_verbatim() {
        // An attached photo staged for a write is opaque bytes, not text — the
        // journal must carry it exactly, with no escaping or UTF-8 assumption.
        let payload: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
        let ops = vec![FileOp::Write { path: "photo.png".into(), bytes: payload.clone() }];
        let decoded = decode(&encode(&ops).unwrap()).unwrap();
        assert_eq!(decoded, ops);
    }

    #[test]
    fn a_tampered_journal_is_refused_not_replayed() {
        // The checksum's whole job: a journal whose bytes changed under it must be
        // rejected loudly, never silently replayed into a corrupt workspace.
        let ops = vec![FileOp::Write { path: "child.md".into(), bytes: b"hello".to_vec() }];
        let mut bytes = encode(&ops).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        let err = decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("corrupt"), "{err}");
    }

    #[test]
    fn a_non_journal_file_is_rejected() {
        assert!(decode(b"not a journal at all").is_err());
        assert!(decode(b"").is_err());
    }

    // ---- recovery: simulated crashes ----
    //
    // A unit test cannot pull the power, so it constructs the exact on-disk state
    // a crash at a given instant would leave — a whole journal plus some prefix of
    // its ops applied — and asserts recovery reaches the fully-applied state.

    #[test]
    fn recovery_completes_a_change_set_that_had_not_started() {
        // Crash right after the commit point: journal on disk, no op applied yet.
        let root = tmp("recover-none-applied");
        std::fs::write(root.join("parent.md"), "old parent").unwrap();
        let ops = vec![
            FileOp::Write { path: "child.md".into(), bytes: b"child".to_vec() },
            FileOp::Write { path: "parent.md".into(), bytes: b"new parent".to_vec() },
        ];
        std::fs::write(root.join(JOURNAL_NAME), encode(&ops).unwrap()).unwrap();

        let outcome = block_on(recover(&StdFs, &root)).unwrap();

        assert_eq!(outcome, Recovered::Applied(2));
        assert_eq!(read(&root, "child.md").as_deref(), Some("child"));
        assert_eq!(read(&root, "parent.md").as_deref(), Some("new parent"));
        assert!(!root.join(JOURNAL_NAME).exists(), "journal must be cleared after recovery");
    }

    #[test]
    fn recovery_completes_a_partially_applied_change_set() {
        // Crash mid-apply: the first write landed, the second did not.
        let root = tmp("recover-partial");
        std::fs::write(root.join("parent.md"), "old parent").unwrap();
        let ops = vec![
            FileOp::Write { path: "child.md".into(), bytes: b"child".to_vec() },
            FileOp::Write { path: "parent.md".into(), bytes: b"new parent".to_vec() },
        ];
        std::fs::write(root.join(JOURNAL_NAME), encode(&ops).unwrap()).unwrap();
        // Simulate the first op having landed before the crash.
        std::fs::write(root.join("child.md"), "child").unwrap();

        block_on(recover(&StdFs, &root)).unwrap();

        assert_eq!(read(&root, "child.md").as_deref(), Some("child"));
        assert_eq!(read(&root, "parent.md").as_deref(), Some("new parent"));
        assert!(!root.join(JOURNAL_NAME).exists());
    }

    #[test]
    fn recovery_rolls_a_rename_forward_from_either_side_of_the_crash() {
        // A rename is the one non-idempotent op. Recovery must complete it whether
        // the crash struck before it (source still present) or after (only the
        // destination present).
        for already_moved in [false, true] {
            let root = tmp(&format!("recover-rename-{already_moved}"));
            let ops = vec![FileOp::Rename { from: "a.md".into(), to: "sub/a.md".into() }];
            std::fs::write(root.join(JOURNAL_NAME), encode(&ops).unwrap()).unwrap();
            if already_moved {
                std::fs::create_dir_all(root.join("sub")).unwrap();
                std::fs::write(root.join("sub/a.md"), "moved").unwrap();
            } else {
                std::fs::write(root.join("a.md"), "moved").unwrap();
            }

            block_on(recover(&StdFs, &root)).unwrap();

            assert_eq!(read(&root, "sub/a.md").as_deref(), Some("moved"));
            assert!(!root.join("a.md").exists());
            assert!(!root.join(JOURNAL_NAME).exists());
        }
    }

    #[test]
    fn recovery_is_a_noop_when_there_is_no_journal() {
        let root = tmp("recover-noop");
        std::fs::write(root.join("doc.md"), "untouched").unwrap();
        assert_eq!(block_on(recover(&StdFs, &root)).unwrap(), Recovered::Nothing);
        assert_eq!(read(&root, "doc.md").as_deref(), Some("untouched"));
    }

    #[test]
    fn recovering_the_same_journal_twice_is_safe() {
        // A crash *during* recovery must be survivable: replaying an already-
        // recovered (or re-created) journal reaches the same state, never an error.
        let root = tmp("recover-twice");
        std::fs::write(root.join("parent.md"), "old").unwrap();
        let ops = vec![FileOp::Write { path: "parent.md".into(), bytes: b"new".to_vec() }];
        let journal = encode(&ops).unwrap();

        std::fs::write(root.join(JOURNAL_NAME), &journal).unwrap();
        block_on(recover(&StdFs, &root)).unwrap();
        // Recovery removed the journal; imagine the crash left it and re-run.
        std::fs::write(root.join(JOURNAL_NAME), &journal).unwrap();
        block_on(recover(&StdFs, &root)).unwrap();

        assert_eq!(read(&root, "parent.md").as_deref(), Some("new"));
        assert!(!root.join(JOURNAL_NAME).exists());
    }
}
