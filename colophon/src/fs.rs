//! colophon's filesystem port.
//!
//! colophon is generic over *where* documents live. Rather than depend on any
//! one concrete backend — `std::fs`, `tokio::fs`, or a browser filesystem like
//! OPFS/IndexedDB — the library asks only for a small async trait that mirrors
//! the slice of [`std::fs`] its scan/traverse/mutate engine needs. Integrators
//! implement [`Storage`] over whatever backend they have; the workspace never
//! learns which one.
//!
//! This is the classic *ports and adapters* seam. The trait uses native
//! `async fn` (no boxed futures) because [`crate::workspace::Workspace`] is
//! generic over its backend rather than erased to `dyn`, so callers keep the
//! backend's real future types and their `Send`-ness. A backend whose futures
//! are `Send` composes into multithreaded runtimes unchanged.
//!
//! The method set mirrors [`std::fs`] names exactly so an adapter is mechanical
//! to write.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// An async filesystem backend colophon can drive.
///
/// Each method mirrors the [`std::fs`] function of the same name. Backends
/// implement the read/write/mutate/inspect surface; [`try_exists`] has a
/// default in terms of [`metadata`].
///
/// [`try_exists`]: Storage::try_exists
/// [`metadata`]: Storage::metadata
pub trait Storage {
    // ---- read ----

    /// Read the entire contents of a file as bytes. Mirrors [`std::fs::read`].
    fn read(&self, path: &Path) -> impl Future<Output = io::Result<Vec<u8>>>;

    /// Read the entire contents of a file as a string. Mirrors
    /// [`std::fs::read_to_string`].
    fn read_to_string(&self, path: &Path) -> impl Future<Output = io::Result<String>>;

    /// Return the entries in a directory (non-recursive). Mirrors
    /// [`std::fs::read_dir`], but yields a `Vec` since async iterators are not
    /// yet stable.
    fn read_dir(&self, path: &Path) -> impl Future<Output = io::Result<Vec<DirEntry>>>;

    // ---- write ----

    /// Write a file, replacing it if it already exists. Mirrors
    /// [`std::fs::write`].
    fn write(&self, path: &Path, contents: &[u8]) -> impl Future<Output = io::Result<()>>;

    /// Create a directory and all missing parents. Mirrors
    /// [`std::fs::create_dir_all`].
    fn create_dir_all(&self, path: &Path) -> impl Future<Output = io::Result<()>>;

    // ---- mutate ----

    /// Remove a regular file. Mirrors [`std::fs::remove_file`].
    fn remove_file(&self, path: &Path) -> impl Future<Output = io::Result<()>>;

    /// Recursively remove a directory and its contents. Mirrors
    /// [`std::fs::remove_dir_all`].
    fn remove_dir_all(&self, path: &Path) -> impl Future<Output = io::Result<()>>;

    /// Rename or move a file or directory. Mirrors [`std::fs::rename`].
    fn rename(&self, from: &Path, to: &Path) -> impl Future<Output = io::Result<()>>;

    // ---- inspect ----

    /// Return metadata about the entry at `path`. Mirrors
    /// [`std::fs::metadata`]; follows symlinks.
    fn metadata(&self, path: &Path) -> impl Future<Output = io::Result<Metadata>>;

    /// Returns `Ok(true)` if the path exists, `Ok(false)` if it does not, and
    /// `Err(_)` if the check itself failed. Mirrors [`std::fs::try_exists`].
    fn try_exists(&self, path: &Path) -> impl Future<Output = io::Result<bool>> {
        async move {
            match self.metadata(path).await {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            }
        }
    }

    // ---- durability ----
    //
    // colophon spans backends with very different crash guarantees — `std::fs`
    // (atomic rename and fsync on every major OS), OPFS (a flush primitive but a
    // weak rename), IndexedDB (its own multi-object transactions). Rather than
    // assume the strongest of these and silently lie on the weakest, the crash-
    // safety machinery *asks* what a backend can promise and adapts. These three
    // members are defaulted to the pessimistic answer, so a backend gains a
    // guarantee only by explicitly claiming it.

    /// What durability guarantees this backend can make. Defaults to
    /// [`Capabilities::NONE`] — a backend promises a guarantee only by saying so,
    /// so an adapter that forgets to override this degrades to the most defensive
    /// path rather than to a false promise.
    fn capabilities(&self) -> Capabilities {
        Capabilities::NONE
    }

