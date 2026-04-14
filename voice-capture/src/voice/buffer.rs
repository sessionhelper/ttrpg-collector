//! Disk-backed per-speaker (and mixed-track) audio cache (F2, F8).
//!
//! Layout:
//!
//! ```text
//! $LOCAL_BUFFER_DIR/<session_id>/
//!   <pseudo_id>/                         # one dir per participant
//!     chunk_<seq:06>_<capture_ms>.pcm    # filename carries original capture time
//!   mixed/                               # reserved pseudo_id for F8 mix track
//!     chunk_<seq:06>_<capture_ms>.pcm
//! ```
//!
//! Every chunk filename embeds the millisecond-precision UTC timestamp at
//! which the first sample was captured. That timestamp is preserved across
//! the gate-flush — the bot POSTs it back to chronicle-data-api via the
//! `X-Capture-Started-At` header so the server side reconstructs the stream
//! as if it had been arriving live all along.
//!
//! Pre-gate the cache is authoritative for every participant. At gate-open
//! the actor walks each surviving subdirectory in sequence order and POSTs
//! the bytes to chronicle-data-api, then deletes the participant's subdir.
//! Declined subdirs are deleted without upload.
//!
//! Per-participant `LOCAL_BUFFER_MAX_SECS` cap triggers oldest-first
//! eviction to keep disk usage bounded.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::api_client::{ApiError, DataApiClient};

/// 48kHz × 16-bit × stereo = 192_000 bytes per second per participant.
pub const BYTES_PER_SECOND_48K_S16_STEREO: u64 = 48_000 * 2 * 2;

/// Reserved pseudo_id for the mixed track (F8).
pub const MIXED_PSEUDO_ID: &str = "mixed";

/// Roots the cache layout. Cheap to clone — every session handle carries one.
#[derive(Clone)]
pub struct BufferRoot {
    root: PathBuf,
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

    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    pub fn participant_dir(&self, session_id: &str, pseudo_id: &str) -> PathBuf {
        self.session_dir(session_id).join(pseudo_id)
    }
}

/// Per-participant writer. Owns the directory handle and next sequence number.
pub struct ParticipantCache {
    dir: PathBuf,
    next_seq: u32,
    max_bytes: u64,
}

impl ParticipantCache {
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

    /// Append a chunk with its capture-start timestamp. Filename encodes both
    /// the sequence number and the millisecond-precision timestamp. Returns
    /// the seq written.
    pub fn append_chunk(
        &mut self,
        bytes: &[u8],
        capture_started_at: DateTime<Utc>,
    ) -> io::Result<u32> {
        let seq = self.next_seq;
        let path = chunk_path(&self.dir, seq, capture_started_at);
        let mut f = std::fs::File::create(&path)?;
        f.write_all(bytes)?;
        f.sync_data().ok();
        self.next_seq = self.next_seq.saturating_add(1);
        self.enforce_cap()?;
        Ok(seq)
    }

    #[allow(dead_code)]
    pub fn chunks_in_order(&self) -> io::Result<Vec<CachedChunk>> {
        list_chunks(&self.dir)
    }

