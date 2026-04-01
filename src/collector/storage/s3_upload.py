"""S3-compatible upload with retry logic."""

from __future__ import annotations

import asyncio
from pathlib import Path

import aioboto3
import structlog

from collector.config import settings

log = structlog.get_logger()

MULTIPART_THRESHOLD = 100 * 1024 * 1024  # 100MB


CONTENT_TYPES = {
    ".json": "application/json",
    ".wav": "audio/wav",
    ".flac": "audio/flac",
    ".md": "text/markdown",
}


class S3Uploader:
    def __init__(self) -> None:
        self._session = aioboto3.Session()

    async def upload_session(
        self,
        session_dir: Path,
        guild_id: int,
        session_id: str,
        max_retries: int = 3,
    ) -> list[str]:
        """Upload all files in a session directory to S3. Returns list of uploaded keys."""
        if not settings.s3_access_key or not settings.s3_secret_key:
            log.warning("s3_not_configured", msg="Skipping upload — no S3 credentials")
            return []

        files = [f for f in session_dir.rglob("*") if f.is_file() and f.suffix != ".pcm"]
        uploaded = []

        async with self._session.client(
            "s3",
            endpoint_url=settings.s3_endpoint,
            aws_access_key_id=settings.s3_access_key,
            aws_secret_access_key=settings.s3_secret_key,
        ) as s3:
            for file_path in files:
                relative = file_path.relative_to(session_dir)
                key = f"sessions/{guild_id}/{session_id}/{relative}"
                content_type = CONTENT_TYPES.get(
                    file_path.suffix, "application/octet-stream"
                )

                for attempt in range(1, max_retries + 1):
                    try:
                        file_size = file_path.stat().st_size
                        if file_size > MULTIPART_THRESHOLD:
                            await self._multipart_upload(
                                s3, file_path, key, content_type
                            )
                        else:
                            await s3.upload_file(
                                str(file_path),
                                settings.s3_bucket,
                                key,
                                ExtraArgs={"ContentType": content_type},
                            )
                        uploaded.append(key)
                        log.info(
                            "file_uploaded",
                            key=key,
                            size=file_size,
                            attempt=attempt,
                        )
                        break
                    except Exception:
                        if attempt == max_retries:
                            log.error(
                                "upload_failed",
                                key=key,
                                attempts=max_retries,
                                exc_info=True,
                            )
                            raise
                        backoff = 2 ** (attempt - 1)
                        log.warning(
                            "upload_retry",
                            key=key,
                            attempt=attempt,
                            backoff=backoff,
                        )
                        await asyncio.sleep(backoff)

        return uploaded

    async def _multipart_upload(
        self, s3, file_path: Path, key: str, content_type: str
    ) -> None:
        """Upload large files using multipart upload."""
        from boto3.s3.transfer import TransferConfig

        config = TransferConfig(
            multipart_threshold=MULTIPART_THRESHOLD,
            multipart_chunksize=10 * 1024 * 1024,  # 10MB chunks
        )
        await s3.upload_file(
            str(file_path),
            settings.s3_bucket,
            key,
            ExtraArgs={"ContentType": content_type},
            Config=config,
        )