    /// Flush `path` — and the directory entry that names it — through to durable
    /// storage, so once this returns the most recent write to `path` survives a
    /// power loss. After a [`rename`](Storage::rename), syncing the destination
    /// also makes the rename itself durable, because the parent directory is
    /// flushed with it.
    ///
    /// The default is a no-op, which is the *correct* behavior for any backend
    /// whose [`capabilities`](Storage::capabilities) report `durable_sync:
    /// false`: it cannot make the promise, so it must not pretend to. A backend
    /// that can flush must both override this and report `durable_sync: true` —
    /// the two always travel together.
    fn sync(&self, path: &Path) -> impl Future<Output = io::Result<()>> {
        async move {
            let _ = path;
            Ok(())
        }
    }

    /// Replace `path`'s contents with `contents` atomically and durably: no
    /// observer — concurrent reader or post-crash survivor — ever sees a splice
    /// of old and new bytes, and once this returns the new contents outlive a
    /// power loss.
    ///
    /// The default composes the primitives into the standard protocol — write a
    /// temporary sibling, [`sync`](Storage::sync) it so its bytes are on disk,
    /// [`rename`](Storage::rename) it over the target (*this* is the atomic
    /// instant), then `sync` the target so the rename is durable — whenever
    /// [`capabilities`](Storage::capabilities) report `atomic_replace`. A backend
    /// that cannot rename atomically falls back to a plain durable write, which
    /// is *not* crash-atomic; a caller that needs the guarantee consults
    /// `capabilities` and leans on the journal instead of pretending this call
    /// gave it. A backend with a better native path — a transactional store —
    /// overrides this method wholesale.
    ///
    /// The temporary is removed on any failure, so a torn attempt leaves the
    /// target exactly as it was and no litter behind. It is a dotted sibling in
    /// the target's own directory, so the follow-up rename stays within one
    /// filesystem (a cross-device rename is neither atomic nor, often, even
    /// permitted).
    fn write_atomic(&self, path: &Path, contents: &[u8]) -> impl Future<Output = io::Result<()>> {
        async move {
            if !self.capabilities().atomic_replace {
                // No atomic rename to lean on: the honest best effort is a plain
                // durable write. Not crash-atomic — and the caller was told so by
                // `capabilities`, so this is a documented degrade, not a lie.
                self.write(path, contents).await?;
                return self.sync(path).await;
            }
            let tmp = temp_sibling(path);
            // Any failure past this point must not leave the staging file behind,
            // and must never have touched the target — hence the whole dance
            // happens on `tmp` and only the rename names `path`.
            let staged = async {
                self.write(&tmp, contents).await?;
                self.sync(&tmp).await?;
                self.rename(&tmp, path).await
            }
            .await;
            match staged {
                Ok(()) => self.sync(path).await,
                Err(e) => {
                    // Best-effort cleanup: if even this fails the target is still
                    // untouched, so the atomicity promise holds regardless — the
                    // worst case is one stray dotfile, not a torn document.
                    let _ = self.remove_file(&tmp).await;
                    Err(e)
                }
            }
        }
    }
}

/// The durability guarantees a [`Storage`] backend can make — declared by the
/// backend through [`Storage::capabilities`], honored by colophon's crash-safety
/// machinery.
///
/// The point of naming these explicitly is that colophon must run correctly over
/// backends that keep very different promises. Rather than assume a guarantee and
/// corrupt data on the backend that cannot keep it, colophon reads the
/// capabilities and picks the strongest *protocol the backend actually supports*:
/// a filesystem gets atomic-rename writes and a journal; a transactional store is
/// handed the whole change set to commit itself; a backend that can promise
/// neither still works, it simply cannot claim a write survives a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The backend can replace an existing file's contents in one indivisible
    /// step, so no crash exposes a half-written file — an observer sees the whole
    /// old contents or the whole new. On a filesystem this is realized by
    /// [`Storage::write_atomic`]'s write-temp-then-`rename`; a backend may
    /// instead be atomic by nature.
    pub atomic_replace: bool,

    /// The backend can flush a write through to durable storage, so that once
    /// [`Storage::sync`] returns the bytes survive a power cut: `fsync` on
    /// `std::fs`, `FileSystemSyncAccessHandle.flush()` on OPFS, the implicit
    /// durability of a committed IndexedDB transaction.
    pub durable_sync: bool,

    /// The backend commits changes to *many* objects as one indivisible unit, so
    /// colophon's own write-ahead journal would be redundant and it should defer
    /// to the backend instead. True for IndexedDB; false for a plain filesystem,
    /// where multi-file atomicity is colophon's job to provide.
    pub native_transactions: bool,
}