    pub fn delete(&self) -> io::Result<()> {
        remove_dir_idempotent(&self.dir)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn enforce_cap(&mut self) -> io::Result<()> {
        loop {
            let entries = list_chunks(&self.dir)?;
            let total: u64 = entries.iter().map(|c| c.bytes).sum();
            if total <= self.max_bytes || entries.is_empty() {
                return Ok(());
            }
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
    pub capture_started_at: DateTime<Utc>,
}

fn chunk_path(dir: &Path, seq: u32, capture_started_at: DateTime<Utc>) -> PathBuf {
    let ms = capture_started_at.timestamp_millis();
    dir.join(format!("chunk_{seq:06}_{ms}.pcm"))
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
        if let Some((seq, captured_ms)) = parse_chunk_name(&name) {
            let meta = entry.metadata()?;
            out.push(CachedChunk {
                seq,
                path: entry.path(),
                bytes: meta.len(),
                capture_started_at: ms_to_datetime(captured_ms),
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

fn parse_chunk_name(name: &str) -> Option<(u32, i64)> {
    let base = name.strip_prefix("chunk_")?.strip_suffix(".pcm")?;
    let mut parts = base.splitn(2, '_');
    let seq = parts.next()?.parse::<u32>().ok()?;
    let ms = parts.next()?.parse::<i64>().ok()?;
    Some((seq, ms))
}

fn ms_to_datetime(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
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

pub fn delete_session(root: &BufferRoot, session_id: &str) -> io::Result<()> {
    remove_dir_idempotent(&root.session_dir(session_id))
}

#[allow(dead_code)]
pub fn delete_participant(
    root: &BufferRoot,
    session_id: &str,
    pseudo_id: &str,
) -> io::Result<()> {
    remove_dir_idempotent(&root.participant_dir(session_id, pseudo_id))
}

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
/// sequence order with its original capture timestamp, then unlink the dir.
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

        let client_chunk_id = format!("{session_uuid}:{pseudo_id}:{}", chunk.seq);
        let duration_ms = pcm_bytes_to_duration_ms(bytes.len());
        api.upload_chunk_with_retry(
            session_uuid,
            pseudo_id,
            bytes,
            chunk.capture_started_at,
            duration_ms,
            &client_chunk_id,
        )
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

/// 48kHz stereo s16le: 192_000 bytes/sec → ms = bytes / 192.
pub fn pcm_bytes_to_duration_ms(bytes: usize) -> u64 {
    (bytes as u64).saturating_mul(1000) / BYTES_PER_SECOND_48K_S16_STEREO.max(1)
}

/// Errors from the gate-flush path.
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

    fn now_plus_ms(offset_ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(1_700_000_000_000 + offset_ms)
            .single()
            .unwrap()
    }

    #[test]
    fn append_then_list_preserves_sequence_and_timestamps() {
        let (_td, root) = root_with(7200);
        let mut cache = ParticipantCache::open(&root, "sess1", "pseudo1").unwrap();
        let ts0 = now_plus_ms(0);
        let ts1 = now_plus_ms(100);
        let ts2 = now_plus_ms(250);
        assert_eq!(cache.append_chunk(b"aaa", ts0).unwrap(), 0);
        assert_eq!(cache.append_chunk(b"bbbb", ts1).unwrap(), 1);
        assert_eq!(cache.append_chunk(b"ccccc", ts2).unwrap(), 2);
        let chunks = cache.chunks_in_order().unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].seq, 0);
        assert_eq!(chunks[0].capture_started_at, ts0);
        assert_eq!(chunks[1].capture_started_at, ts1);
        assert_eq!(chunks[2].capture_started_at, ts2);
    }

    #[test]
    fn delete_removes_all_chunks() {
        let (_td, root) = root_with(7200);
        let mut cache = ParticipantCache::open(&root, "sess2", "pseudo2").unwrap();
        cache.append_chunk(b"aaa", now_plus_ms(0)).unwrap();
        assert!(cache.dir().exists());
        cache.delete().unwrap();
        assert!(!cache.dir().exists());
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
        c1.append_chunk(b"aaaa", now_plus_ms(0)).unwrap();
        let mut c2 = ParticipantCache::open(&root, "sess3", "p2").unwrap();
        c2.append_chunk(b"bbbb", now_plus_ms(0)).unwrap();
        assert!(root.session_dir("sess3").exists());
        delete_session(&root, "sess3").unwrap();
        assert!(!root.session_dir("sess3").exists());
    }

    #[test]
    fn oldest_chunk_drops_when_cap_exceeded() {
        let td = TempDir::new().unwrap();
        let root = BufferRoot {
            root: td.path().to_path_buf(),
            max_bytes_per_participant: 10,
        };
        let mut cache = ParticipantCache::open(&root, "sess4", "p1").unwrap();
        for i in 0..6 {
            cache
                .append_chunk(&[1u8, 2, 3, 4], now_plus_ms(i * 10))
                .unwrap();
        }
        let remaining = cache.chunks_in_order().unwrap();
        assert!(remaining.len() <= 3, "remaining: {}", remaining.len());
        let seqs: Vec<u32> = remaining.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, (6 - remaining.len() as u32..6).collect::<Vec<_>>());
    }

    #[test]
    fn sweep_on_startup_removes_stale_session_dirs() {
        let (td, root) = root_with(7200);
        let mut cache = ParticipantCache::open(&root, "old-sess", "p1").unwrap();
        cache.append_chunk(b"aaa", now_plus_ms(0)).unwrap();
        drop(cache);
        assert!(root.session_dir("old-sess").exists());
        sweep_on_startup(td.path());
        assert!(!root.session_dir("old-sess").exists());
    }

    #[test]
    fn walk_total_bytes_sums_across_all_participants() {
        let (td, root) = root_with(7200);
        let mut c1 = ParticipantCache::open(&root, "s", "p1").unwrap();
        c1.append_chunk(&[0u8; 5], now_plus_ms(0)).unwrap();
        let mut c2 = ParticipantCache::open(&root, "s", "p2").unwrap();
        c2.append_chunk(&[0u8; 7], now_plus_ms(0)).unwrap();
        let total = walk_total_bytes(td.path()).unwrap();
        assert_eq!(total, 12);
    }

    #[test]
    fn chunk_filename_roundtrips_seq_and_timestamp() {
        let ts = now_plus_ms(1_234);
        let name = format!("chunk_{:06}_{}.pcm", 42, ts.timestamp_millis());
        let (seq, ms) = parse_chunk_name(&name).unwrap();
        assert_eq!(seq, 42);
        assert_eq!(ms, ts.timestamp_millis());
    }

    #[test]
    fn pcm_bytes_to_duration_ms_handles_common_sizes() {
        assert_eq!(pcm_bytes_to_duration_ms(192_000), 1_000);
        assert_eq!(pcm_bytes_to_duration_ms(96_000), 500);
        assert_eq!(pcm_bytes_to_duration_ms(0), 0);
    }
}
