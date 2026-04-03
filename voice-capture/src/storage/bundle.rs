use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// What a participant chose when presented with the consent prompt.
/// Defined here (in the lib crate) so both the binary's session module
/// and the storage serialization structs can use it without circular deps.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentScope {
    Full,
    DeclineAudio,
    Decline,
}

pub fn pseudonymize(user_id: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(user_id.to_string().as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..8]) // 16 hex chars
}

#[derive(Serialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_seconds: f64,
    pub game_system: Option<String>,
    pub campaign_name: Option<String>,
    pub session_number: Option<u32>,
    pub participant_count: usize,
    pub consented_audio_count: usize,
    pub collector_version: String,
    pub audio_format: AudioFormat,
    pub participants: Vec<ParticipantMeta>,
}

#[derive(Serialize)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u8,
    pub codec: String,
    pub container: String,
}

#[derive(Serialize)]
pub struct ParticipantMeta {
    pub pseudo_id: String,
    pub track_file: Option<String>,
    pub consent_scope: Option<ConsentScope>,
}

#[derive(Serialize)]
pub struct ConsentRecord {
    pub session_id: String,
    pub consent_version: String,
    pub license: String,
    pub participants: HashMap<String, ConsentEntry>,
}

#[derive(Serialize)]
pub struct ConsentEntry {
    pub consented_at: Option<String>,
    pub scope: Option<ConsentScope>,
    pub audio_release: bool,
    pub mid_session_join: bool,
}

// SessionBundle has been replaced by Session::meta_json() and Session::consent_json().
// The serialization structs above are still used by Session.