impl Capabilities {
    /// Promises nothing — the safe assumption for an unknown backend, and the
    /// [`Storage::capabilities`] default. Every field is the pessimistic value,
    /// so code that checks a capability before relying on it takes the most
    /// defensive branch unless a backend has explicitly earned a lighter one.
    pub const NONE: Self =
        Self { atomic_replace: false, durable_sync: false, native_transactions: false };

    /// A conventional local filesystem: atomic replacement by rename and durable
    /// fsync, but no native multi-object transaction (that is the journal's job).
    /// What [`StdFs`] reports on every platform colophon targets.
    pub const LOCAL_FS: Self =
        Self { atomic_replace: true, durable_sync: true, native_transactions: false };
}

/// The temporary sibling [`Storage::write_atomic`]'s default protocol stages a
/// write through before renaming it into place. A dotted, suffixed name in the
/// target's own directory: dotted and suffixed so it reads as plainly colophon's
/// and will not collide with a real document, and a *sibling* so the rename that
/// follows never crosses a filesystem boundary.
fn temp_sibling(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("document");
    path.with_file_name(format!(".{name}.colophon-tmp"))
}

/// One entry returned by [`Storage::read_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    path: PathBuf,
    file_type: FileType,
}

impl DirEntry {
    /// Construct an entry from its path and type.
    pub fn new(path: impl Into<PathBuf>, file_type: FileType) -> Self {
        Self { path: path.into(), file_type }
    }

    /// The full path to the entry.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The final component of the entry's path.
    pub fn file_name(&self) -> Option<&std::ffi::OsStr> {
        self.path.file_name()
    }

    /// The entry's type.
    pub fn file_type(&self) -> FileType {
        self.file_type
    }
}

/// Metadata about a filesystem entry — the subset colophon needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    file_type: FileType,
    len: u64,
    modified: Option<SystemTime>,
}

impl Metadata {
    /// Construct metadata from its parts.
    pub fn new(file_type: FileType, len: u64, modified: Option<SystemTime>) -> Self {
        Self { file_type, len, modified }
    }

    /// The entry's type.
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    /// Whether the entry is a regular file.
    pub fn is_file(&self) -> bool {
        self.file_type.is_file()
    }

    /// Whether the entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }

    /// Size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the entry is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Last-modified time, if the backend reports one. Mirrors
    /// [`std::fs::Metadata::modified`], returning [`io::ErrorKind::Unsupported`]
    /// when unavailable.
    pub fn modified(&self) -> io::Result<SystemTime> {
        self.modified
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "modified time unavailable"))
    }
}

/// [`Storage`] over the process filesystem (`std::fs`).
///
/// The trait is async so that genuinely async backends (network, OPFS) fit;
/// this adapter's futures are immediately ready, so any executor — including
/// the dependency-free [`crate::exec::block_on`] — drives them to completion
/// in a single poll.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFs;

impl Storage for StdFs {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        std::fs::read_dir(path)?
            .map(|entry| {
                let entry = entry?;
                Ok(DirEntry::new(entry.path(), convert_file_type(entry.file_type()?)))
            })
            .collect()
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        std::fs::write(path, contents)
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        let md = std::fs::metadata(path)?;
        Ok(Metadata::new(convert_file_type(md.file_type()), md.len(), md.modified().ok()))
    }

    fn capabilities(&self) -> Capabilities {
        // Every OS colophon targets gives an atomic same-filesystem rename and an
        // fsync. `std::fs::rename` replaces the destination on all of them —
        // POSIX by definition, Windows via `MoveFileEx(MOVEFILE_REPLACE_EXISTING)`
        // — so the write-temp-then-rename protocol in the default `write_atomic`
        // is genuinely atomic here.
        Capabilities::LOCAL_FS
    }

    async fn sync(&self, path: &Path) -> io::Result<()> {
        sync_path(path)
    }
}

