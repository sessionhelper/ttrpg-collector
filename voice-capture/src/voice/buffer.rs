//! Disk-backed pre-consent audio cache (F2).
//!
//! Every participant in an active session gets its own sub-directory under
//! `$LOCAL_BUFFER_DIR/<session_id>/<pseudo_id>/`. Decrypted PCM frames stream
//! in as `chunk_<seq:06>.pcm` files written in append order.
//!
//! - On Accept, [`flush_and_delete`] streams each chunk file to the data-api
//!   in strict sequence order and then deletes the participant's subdirectory.
//! - On Decline/timeout or session end, [`delete_participant`] /
//!   [`delete_session`] unlink the relevant directory.
//! - A per-participant cap (`LOCAL_BUFFER_MAX_SECS`) triggers oldest-first
//!   eviction. Each eviction increments `chronicle_prerolled_chunks_dropped_total`.
//!
//! The cache keeps RAM flat: all PCM lives on disk until flushed.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, warn};
use uuid::Uuid;

use crate::api_client::{ApiError, DataApiClient};

/// 48kHz × 16-bit × stereo = 192_000 bytes per second per participant.
pub const BYTES_PER_SECOND_48K_S16_STEREO: u64 = 48_000 * 2 * 2;

/// Roots the cache layout. Cheap to clone — every session handle carries one.
#[derive(Clone)]
pub struct BufferRoot {
    /// `$LOCAL_BUFFER_DIR`
    root: PathBuf,
    /// Per-participant cap in bytes, computed from seconds × sample-rate × channels × bit-depth.
    max_bytes_per_participant: u64,
}

impl BufferRoot {
    pub fn new(root: PathBuf, max_seconds: u64) -> Self {
        Self {
            max_bytes_per_participant: max_seconds.saturating_mul(BYTES_PER_SECOND_48K_S16_STEREO),
            root,
        }
    }

    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Per-session root: `$LOCAL_BUFFER_DIR/<session_id>/`
    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    /// Per-participant root: `$LOCAL_BUFFER_DIR/<session_id>/<pseudo_id>/`
    pub fn participant_dir(&self, session_id: &str, pseudo_id: &str) -> PathBuf {
        self.session_dir(session_id).join(pseudo_id)
    }
}

/// Per-participant writer. One per pseudo_id. Owns the directory handle and
/// the next sequence number.
pub struct ParticipantCache {
    dir: PathBuf,
    next_seq: u32,
    max_bytes: u64,
}

impl ParticipantCache {
    /// Open (or create) the participant's cache sub-directory. Scans for
    /// existing `chunk_<seq>.pcm` files and resumes the seq counter after
    /// the highest-numbered file — supports the case where the buffer_task
    /// reuses a writer across a heal-driven `attach` cycle. On first call
    /// for a fresh participant the directory is empty and `next_seq = 0`.
    pub fn open(root: &BufferRoot, session_id: &str, pseudo_id: &str) -> io::Result<Self> {
        let dir = root.participant_dir(session_id, pseudo_id);
        std::fs::create_dir_all(&dir)?;
        let highest = max_existing_seq(&dir)?;
        let next_seq = if is_dir_empty(&dir)? { 0 } else { highest + 1 };
        Ok(Self {
            dir,
            next_seq,
            max_bytes: root.max_bytes_per_participant,
        })
    }

    /// Write a fresh chunk file. Returns the chunk's sequence number.
    /// Triggers oldest-first eviction if the per-participant cap is exceeded
    /// after the write.
    pub fn append_chunk(&mut self, bytes: &[u8]) -> io::Result<u32> {
        let seq = self.next_seq;
        let path = self.dir.join(format!("chunk_{:06}.pcm", seq));
        let mut f = std::fs::File::create(&path)?;
        f.write_all(bytes)?;
        f.sync_data().ok();
        self.next_seq = self.next_seq.saturating_add(1);
        self.enforce_cap()?;
        Ok(seq)
    }

