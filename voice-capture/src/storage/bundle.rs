use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::consent::manager::{ConsentSession, ConsentScope};

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

pub struct SessionBundle {
    pub session_id: String,
    pub session_dir: PathBuf,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub game_system: Option<String>,
    pub campaign_name: Option<String>,
    pub session_number: Option<u32>,
}

impl SessionBundle {
    pub fn new(session_id: String, base_dir: &str, guild_id: u64) -> Self {
        let session_dir = Path::new(base_dir)
            .join(guild_id.to_string())
            .join(&session_id);
        std::fs::create_dir_all(&session_dir).expect("Failed to create session dir");
        std::fs::create_dir_all(session_dir.join("audio")).expect("Failed to create audio dir");
        std::fs::create_dir_all(session_dir.join("pcm")).expect("Failed to create pcm dir");

        Self {
            session_id,
            session_dir,
            started_at: Utc::now(),
            ended_at: None,
            game_system: None,
            campaign_name: None,
            session_number: None,
        }
    }

    pub fn pcm_dir(&self) -> PathBuf {
        self.session_dir.join("pcm")
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.session_dir.join("audio")
    }

    /// Serialize session metadata to JSON bytes (for S3 upload).
    pub fn meta_json(&self, consent: &ConsentSession) -> Vec<u8> {
        let duration = self
            .ended_at
            .map(|e| (e - self.started_at).num_milliseconds() as f64 / 1000.0)
            .unwrap_or(0.0);

        let participants: Vec<ParticipantMeta> = consent
            .participants
            .values()
            .map(|p| {
                let pseudo = pseudonymize(p.user_id.get());
                ParticipantMeta {
                    pseudo_id: pseudo.clone(),
                    // Audio is stored as chunked PCM under audio/{pseudo_id}/
                    track_file: if p.scope == Some(ConsentScope::Full) {
                        Some(format!("audio/{}/", pseudo))
                    } else {
                        None
                    },
                    consent_scope: p.scope,
                }
            })
            .collect();

        let meta = SessionMeta {
            session_id: self.session_id.clone(),
            started_at: self.started_at,
            ended_at: self.ended_at,
            duration_seconds: duration,
            game_system: self.game_system.clone(),
            campaign_name: self.campaign_name.clone(),
            session_number: self.session_number,
            participant_count: consent.participants.len(),
            consented_audio_count: consent.consented_user_ids().len(),
            collector_version: env!("CARGO_PKG_VERSION").to_string(),
            audio_format: AudioFormat {
                sample_rate: 48000,
                bit_depth: 16,
                channels: 2,
                codec: "pcm_s16le".to_string(),
                container: "raw".to_string(),
            },
            participants,
        };

        serde_json::to_string_pretty(&meta)
            .expect("Failed to serialize meta")
            .into_bytes()
    }

    /// Serialize consent records to JSON bytes (for S3 upload).
    pub fn consent_json(&self, consent: &ConsentSession) -> Vec<u8> {
        let mut participants = HashMap::new();
        for p in consent.participants.values() {
            let pseudo = pseudonymize(p.user_id.get());
            participants.insert(
                pseudo,
                ConsentEntry {
                    consented_at: p.consented_at.map(|t: DateTime<Utc>| t.to_rfc3339()),
                    scope: p.scope,
                    audio_release: p.scope == Some(ConsentScope::Full),
                    mid_session_join: p.mid_session_join,
                },
            );
        }

        let record = ConsentRecord {
            session_id: self.session_id.clone(),
            consent_version: "1.0".to_string(),
            license: "CC BY-SA 4.0".to_string(),
            participants,
        };

        serde_json::to_string_pretty(&record)
            .expect("Failed to serialize consent")
            .into_bytes()
    }
}
