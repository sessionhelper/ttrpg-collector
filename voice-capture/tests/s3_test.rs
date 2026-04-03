mod common;

use std::fs;
use std::path::Path;

use aws_sdk_s3::primitives::ByteStream;

use common::*;

#[tokio::test]
async fn test_upload_session_files() {
    let (client, bucket) = setup_s3().await;
    cleanup_s3(&client, &bucket).await;

    let session_id = fake_session_id();
    let dir = create_test_session_dir(&session_id);

    // Create test files
    let meta = serde_json::json!({
        "session_id": session_id,
        "started_at": "2026-01-01T00:00:00Z"
    });
    fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap()).unwrap();

    let consent = serde_json::json!({
        "session_id": session_id,
        "consent_version": "1.0"
    });
    fs::write(
        dir.join("consent.json"),
        serde_json::to_string_pretty(&consent).unwrap(),
    )
    .unwrap();

    // Create a small fake FLAC file (just some bytes — doesn't need to be valid audio)
    let audio_dir = dir.join("audio").join("abcdef0123456789");
    fs::create_dir_all(&audio_dir).unwrap();
    fs::write(audio_dir.join("chunk_0000.flac"), b"fake flac data for testing").unwrap();

    // Upload files using the raw S3 client (simulating what S3Uploader does)
    let guild_id = 123456u64;
    let files_to_upload = vec![
        (dir.join("meta.json"), "application/json"),
        (dir.join("consent.json"), "application/json"),
        (
            audio_dir.join("chunk_0000.flac"),
            "audio/flac",
        ),
    ];

    for (path, content_type) in &files_to_upload {
        let relative = path.strip_prefix(&dir).unwrap();
        let key = format!("sessions/{}/{}/{}", guild_id, session_id, relative.display());
        let body = ByteStream::from_path(path).await.unwrap();

        client
            .put_object()
            .bucket(&bucket)
            .key(&key)
            .content_type(*content_type)
            .body(body)
            .send()
            .await
            .unwrap();
    }

    // Verify objects exist
    let list = client
        .list_objects_v2()
        .bucket(&bucket)
        .prefix(format!("sessions/{}/{}/", guild_id, session_id))
        .send()
        .await
        .unwrap();

    let keys: Vec<String> = list
        .contents()
        .iter()
        .filter_map(|o| o.key().map(String::from))
        .collect();

    assert_eq!(keys.len(), 3);
    assert!(keys.iter().any(|k| k.ends_with("meta.json")));
    assert!(keys.iter().any(|k| k.ends_with("consent.json")));
    assert!(keys.iter().any(|k| k.ends_with("chunk_0000.flac")));

    // Cleanup
    cleanup_s3(&client, &bucket).await;
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_upload_skips_pcm() {
    let (client, bucket) = setup_s3().await;
    cleanup_s3(&client, &bucket).await;

    let session_id = fake_session_id();
    let dir = create_test_session_dir(&session_id);

    // Create a PCM file and a JSON file
    fs::write(dir.join("meta.json"), b"{}").unwrap();
    let pcm_dir = dir.join("pcm");
    fs::create_dir_all(&pcm_dir).unwrap();
    fs::write(pcm_dir.join("chunk_0000.pcm"), b"raw pcm data").unwrap();

    // Walk the directory like S3Uploader does, skipping .pcm
    let guild_id = 999u64;
    let mut uploaded_keys = Vec::new();

    for entry in walkdir_recursive(&dir) {
        if !entry.is_file() {
            continue;
        }
        // S3Uploader skips PCM files
        if entry.extension().is_some_and(|e| e == "pcm") {
            continue;
        }

        let relative = entry.strip_prefix(&dir).unwrap();
        let key = format!("sessions/{}/{}/{}", guild_id, session_id, relative.display());
        let body = ByteStream::from_path(&entry).await.unwrap();

        client
            .put_object()
            .bucket(&bucket)
            .key(&key)
            .body(body)
            .send()
            .await
            .unwrap();

        uploaded_keys.push(key);
    }

    // Only meta.json should have been uploaded, not the PCM file
    assert_eq!(uploaded_keys.len(), 1);
    assert!(uploaded_keys[0].ends_with("meta.json"));

    // Verify in S3
    let list = client
        .list_objects_v2()
        .bucket(&bucket)
        .prefix(format!("sessions/{}/{}/", guild_id, session_id))
        .send()
        .await
        .unwrap();

    let keys: Vec<String> = list
        .contents()
        .iter()
        .filter_map(|o| o.key().map(String::from))
        .collect();
    assert_eq!(keys.len(), 1);
    assert!(!keys.iter().any(|k| k.ends_with(".pcm")));

    cleanup_s3(&client, &bucket).await;
    fs::remove_dir_all(&dir).ok();
}

/// Simple recursive directory walk for tests.
fn walkdir_recursive(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir_recursive(&path));
            } else {
                files.push(path);
            }
        }
    }
    files
}
