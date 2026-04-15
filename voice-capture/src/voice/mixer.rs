//! Mix track producer (F8).
//!
//! Two distinct mix paths, one per phase:
//!
//! - **Pre-gate**: mix is NOT computed incrementally. Pre-gate frames flow
//!   only into per-speaker caches. At gate-open the actor invokes
//!   [`render_mix_from_caches`] on the surviving (non-Declined) per-speaker
//!   caches to produce a mix cache ready to flush.
//! - **Post-gate**: a live mix accumulator task receives every AudioPacket
//!   from every Accepted speaker, sums them into 20ms tick windows, and
//!   writes 2MB rollover chunks straight to the data-api. Late mid-session
//!   joiners whose Accept click happens post-gate are mixed in from
//!   accept-time forward only — their pre-accept frames do not retroactively
//!   appear in the mix (their per-speaker track captures them faithfully).
//!
//! All mix chunks are uploaded under the reserved pseudo_id "mixed".

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};

use crate::voice::buffer::{BufferRoot, MIXED_PSEUDO_ID};

/// 48kHz stereo s16le → 192,000 bytes/sec.
pub const BYTES_PER_SEC: usize = 48_000 * 2 * 2;

/// Target bytes per emitted mix chunk (matches per-speaker chunk size).
pub const MIX_CHUNK_SIZE_BYTES: usize = 2 * 1024 * 1024;

/// A rendered mix chunk ready to upload.
#[derive(Debug, Clone)]
pub struct MixChunk {
    pub seq: u32,
    pub capture_started_at: DateTime<Utc>,
    pub bytes: Vec<u8>,
}

/// Source input for `render_mix_from_caches`: one surviving per-speaker
/// cache. `chunks` are the on-disk chunks in sequence order.
pub struct PerSpeakerSource {
    pub chunks: Vec<SourceChunk>,
}

pub struct SourceChunk {
    pub capture_started_at: DateTime<Utc>,
    pub bytes: Vec<u8>,
}

/// One-shot mix render from surviving per-speaker caches, using their
/// original capture timestamps. Returns the full mix as a sequence of
/// 2MB `MixChunk`s, each with its own `capture_started_at`.
///
/// Algorithm (simple, correct, byte-exact on aligned streams):
///
/// 1. Find the earliest `capture_started_at` across all input chunks → `t0`.
/// 2. Find the latest `capture_started_at + duration` → `t_end`.
/// 3. Allocate a mix buffer of `(t_end - t0)` duration, initialized to zero.
/// 4. For each input chunk, compute its byte offset in the mix buffer
///    (`(capture_started_at - t0) * BYTES_PER_SEC`), then saturating-add
///    its samples into the mix buffer.
/// 5. Split the mix buffer into `MIX_CHUNK_SIZE_BYTES` chunks; each chunk's
///    `capture_started_at` = `t0 + offset / BYTES_PER_SEC`.
///
/// Returns an empty Vec if there is no input audio.
pub fn render_mix_from_caches(sources: &[PerSpeakerSource]) -> Vec<MixChunk> {
    let mut any_chunk = false;
    let mut t0: Option<DateTime<Utc>> = None;
    let mut t_end: Option<DateTime<Utc>> = None;
    for s in sources {
        for c in &s.chunks {
            any_chunk = true;
            let dur = ChronoDuration::milliseconds(bytes_to_ms(c.bytes.len()) as i64);
            let start = c.capture_started_at;
            let end = start + dur;
            t0 = Some(t0.map_or(start, |v| v.min(start)));
            t_end = Some(t_end.map_or(end, |v| v.max(end)));
        }
    }
    if !any_chunk {
        return Vec::new();
    }
    let t0 = t0.unwrap();
    let t_end = t_end.unwrap();
    let total_ms = (t_end - t0).num_milliseconds().max(0) as usize;
    let total_bytes = ms_to_bytes(total_ms);
    // Pad to even sample boundary (2 bytes per mono sample, 4 bytes stereo).
    let total_bytes = total_bytes - (total_bytes % 4);
    let mut mix = vec![0i16; total_bytes / 2];

    for s in sources {
        for c in &s.chunks {
            let offset_bytes = ms_to_bytes(
                (c.capture_started_at - t0).num_milliseconds().max(0) as usize,
            );
            let offset_samples = offset_bytes / 2;
            sum_bytes_into_mix(&c.bytes, &mut mix, offset_samples);
        }
    }

    let mix_bytes = samples_to_bytes(&mix);
    split_mix_into_chunks(&mix_bytes, t0)
}