    /// Enumerate on-disk chunks in strictly-ascending sequence order. Kept
    /// on the public surface for the test suite; production code uses
    /// [`flush_and_delete`] directly.
    #[allow(dead_code)]
    pub fn chunks_in_order(&self) -> io::Result<Vec<CachedChunk>> {
        list_chunks(&self.dir)
    }

    /// Remove every chunk and the participant's sub-directory. Idempotent.
    pub fn delete(&self) -> io::Result<()> {
        remove_dir_idempotent(&self.dir)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn enforce_cap(&mut self) -> io::Result<()> {
        loop {
            let mut total: u64 = 0;
            let entries = list_chunks(&self.dir)?;
            for e in &entries {
                total = total.saturating_add(e.bytes);
            }
            if total <= self.max_bytes || entries.is_empty() {
                return Ok(());
            }
            // Drop the oldest chunk, bump the metric, log WARN, loop.
            let oldest = &entries[0];
            warn!(
                path = %oldest.path.display(),
                dropped_seq = oldest.seq,
                cap_bytes = self.max_bytes,
                actual_bytes = total,
                "prerolled_chunk_evicted"
            );
            std::fs::remove_file(&oldest.path)?;
            metrics::counter!("chronicle_prerolled_chunks_dropped_total").increment(1);
        }
    }
}

/// One cached chunk on disk.
#[derive(Debug, Clone)]
pub struct CachedChunk {
    pub seq: u32,
    pub path: PathBuf,
    pub bytes: u64,
}

fn list_chunks(dir: &Path) -> io::Result<Vec<CachedChunk>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(seq) = parse_chunk_seq(&name) {
            let meta = entry.metadata()?;
            out.push(CachedChunk {
                seq,
                path: entry.path(),
                bytes: meta.len(),
            });
        }
    }
    out.sort_by_key(|c| c.seq);
    Ok(out)
}

fn max_existing_seq(dir: &Path) -> io::Result<u32> {
    let chunks = list_chunks(dir)?;
    Ok(chunks.last().map(|c| c.seq).unwrap_or(0))
}

fn parse_chunk_seq(name: &str) -> Option<u32> {
    let base = name.strip_prefix("chunk_")?.strip_suffix(".pcm")?;
    base.parse().ok()
}

fn is_dir_empty(dir: &Path) -> io::Result<bool> {
    Ok(std::fs::read_dir(dir)?.next().is_none())
}

fn remove_dir_idempotent(dir: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Walk the root directory and unlink any `<session_id>` subdir that exists —
/// used by `main.rs` at startup to clean crashed-run remnants.
pub fn sweep_on_startup(root: &Path) {
    if !root.exists() {
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, root = %root.display(), "buffer_sweep_read_dir_failed");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Err(e) = remove_dir_idempotent(&path) {
                warn!(error = %e, path = %path.display(), "buffer_sweep_remove_failed");
            } else {
                debug!(path = %path.display(), "buffer_sweep_removed_stale_session");
            }
        }
    }
}

/// Delete the entire session subtree. Idempotent.
pub fn delete_session(root: &BufferRoot, session_id: &str) -> io::Result<()> {
    remove_dir_idempotent(&root.session_dir(session_id))
}

/// Delete a participant's subtree. Idempotent.
#[allow(dead_code)]
pub fn delete_participant(
    root: &BufferRoot,
    session_id: &str,
    pseudo_id: &str,
) -> io::Result<()> {
    remove_dir_idempotent(&root.participant_dir(session_id, pseudo_id))
}

/// Count total bytes currently cached across all sessions/participants under
/// `root`. Used periodically by a spawn_blocking task to update
/// `chronicle_prerolled_chunks_cached_bytes`. Blocking — never call inside an
/// async context.
pub fn walk_total_bytes(root: &Path) -> io::Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    fn walk(dir: &Path, total: &mut u64) -> io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                walk(&entry.path(), total)?;
            } else if ft.is_file() {
                *total = total.saturating_add(entry.metadata()?.len());
            }
        }
        Ok(())
    }
    let mut total = 0u64;
    walk(root, &mut total)?;
    Ok(total)
}