/// Flush `path`'s data and the directory entry naming it, so a preceding write or
/// rename is durable. The two-step fsync (file, then parent directory) is what
/// makes [`Storage::write_atomic`]'s rename survive a power cut, and it is also
/// the one place a real OS difference lives, so it is quarantined behind the port
/// here rather than leaking up into the engine.
fn sync_path(path: &Path) -> io::Result<()> {
    // Flush the file itself. A fresh read handle is enough: fsync acts on the
    // inode, not the descriptor, so it flushes writes made through any handle.
    // A path that does not exist (a fallback write that failed before creating
    // it) has nothing to flush and is not an error.
    match std::fs::File::open(path) {
        Ok(file) => file.sync_all()?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Flush the parent directory so a *create* or *rename* — a change to the
    // directory, not the file — is itself durable. This is a POSIX facility:
    // a directory can be opened and fsynced. On Windows a directory handle
    // cannot be fsynced this way (and `MoveFileEx`'s durability is a separate
    // story), so the step is compiled out there rather than faked.
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        match std::fs::File::open(parent) {
            Ok(dir) => dir.sync_all()?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn convert_file_type(ft: std::fs::FileType) -> FileType {
    if ft.is_dir() {
        FileType::DIR
    } else if ft.is_file() {
        FileType::FILE
    } else {
        FileType::SYMLINK
    }
}

/// [`Storage`] over `std::fs` that fails the *n*th write, for testing that a
/// [`ChangeSet`](crate::change::ChangeSet) unwinds.
///
/// Every other method delegates to [`StdFs`], so a workspace over this backend
/// behaves exactly like a real one until the chosen write, then reports the kind
/// of failure a full disk or a revoked permission would.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FailAtWrite {
    writes: std::cell::Cell<usize>,
    fail_at: usize,
}

#[cfg(test)]
impl FailAtWrite {
    /// Fail the `fail_at`th write (0-indexed); let every other one through.
    pub(crate) fn nth(fail_at: usize) -> Self {
        Self { writes: std::cell::Cell::new(0), fail_at }
    }

    /// Never fail — a counting [`StdFs`]. Pair with
    /// [`attempted`](Self::attempted) to learn how many writes an operation
    /// makes, so a test can then fail each of them in turn.
    pub(crate) fn never() -> Self {
        Self::nth(usize::MAX)
    }

    /// How many writes have been attempted.
    ///
    /// Only meaningful after a *successful* run: once a write fails, the
    /// rollback's own writes go through this same backend and are counted too.
    pub(crate) fn attempted(&self) -> usize {
        self.writes.get()
    }
}

#[cfg(test)]
impl Storage for FailAtWrite {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        StdFs.read(path).await
    }
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        StdFs.read_to_string(path).await
    }
    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        StdFs.read_dir(path).await
    }
    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        // The write-ahead journal's own writes are infrastructure, not document
        // writes: leaving them uncounted keeps `nth` addressing the *document*
        // write a test means to fail, the way a real full disk fills mid-content
        // rather than mid-journal. A journal-write failure has its own test.
        if crate::journal::is_journal_path(path) {
            return StdFs.write(path, contents).await;
        }
        let n = self.writes.get();
        self.writes.set(n + 1);
        if n == self.fail_at {
            return Err(io::Error::other("disk full (test)"));
        }
        StdFs.write(path, contents).await
    }
    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.create_dir_all(path).await
    }
    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_file(path).await
    }
    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_dir_all(path).await
    }
    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        StdFs.rename(from, to).await
    }
    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        StdFs.metadata(path).await
    }
    // A faithful local filesystem in every respect but the chosen failing write,
    // so a change set applied over it exercises the *real* atomic-write protocol
    // (temp, sync, rename) — the injected failure lands on the staging write and
    // the target is never touched, exactly as a full disk mid-write would behave.
    fn capabilities(&self) -> Capabilities {
        Capabilities::LOCAL_FS
    }
    async fn sync(&self, path: &Path) -> io::Result<()> {
        StdFs.sync(path).await
    }
}

/// A [`Storage`] over `std::fs` that records the ordered sequence of mutating
/// operations it performs, for asserting a *protocol* — the one durability
/// guarantee a unit test cannot check by actually crashing. It reports whatever
/// [`Capabilities`] it is built with, and never overrides
/// [`write_atomic`](Storage::write_atomic), so a test observes the default
/// protocol's own internal ordering (write temp → sync temp → rename → sync
/// target) rather than a substitute.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct RecordingFs {
    log: std::cell::RefCell<Vec<FsEvent>>,
    caps: Capabilities,
}

