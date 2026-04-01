"""Tests for the DiskSink audio pipeline."""

from __future__ import annotations

from pathlib import Path

from collector.voice.sink import DiskSink, UserStream


class TestUserStream:
    def test_write_and_close(self, tmp_path: Path):
        pcm_path = tmp_path / "test.pcm"
        stream = UserStream(user_id=123, pcm_path=pcm_path)
        stream.open()
        stream.write(b"\x00" * 1024)
        assert stream.bytes_written == 1024
        stream.close()
        assert pcm_path.exists()
        assert pcm_path.stat().st_size == 1024

    def test_append_on_reconnect(self, tmp_path: Path):
        pcm_path = tmp_path / "test.pcm"
        stream = UserStream(user_id=123, pcm_path=pcm_path)

        stream.open()
        stream.write(b"\x01" * 512)
        stream.close()

        # Reopen (simulating reconnect)
        stream.open()
        stream.write(b"\x02" * 512)
        stream.close()

        assert pcm_path.stat().st_size == 1024

    def test_write_when_closed_is_noop(self, tmp_path: Path):
        pcm_path = tmp_path / "test.pcm"
        stream = UserStream(user_id=123, pcm_path=pcm_path)
        stream.write(b"\x00" * 100)  # file not opened
        assert stream.bytes_written == 0


class TestDiskSink:
    def test_write_consented_user(self, tmp_path: Path):
        sink = DiskSink(session_dir=tmp_path, consented_user_ids={100, 200})
        sink.write(b"\x00" * 512, 100)
        sink.write(b"\x00" * 256, 200)

        assert len(sink.active_streams) == 2
        assert sink.active_streams[100].bytes_written == 512
        assert sink.active_streams[200].bytes_written == 256

    def test_write_unconsented_user_ignored(self, tmp_path: Path):
        sink = DiskSink(session_dir=tmp_path, consented_user_ids={100})
        sink.write(b"\x00" * 512, 999)

        assert 999 not in sink.active_streams
        assert sink.total_bytes == 0

    def test_add_consented_user(self, tmp_path: Path):
        sink = DiskSink(session_dir=tmp_path, consented_user_ids={100})
        sink.write(b"\x00" * 100, 200)  # ignored
        assert sink.total_bytes == 0

        sink.add_consented_user(200)
        sink.write(b"\x00" * 100, 200)  # now recorded
        assert sink.total_bytes == 100

    def test_total_bytes(self, tmp_path: Path):
        sink = DiskSink(session_dir=tmp_path, consented_user_ids={100, 200})
        sink.write(b"\x00" * 100, 100)
        sink.write(b"\x00" * 200, 200)
        assert sink.total_bytes == 300

    def test_cleanup_closes_files(self, tmp_path: Path):
        sink = DiskSink(session_dir=tmp_path, consented_user_ids={100})
        sink.write(b"\x00" * 100, 100)
        sink.cleanup()
        assert sink.active_streams[100].file is None

    def test_pcm_files_created_in_pcm_dir(self, tmp_path: Path):
        sink = DiskSink(session_dir=tmp_path, consented_user_ids={100})
        sink.write(b"\x00" * 100, 100)
        sink.cleanup()
        assert (tmp_path / "pcm" / "100.pcm").exists()
