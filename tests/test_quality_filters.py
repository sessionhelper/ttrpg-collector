"""Tests for quality flag computation."""

from __future__ import annotations

from pathlib import Path

from collector.metadata.quality_filters import compute_quality_flags


class TestQualityFilters:
    def test_no_flags_for_good_session(self, tmp_path: Path):
        wav1 = tmp_path / "speaker1.wav"
        wav1.write_bytes(b"\x00" * 50000)
        wav2 = tmp_path / "speaker2.wav"
        wav2.write_bytes(b"\x00" * 50000)

        flags = compute_quality_flags(
            duration_seconds=3600,
            consented_count=4,
            wav_paths=[wav1, wav2],
        )
        assert flags == {}

    def test_short_session_flag(self, tmp_path: Path):
        wav = tmp_path / "speaker.wav"
        wav.write_bytes(b"\x00" * 50000)

        flags = compute_quality_flags(
            duration_seconds=30,
            consented_count=3,
            wav_paths=[wav],
        )
        assert "short_session" in flags
        assert flags["short_session"]["actual"] == 30

    def test_low_participants_flag(self, tmp_path: Path):
        wav = tmp_path / "speaker.wav"
        wav.write_bytes(b"\x00" * 50000)

        flags = compute_quality_flags(
            duration_seconds=3600,
            consented_count=1,
            wav_paths=[wav],
        )
        assert "low_participants" in flags

    def test_empty_tracks_flag(self, tmp_path: Path):
        wav = tmp_path / "speaker.wav"
        wav.write_bytes(b"\x00" * 100)  # tiny file

        flags = compute_quality_flags(
            duration_seconds=3600,
            consented_count=3,
            wav_paths=[wav],
        )
        assert "empty_tracks" in flags

    def test_minimal_audio_flag(self, tmp_path: Path):
        wav = tmp_path / "speaker.wav"
        wav.write_bytes(b"\x00" * 500)

        flags = compute_quality_flags(
            duration_seconds=3600,
            consented_count=3,
            wav_paths=[wav],
        )
        assert "minimal_audio" in flags

    def test_multiple_flags(self, tmp_path: Path):
        wav = tmp_path / "speaker.wav"
        wav.write_bytes(b"\x00" * 100)

        flags = compute_quality_flags(
            duration_seconds=10,
            consented_count=1,
            wav_paths=[wav],
        )
        assert "short_session" in flags
        assert "low_participants" in flags
        assert "empty_tracks" in flags