fn split_mix_into_chunks(mix_bytes: &[u8], t0: DateTime<Utc>) -> Vec<MixChunk> {
    let mut out = Vec::new();
    let mut seq: u32 = 0;
    let mut offset = 0usize;
    while offset < mix_bytes.len() {
        let end = (offset + MIX_CHUNK_SIZE_BYTES).min(mix_bytes.len());
        // Sample alignment: never cut a stereo sample frame in half.
        let end = end - (end % 4);
        if end <= offset {
            break;
        }
        let chunk_offset_ms = bytes_to_ms(offset);
        let capture_started_at = t0 + ChronoDuration::milliseconds(chunk_offset_ms as i64);
        out.push(MixChunk {
            seq,
            capture_started_at,
            bytes: mix_bytes[offset..end].to_vec(),
        });
        seq += 1;
        offset = end;
    }
    out
}

fn sum_bytes_into_mix(src: &[u8], mix: &mut [i16], start_sample: usize) {
    let mut i = start_sample;
    let mut j = 0;
    while j + 1 < src.len() && i < mix.len() {
        let s = i16::from_le_bytes([src[j], src[j + 1]]);
        mix[i] = mix[i].saturating_add(s);
        i += 1;
        j += 2;
    }
}

fn samples_to_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// 192 bytes = 1 ms of 48kHz stereo PCM.
pub fn ms_to_bytes(ms: usize) -> usize {
    ms.saturating_mul(BYTES_PER_SEC) / 1000
}

pub fn bytes_to_ms(bytes: usize) -> usize {
    bytes.saturating_mul(1000) / BYTES_PER_SEC.max(1)
}

/// Write out a set of pre-gate mix chunks into the mix cache directory at
/// `root/<session_id>/mixed/`. Filenames encode seq + capture_started_at,
/// so that the common flush path (`buffer::flush_and_delete`) picks them up
/// just like per-speaker chunks. Synchronous — caller should invoke from
/// `spawn_blocking` if the set is large.
pub fn write_mix_chunks_to_cache(
    root: &BufferRoot,
    session_id: &str,
    mix_chunks: &[MixChunk],
) -> std::io::Result<()> {
    let dir = root.participant_dir(session_id, MIXED_PSEUDO_ID);
    std::fs::create_dir_all(&dir)?;
    for c in mix_chunks {
        write_mix_chunk(&dir, c)?;
    }
    Ok(())
}