/// Stream every cached chunk for (session_id, pseudo_id) to the data-api in
/// sequence order and then unlink the participant's directory. Happy-path
/// invocation after an Accept click.
pub async fn flush_and_delete(
    api: Arc<DataApiClient>,
    session_uuid: Uuid,
    pseudo_id: &str,
    dir: PathBuf,
) -> Result<usize, FlushError> {
    let chunks = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || list_chunks(&dir)
    })
    .await
    .map_err(|e| FlushError::Join(e.to_string()))??;

    let mut uploaded = 0usize;
    for chunk in &chunks {
        let bytes = tokio::task::spawn_blocking({
            let path = chunk.path.clone();
            move || std::fs::read(path)
        })
        .await
        .map_err(|e| FlushError::Join(e.to_string()))??;

        api.upload_chunk_with_retry(session_uuid, pseudo_id, bytes)
            .await
            .map_err(FlushError::Api)?;
        uploaded += 1;
    }

    tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || remove_dir_idempotent(&dir)
    })
    .await
    .map_err(|e| FlushError::Join(e.to_string()))??;

    Ok(uploaded)
}

/// Errors from the Accept-driven flush path.
#[derive(Debug, thiserror::Error)]
pub enum FlushError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("data-api: {0}")]
    Api(ApiError),
    #[error("spawn_blocking joined abnormally: {0}")]
    Join(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn root_with(max_secs: u64) -> (TempDir, BufferRoot) {
        let td = TempDir::new().unwrap();
        let root = BufferRoot::new(td.path().to_path_buf(), max_secs);
        (td, root)
    }

    #[test]
    fn append_then_list_preserves_sequence() {
        let (_td, root) = root_with(7200);
        let mut cache = ParticipantCache::open(&root, "sess1", "pseudo1").unwrap();
        assert_eq!(cache.append_chunk(b"aaa").unwrap(), 0);
        assert_eq!(cache.append_chunk(b"bbbb").unwrap(), 1);
        assert_eq!(cache.append_chunk(b"ccccc").unwrap(), 2);
        let chunks = cache.chunks_in_order().unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].seq, 0);
        assert_eq!(chunks[1].seq, 1);
        assert_eq!(chunks[2].seq, 2);
    }

    #[test]
    fn delete_removes_all_chunks() {
        let (_td, root) = root_with(7200);
        let mut cache = ParticipantCache::open(&root, "sess2", "pseudo2").unwrap();
        cache.append_chunk(b"aaa").unwrap();
        assert!(cache.dir().exists());
        cache.delete().unwrap();
        assert!(!cache.dir().exists());
        // Idempotent — second delete is fine.
        cache.delete().unwrap();
    }

    #[test]
    fn delete_participant_is_idempotent() {
        let (_td, root) = root_with(7200);
        delete_participant(&root, "sess-ghost", "pseudo-ghost").unwrap();
    }

    #[test]
    fn delete_session_removes_every_participant() {
        let (_td, root) = root_with(7200);
        let mut c1 = ParticipantCache::open(&root, "sess3", "p1").unwrap();
        c1.append_chunk(b"aaaa").unwrap();
        let mut c2 = ParticipantCache::open(&root, "sess3", "p2").unwrap();
        c2.append_chunk(b"bbbb").unwrap();
        assert!(root.session_dir("sess3").exists());
        delete_session(&root, "sess3").unwrap();
        assert!(!root.session_dir("sess3").exists());
    }

    #[test]
    fn oldest_chunk_drops_when_cap_exceeded() {
        // Tiny cap (10 bytes total) to exercise eviction without writing
        // gigabytes of zeros.
        let td = TempDir::new().unwrap();
        let root = BufferRoot {
            root: td.path().to_path_buf(),
            max_bytes_per_participant: 10,
        };
        let mut cache = ParticipantCache::open(&root, "sess4", "p1").unwrap();
        // Write 6 chunks of 4 bytes each = 24 bytes; should trim back to
        // at most 10 bytes (i.e. keeping only the last 2 chunks).
        for _ in 0..6 {
            cache.append_chunk(&[1u8, 2, 3, 4]).unwrap();
        }
        let remaining = cache.chunks_in_order().unwrap();
        assert!(remaining.len() <= 3, "remaining: {}", remaining.len());
        // The kept chunks must be the most recent ones (monotonically
        // increasing seq numbers near the end).
        let seqs: Vec<u32> = remaining.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, (6 - remaining.len() as u32..6).collect::<Vec<_>>());
    }

    #[test]
    fn sweep_on_startup_removes_stale_session_dirs() {
        let (td, root) = root_with(7200);
        let mut cache = ParticipantCache::open(&root, "old-sess", "p1").unwrap();
        cache.append_chunk(b"aaa").unwrap();
        drop(cache);
        assert!(root.session_dir("old-sess").exists());
        sweep_on_startup(td.path());
        assert!(!root.session_dir("old-sess").exists());
    }

    #[test]
    fn walk_total_bytes_sums_across_all_participants() {
        let (td, root) = root_with(7200);
        let mut c1 = ParticipantCache::open(&root, "s", "p1").unwrap();
        c1.append_chunk(&[0u8; 5]).unwrap();
        let mut c2 = ParticipantCache::open(&root, "s", "p2").unwrap();
        c2.append_chunk(&[0u8; 7]).unwrap();
        let total = walk_total_bytes(td.path()).unwrap();
        assert_eq!(total, 12);
    }

    #[test]
    fn chunks_in_order_preserves_sequence_after_sparse_writes() {
        // Simulates the flush-on-accept scan order: seq 0 written, seq 1
        // written, seq 2 written, listing must come back in 0-1-2 order
        // regardless of filesystem iteration order.
        let (_td, root) = root_with(7200);
        let mut c = ParticipantCache::open(&root, "sess", "p").unwrap();
        c.append_chunk(b"first").unwrap();
        c.append_chunk(b"second_payload").unwrap();
        c.append_chunk(b"third_even_longer").unwrap();
        let chunks = c.chunks_in_order().unwrap();
        assert_eq!(
            chunks.iter().map(|c| c.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // File sizes map back to the right seq.
        assert_eq!(chunks[0].bytes, b"first".len() as u64);
        assert_eq!(chunks[1].bytes, b"second_payload".len() as u64);
        assert_eq!(chunks[2].bytes, b"third_even_longer".len() as u64);
    }

    #[test]
    fn decline_path_deletes_participant_dir() {
        // ParticipantCache::delete mirrors what the actor does on Decline.
        let (_td, root) = root_with(7200);
        let mut c = ParticipantCache::open(&root, "sess-dec", "p").unwrap();
        c.append_chunk(b"bytes").unwrap();
        let dir = c.dir().to_path_buf();
        assert!(dir.exists());
        c.delete().unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn flush_order_matches_write_order() {
        // The flush-on-accept contract: cache stream order equals upload
        // order. We verify this by listing the chunks in sequence order
        // and comparing to the original write order.
        let (_td, root) = root_with(7200);
        let mut c = ParticipantCache::open(&root, "sess-flush", "p").unwrap();
        let payloads: Vec<Vec<u8>> = (0..5).map(|i| vec![i as u8; 10 + i]).collect();
        for p in &payloads {
            c.append_chunk(p).unwrap();
        }
        // Read chunks back via the public API used by flush_and_delete.
        let chunks = c.chunks_in_order().unwrap();
        assert_eq!(chunks.len(), payloads.len());
        for (i, chunk) in chunks.iter().enumerate() {
            let read_back = std::fs::read(&chunk.path).unwrap();
            assert_eq!(read_back, payloads[i], "chunk {i} roundtrip");
        }
    }
}