/// One mutating operation [`RecordingFs`] observed, in order. Reads are not
/// recorded — the protocol under test is about the sequence of *durability*
/// steps, and a read changes nothing.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FsEvent {
    Write(PathBuf),
    Sync(PathBuf),
    Rename(PathBuf, PathBuf),
    Remove(PathBuf),
}

#[cfg(test)]
impl RecordingFs {
    /// A recorder that reports the local-filesystem guarantees, so `write_atomic`
    /// runs its full atomic protocol.
    pub(crate) fn local() -> Self {
        Self { log: std::cell::RefCell::new(Vec::new()), caps: Capabilities::LOCAL_FS }
    }

    /// A recorder that reports the given capabilities — used to observe the
    /// `atomic_replace: false` fallback taking the plain-write path.
    pub(crate) fn with_caps(caps: Capabilities) -> Self {
        Self { log: std::cell::RefCell::new(Vec::new()), caps }
    }

    /// The operations recorded so far, in the order they happened.
    pub(crate) fn events(&self) -> Vec<FsEvent> {
        self.log.borrow().clone()
    }
}

#[cfg(test)]
impl Storage for RecordingFs {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        StdFs.read(path).await
    }
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        StdFs.read_to_string(path).await
    }
    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        StdFs.read_dir(path).await
    }
    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.log.borrow_mut().push(FsEvent::Write(path.to_path_buf()));
        StdFs.write(path, contents).await
    }
    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.create_dir_all(path).await
    }
    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.log.borrow_mut().push(FsEvent::Remove(path.to_path_buf()));
        StdFs.remove_file(path).await
    }
    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_dir_all(path).await
    }
    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.log.borrow_mut().push(FsEvent::Rename(from.to_path_buf(), to.to_path_buf()));
        StdFs.rename(from, to).await
    }
    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        StdFs.metadata(path).await
    }
    fn capabilities(&self) -> Capabilities {
        self.caps
    }
    async fn sync(&self, path: &Path) -> io::Result<()> {
        self.log.borrow_mut().push(FsEvent::Sync(path.to_path_buf()));
        StdFs.sync(path).await
    }
}

/// A local filesystem whose `rename` always fails — the fault an atomic write
/// must survive without ever touching the target. Every other operation is real,
/// so the staging write genuinely happens and the test can prove it was cleaned
/// up and the target left untouched.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FailingRename;

#[cfg(test)]
impl Storage for FailingRename {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        StdFs.read(path).await
    }
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        StdFs.read_to_string(path).await
    }
    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        StdFs.read_dir(path).await
    }
    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        StdFs.write(path, contents).await
    }
    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.create_dir_all(path).await
    }
    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_file(path).await
    }
    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        StdFs.remove_dir_all(path).await
    }
    async fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(io::Error::other("rename failed (test)"))
    }
    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        StdFs.metadata(path).await
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::LOCAL_FS
    }
    async fn sync(&self, path: &Path) -> io::Result<()> {
        StdFs.sync(path).await
    }
}

/// The type of a filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileType {
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

impl FileType {
    /// A regular file.
    pub const FILE: FileType = FileType { is_dir: false, is_file: true, is_symlink: false };

    /// A directory.
    pub const DIR: FileType = FileType { is_dir: true, is_file: false, is_symlink: false };

    /// A symbolic link.
    pub const SYMLINK: FileType = FileType { is_dir: false, is_file: false, is_symlink: true };

    /// Whether this is a regular file.
    pub fn is_file(&self) -> bool {
        self.is_file
    }