fn write_mix_chunk(dir: &Path, chunk: &MixChunk) -> std::io::Result<()> {
    let ms = chunk.capture_started_at.timestamp_millis();
    let path = dir.join(format!("chunk_{:06}_{}.pcm", chunk.seq, ms));
    std::fs::write(path, &chunk.bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Post-gate live mix accumulator (used by actor's MixTask)
// ---------------------------------------------------------------------------

/// Live mix accumulator. Post-gate, every AudioPacket from an Accepted
/// speaker funnels through `submit`; once `roll_over_bytes` is reached the
/// accumulated bytes are handed back as a `MixChunk` ready to upload.
///
/// "Tick bucketing": songbird delivers one VoiceTick per 20ms, containing
/// samples for every currently-speaking user on that tick. We bucket by the
/// local wall-clock of frame arrival at a resolution finer than a tick —
/// any frames arriving within `bucket_tolerance_ms` of each other are
/// considered to belong to the same tick and sum into one output frame.
pub struct LiveMixAccum {
    bucket: BTreeMap<i64, Vec<i16>>,
    max_samples_per_bucket: usize,
    /// Bytes pending inside the rollover window. Flush when exceeds target.
    roll_over_bytes: usize,
    /// Flushed mono-stereo PCM bytes waiting to emit as a chunk.
    staged: Vec<u8>,
    /// `capture_started_at` of the first byte in `staged`.
    staged_start: Option<DateTime<Utc>>,
    next_seq: u32,
}

impl LiveMixAccum {
    pub fn new() -> Self {
        Self {
            bucket: BTreeMap::new(),
            max_samples_per_bucket: 48_000 * 2, // 1 second worth
            roll_over_bytes: MIX_CHUNK_SIZE_BYTES,
            staged: Vec::with_capacity(MIX_CHUNK_SIZE_BYTES),
            staged_start: None,
            next_seq: 0,
        }
    }

    /// Submit a frame from an Accepted speaker at wall-clock `now`. Returns
    /// a rollover `MixChunk` if the staged buffer filled up.
    pub fn submit(
        &mut self,
        now: DateTime<Utc>,
        samples: &[i16],
    ) -> Option<MixChunk> {
        let key = now.timestamp_millis() / 20 * 20; // 20ms tick bucket
        let cap = self.max_samples_per_bucket;
        let bucket = self
            .bucket
            .entry(key)
            .or_insert_with(|| Vec::with_capacity(samples.len().min(cap)));
        sum_into_bucket(bucket, samples, cap);

        self.drain_ready(now)
    }

    /// Drain any buckets older than `now - 100ms` (they're done accumulating)
    /// into the staged buffer and emit a chunk if staged is full.
    pub fn drain_ready(&mut self, now: DateTime<Utc>) -> Option<MixChunk> {
        let cutoff = now.timestamp_millis() - 100;
        let mut ready_keys = Vec::new();
        for &k in self.bucket.keys() {
            if k <= cutoff {
                ready_keys.push(k);
            }
        }
        for k in ready_keys {
            if let Some(samples) = self.bucket.remove(&k) {
                let t = Utc.timestamp_millis_opt(k).single().unwrap_or(now);
                if self.staged_start.is_none() {
                    self.staged_start = Some(t);
                }
                append_samples(&mut self.staged, &samples);
            }
        }
        if self.staged.len() >= self.roll_over_bytes {
            return Some(self.emit_staged_chunk());
        }
        None
    }

    /// Flush any buffered bytes into a final `MixChunk`. Returns `None` if
    /// nothing was staged.
    pub fn finish(&mut self) -> Option<MixChunk> {
        if let Some(_st) = self.staged_start {
            // Drain remaining buckets before finalizing.
            let remaining_keys: Vec<i64> = self.bucket.keys().copied().collect();
            for k in remaining_keys {
                if let Some(samples) = self.bucket.remove(&k) {
                    let t = Utc.timestamp_millis_opt(k).single();
                    if self.staged_start.is_none()
                        && let Some(t) = t {
                            self.staged_start = Some(t);
                        }
                    append_samples(&mut self.staged, &samples);
                }
            }
        }
        if self.staged.is_empty() {
            return None;
        }
        Some(self.emit_staged_chunk())
    }

    fn emit_staged_chunk(&mut self) -> MixChunk {
        let start = self.staged_start.unwrap_or_else(Utc::now);
        let bytes = std::mem::take(&mut self.staged);
        self.staged_start = None;
        let seq = self.next_seq;
        self.next_seq += 1;
        MixChunk {
            seq,
            capture_started_at: start,
            bytes,
        }
    }
}

impl Default for LiveMixAccum {
    fn default() -> Self {
        Self::new()
    }
}

fn sum_into_bucket(bucket: &mut Vec<i16>, samples: &[i16], cap: usize) {
    if bucket.len() < samples.len() {
        bucket.resize(samples.len().min(cap), 0);
    }
    for (i, s) in samples.iter().enumerate() {
        if i >= bucket.len() {
            break;
        }
        bucket[i] = bucket[i].saturating_add(*s);
    }
}

fn append_samples(out: &mut Vec<u8>, samples: &[i16]) {
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(offset_ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(1_700_000_000_000 + offset_ms)
            .single()
            .unwrap()
    }

    /// Build a PCM byte stream from a constant i16 value, `samples` stereo frames long.
    fn pcm_with_value(samples: usize, value: i16) -> Vec<u8> {
        let mut v = Vec::with_capacity(samples * 4);
        for _ in 0..samples {
            v.extend_from_slice(&value.to_le_bytes());
            v.extend_from_slice(&value.to_le_bytes());
        }
        v
    }

    #[test]
    fn render_mix_empty_input_yields_no_chunks() {
        assert!(render_mix_from_caches(&[]).is_empty());
        let s = PerSpeakerSource { chunks: vec![] };
        assert!(render_mix_from_caches(&[s]).is_empty());
    }

    #[test]
    fn render_mix_sums_two_aligned_streams() {
        // Two 1-second chunks that start at the same time produce a mixed
        // output whose samples are the saturating sum.
        let a = pcm_with_value(48_000, 100);
        let b = pcm_with_value(48_000, 200);
        let src_a = PerSpeakerSource {
            chunks: vec![SourceChunk {
                capture_started_at: ts(0),
                bytes: a,
            }],
        };
        let src_b = PerSpeakerSource {
            chunks: vec![SourceChunk {
                capture_started_at: ts(0),
                bytes: b,
            }],
        };
        let out = render_mix_from_caches(&[src_a, src_b]);
        assert!(!out.is_empty());
        // First mix chunk should start at ts(0).
        assert_eq!(out[0].capture_started_at, ts(0));
        // First sample frame: two LE i16s, each == 300 (saturating).
        let first = i16::from_le_bytes([out[0].bytes[0], out[0].bytes[1]]);
        assert_eq!(first, 300);
    }

    #[test]
    fn render_mix_offsets_laggy_streams_by_timestamp() {
        // Speaker A starts at t=0 with 500ms of 100s.
        // Speaker B starts at t=500ms with 500ms of 200s.
        // Mix should be: first 500ms = 100, next 500ms = 200.
        let a = pcm_with_value(24_000, 100); // 500ms
        let b = pcm_with_value(24_000, 200);
        let src_a = PerSpeakerSource {
            chunks: vec![SourceChunk {
                capture_started_at: ts(0),
                bytes: a,
            }],
        };
        let src_b = PerSpeakerSource {
            chunks: vec![SourceChunk {
                capture_started_at: ts(500),
                bytes: b,
            }],
        };
        let out = render_mix_from_caches(&[src_a, src_b]);
        assert!(!out.is_empty());
        let all: Vec<u8> = out.iter().flat_map(|c| c.bytes.clone()).collect();
        // Sample at t=0ms → 100. Sample at t=500ms → 200.
        let at0 = i16::from_le_bytes([all[0], all[1]]);
        let at500ms_offset = (500 * BYTES_PER_SEC) / 1000;
        let at500 = i16::from_le_bytes([all[at500ms_offset], all[at500ms_offset + 1]]);
        assert_eq!(at0, 100);
        assert_eq!(at500, 200);
    }

    #[test]
    fn live_mix_accum_rolls_over_on_size() {
        let mut acc = LiveMixAccum {
            bucket: BTreeMap::new(),
            max_samples_per_bucket: 48_000 * 2,
            roll_over_bytes: 400, // small rollover for test
            staged: Vec::new(),
            staged_start: None,
            next_seq: 0,
        };
        // Submit a burst so staged crosses 400 bytes.
        let samples = vec![10i16; 200]; // 400 bytes
        let out = acc.submit(ts(0), &samples);
        // Drain ready: empty because the tick is "now". Force drain with a later now.
        assert!(out.is_none());
        let later = ts(500);
        let out2 = acc.drain_ready(later);
        assert!(out2.is_some());
        let mc = out2.unwrap();
        assert_eq!(mc.seq, 0);
    }

    #[test]
    fn live_mix_sums_simultaneous_speakers() {
        // Two speakers submit at the same tick — output should be the sum.
        let mut acc = LiveMixAccum {
            bucket: BTreeMap::new(),
            max_samples_per_bucket: 48_000 * 2,
            roll_over_bytes: 4, // smaller than 8 bytes per submit so we trigger
            staged: Vec::new(),
            staged_start: None,
            next_seq: 0,
        };
        let t0 = ts(0);
        acc.submit(t0, &[10i16, 10, 10, 10]);
        acc.submit(t0, &[5i16, 5, 5, 5]);
        let later = ts(500);
        let out = acc.drain_ready(later).unwrap();
        let first = i16::from_le_bytes([out.bytes[0], out.bytes[1]]);
        assert_eq!(first, 15);
    }
}