    /// Whether this is a directory.
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// Whether this is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.is_symlink
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::block_on;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("colophon-fs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---- capability declaration ----

    #[test]
    fn stdfs_declares_the_local_filesystem_guarantees() {
        // The native adapter promises atomic replacement and durable fsync, but
        // not native transactions — the journal's job, not the filesystem's.
        assert_eq!(StdFs.capabilities(), Capabilities::LOCAL_FS);
        assert!(StdFs.capabilities().atomic_replace);
        assert!(StdFs.capabilities().durable_sync);
        assert!(!StdFs.capabilities().native_transactions);
    }

    #[test]
    fn the_default_capability_promises_nothing() {
        // A backend that does not override `capabilities` is assumed to guarantee
        // nothing, so nothing above it can rely on a promise it never made. Proven
        // through a backend (`RecordingFs::with_caps`) that reports the default.
        let bare = RecordingFs::with_caps(Capabilities::NONE);
        assert_eq!(bare.capabilities(), Capabilities::NONE);
        // NONE is every field at its pessimistic value — spelled out so a future
        // edit that quietly flips one has to change this line too.
        assert_eq!(
            Capabilities::NONE,
            Capabilities { atomic_replace: false, durable_sync: false, native_transactions: false }
        );
    }

    // ---- the atomic-write protocol ----

    #[test]
    fn write_atomic_follows_the_durable_replace_protocol() {
        // The one durability guarantee a unit test cannot check by crashing, so it
        // checks the *protocol* that makes a crash survivable instead: the new
        // bytes are written and flushed to a *temporary* file, and only then
        // renamed over the target — so a crash at any instant leaves the target
        // wholly old or wholly new, never spliced — and the target is flushed
        // last, which is what makes the rename itself durable.
        let root = tmp("protocol");
        std::fs::write(root.join("doc.md"), "old").unwrap();
        let fs = RecordingFs::local();
        let target = root.join("doc.md");
        let temp = root.join(".doc.md.colophon-tmp");

        block_on(fs.write_atomic(&target, b"new")).unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(!temp.exists(), "the staging file must be gone");
        assert_eq!(
            fs.events(),
            vec![
                FsEvent::Write(temp.clone()),
                FsEvent::Sync(temp.clone()),
                FsEvent::Rename(temp.clone(), target.clone()),
                FsEvent::Sync(target.clone()),
            ],
            "the atomic-replace protocol ran out of order"
        );
    }

    #[test]
    fn a_torn_atomic_write_leaves_the_target_untouched_and_no_litter() {
        // The atomicity promise stated as a failure: a write that dies at the
        // rename — the moment nearest the atomic instant — must leave the target
        // exactly as it was, and clean up its staging file.
        let root = tmp("atomic-fail");
        std::fs::write(root.join("doc.md"), "old").unwrap();
        let target = root.join("doc.md");
        let temp = root.join(".doc.md.colophon-tmp");

        let err = block_on(FailingRename.write_atomic(&target, b"new")).unwrap_err();

        assert!(err.to_string().contains("rename failed"), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old", "target was touched");
        assert!(!temp.exists(), "the staging file was left behind");
    }

    #[test]
    fn write_atomic_creates_a_new_file_without_disturbing_the_directory() {
        // The target need not already exist — replacing "nothing" with the whole
        // file is still atomic, and still routes through the staging sibling.
        let root = tmp("atomic-create");
        let fs = RecordingFs::local();
        let target = root.join("fresh.md");
        let temp = root.join(".fresh.md.colophon-tmp");

        block_on(fs.write_atomic(&target, b"hello")).unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        assert!(!temp.exists());
        assert_eq!(fs.events().first(), Some(&FsEvent::Write(temp)));
    }

    #[test]
    fn a_non_atomic_backend_writes_straight_through_without_claiming_atomicity() {
        // With `atomic_replace: false` there is no rename to lean on, so
        // `write_atomic` degrades to a plain durable write — the bytes still land,
        // it simply does not route through a staging sibling and makes no
        // crash-atomicity claim. Proven by the absence of a Rename in the log.
        let root = tmp("fallback");
        let fs = RecordingFs::with_caps(Capabilities::NONE);
        let target = root.join("doc.md");

        block_on(fs.write_atomic(&target, b"hello")).unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        assert_eq!(
            fs.events(),
            vec![FsEvent::Write(target.clone()), FsEvent::Sync(target)],
            "the fallback must write the target directly, with no staging rename"
        );
        assert!(!root.join(".doc.md.colophon-tmp").exists());
    }

    // ---- sync ----

    #[test]
    fn sync_of_a_missing_path_is_not_an_error() {
        // A fallback write that failed before creating the file leaves nothing to
        // flush; asking to sync it is a no-op, not a failure.
        let root = tmp("sync-missing");
        block_on(StdFs.sync(&root.join("never-created.md"))).unwrap();
    }
}
