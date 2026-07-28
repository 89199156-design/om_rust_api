from __future__ import annotations

from dataclasses import dataclass
from concurrent.futures import ThreadPoolExecutor
from concurrent.futures import as_completed
from http.client import HTTPConnection, HTTPException, HTTPSConnection, IncompleteRead, RemoteDisconnected
import os
from pathlib import Path
import socket
import threading
import time
from typing import BinaryIO, Iterable, Any
from urllib.error import HTTPError
from urllib.error import URLError
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

from .checksum import sha256_file

READ_RANGE_RETRY_DELAYS = (0.2, 1.0, 3.0)
RETRYABLE_HTTP_CODES = {429, 500, 502, 503, 504}
TRANSIENT_RANGE_ERRORS = (URLError, TimeoutError, ConnectionError, RemoteDisconnected, IncompleteRead)
TRANSIENT_CONNECTION_ERRORS = TRANSIENT_RANGE_ERRORS + (HTTPException, OSError)
_THREAD_LOCAL = threading.local()


class _RetryableRangeStatus(Exception):
    def __init__(self, status: int) -> None:
        self.status = status
        super().__init__(f"range request failed with HTTP {status}")


@dataclass(frozen=True)
class ByteRange:
    start: int
    end: int

    def __post_init__(self) -> None:
        if self.start < 0:
            raise ValueError("byte range start must be non-negative")
        if self.end < self.start:
            raise ValueError("byte range end must be greater than or equal to start")

    @property
    def length(self) -> int:
        return self.end - self.start + 1

    def as_header(self) -> str:
        return f"bytes={self.start}-{self.end}"

    def as_manifest(self) -> list[int]:
        return [self.start, self.end]


@dataclass(frozen=True)
class HttpObjectInfo:
    url: str
    content_length: int
    accept_ranges: bool


@dataclass(frozen=True)
class RangeFetchRequest:
    url: str
    byte_range: ByteRange
    remote_content_length: int | None = None


@dataclass(frozen=True)
class RangeFetchResult:
    index: int
    url: str
    byte_range: ByteRange
    payload: bytes


def _probe_http_object_once(url: str, timeout: int) -> HttpObjectInfo:
    request = Request(url, method="HEAD")
    with urlopen(request, timeout=timeout) as response:
        content_length = int(response.headers.get("Content-Length", "0"))
        accept_ranges = response.headers.get("Accept-Ranges", "").lower() == "bytes"
    return HttpObjectInfo(url=url, content_length=content_length, accept_ranges=accept_ranges)


def probe_http_object(url: str, timeout: int = 30) -> HttpObjectInfo:
    attempts = len(READ_RANGE_RETRY_DELAYS) + 1
    for attempt in range(attempts):
        try:
            return _probe_http_object_once(url, timeout)
        except HTTPError as exc:
            if exc.code not in RETRYABLE_HTTP_CODES or attempt == attempts - 1:
                raise ValueError(f"object probe failed with HTTP {exc.code}") from exc
        except TRANSIENT_RANGE_ERRORS as exc:
            if attempt == attempts - 1:
                raise ValueError(f"object probe failed after {attempts} attempts") from exc
        time.sleep(READ_RANGE_RETRY_DELAYS[attempt])
    raise ValueError("object probe failed")


def _normalise_ranges(ranges: Iterable[ByteRange]) -> list[ByteRange]:
    normalised = list(ranges)
    if not normalised:
        raise ValueError("at least one byte range is required")
    previous_end = -1
    for item in normalised:
        if item.start <= previous_end:
            raise ValueError("byte ranges must be sorted and non-overlapping")
        previous_end = item.end
    return normalised


def _refuses_full_object(ranges: list[ByteRange], content_length: int) -> bool:
    return len(ranges) == 1 and ranges[0].start == 0 and ranges[0].end == content_length - 1


def _connection_key(url: str) -> tuple[str, str, int]:
    parsed = urlsplit(url)
    if parsed.scheme not in {"http", "https"}:
        raise ValueError(f"unsupported URL scheme for range request: {parsed.scheme}")
    if not parsed.hostname:
        raise ValueError("range request URL is missing a host")
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    return parsed.scheme, parsed.hostname, port


def _connection_path(url: str) -> str:
    parsed = urlsplit(url)
    path = parsed.path or "/"
    if parsed.query:
        path = f"{path}?{parsed.query}"
    return path


def _close_thread_connection(key: tuple[str, str, int]) -> None:
    connections = getattr(_THREAD_LOCAL, "connections", None)
    if not connections:
        return
    connection = connections.pop(key, None)
    if connection is not None:
        connection.close()


def _thread_connection(url: str, timeout: int):
    key = _connection_key(url)
    connections = getattr(_THREAD_LOCAL, "connections", None)
    if connections is None:
        connections = {}
        _THREAD_LOCAL.connections = connections
    connection = connections.get(key)
    if connection is None:
        scheme, host, port = key
        cls = HTTPSConnection if scheme == "https" else HTTPConnection
        connection = cls(host, port, timeout=timeout)
        connections[key] = connection
    return key, connection


def _read_range_once(url: str, byte_range: ByteRange, timeout: int) -> bytes:
    key, connection = _thread_connection(url, timeout)
    deadline_expired = threading.Event()
    response_holder: list[Any] = []
    partial_payload: bytes | None = None
    status: int | None = None

    def abort_request() -> None:
        deadline_expired.set()
        response = response_holder[0] if response_holder else None
        raw = getattr(getattr(response, "fp", None), "raw", None)
        sock = connection.sock or getattr(raw, "_sock", None)
        if sock is not None:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                sock.close()
            except OSError:
                pass

    deadline = threading.Timer(timeout, abort_request)
    deadline.daemon = True
    deadline.start()
    try:
        connection.request(
            "GET",
            _connection_path(url),
            headers={"Range": byte_range.as_header(), "Connection": "keep-alive"},
        )
        response = connection.getresponse()
        response_holder.append(response)
        try:
            status = int(response.status)
            try:
                payload = response.read()
            except IncompleteRead as exc:
                partial_payload = bytes(exc.partial)
                _close_thread_connection(key)
                if status != 206 or not partial_payload:
                    raise
                payload = partial_payload
        finally:
            if response.getheader("Connection", "").lower() == "close":
                _close_thread_connection(key)
    except TRANSIENT_CONNECTION_ERRORS as exc:
        _close_thread_connection(key)
        if deadline_expired.is_set():
            raise TimeoutError(
                f"range request exceeded {timeout}s hard deadline: {byte_range.as_header()}"
            ) from exc
        raise
    except AttributeError as exc:
        _close_thread_connection(key)
        if deadline_expired.is_set():
            raise TimeoutError(
                f"range request exceeded {timeout}s hard deadline: {byte_range.as_header()}"
            ) from exc
        raise
    finally:
        deadline.cancel()

    if deadline_expired.is_set() and partial_payload is None:
        _close_thread_connection(key)
        raise TimeoutError(
            f"range request exceeded {timeout}s hard deadline: {byte_range.as_header()}"
        )

    if status in RETRYABLE_HTTP_CODES:
        _close_thread_connection(key)
        raise _RetryableRangeStatus(status)
    if status != 206:
        raise ValueError(f"range request returned HTTP {status}, expected 206")
    return payload


def _read_range(url: str, byte_range: ByteRange, timeout: int) -> bytes:
    attempts = len(READ_RANGE_RETRY_DELAYS) + 1
    payload_parts: list[bytes] = []
    remaining = byte_range
    for attempt in range(attempts):
        try:
            payload = _read_range_once(url, remaining, timeout)
            if len(payload) > remaining.length:
                raise ValueError(
                    f"range request returned {len(payload)} bytes, expected at most "
                    f"{remaining.length}"
                )
            if payload:
                payload_parts.append(payload)
                if len(payload) == remaining.length:
                    return b"".join(payload_parts)
                remaining = ByteRange(remaining.start + len(payload), remaining.end)
            elif attempt == attempts - 1:
                raise ValueError(
                    f"range request returned 0 bytes for {remaining.as_header()}"
                )
        except _RetryableRangeStatus as exc:
            if attempt == attempts - 1:
                raise ValueError(f"range request failed with HTTP {exc.status}") from exc
        except HTTPError as exc:
            if exc.code not in RETRYABLE_HTTP_CODES or attempt == attempts - 1:
                raise ValueError(f"range request failed with HTTP {exc.code}") from exc
        except TRANSIENT_CONNECTION_ERRORS as exc:
            if attempt == attempts - 1:
                raise ValueError(
                    f"range request failed after {attempts} attempts for {remaining.as_header()}"
                ) from exc
        if attempt < attempts - 1:
            time.sleep(READ_RANGE_RETRY_DELAYS[attempt])

    downloaded = sum(len(part) for part in payload_parts)
    raise ValueError(
        f"range request returned {downloaded} bytes, expected {byte_range.length} "
        f"after {attempts} attempts"
    )


def fetch_byte_range(url: str, start: int, end: int, *, timeout: int = 30) -> bytes:
    if end <= start:
        raise ValueError("byte range end must be greater than start")
    return _read_range(url, ByteRange(start, end - 1), timeout)


def fetch_byte_range_with_retry(
    url: str,
    byte_range: ByteRange,
    *,
    timeout: int = 30,
    remote_content_length: int | None = None,
) -> bytes:
    if remote_content_length is not None and byte_range.end >= remote_content_length:
        raise ValueError("byte range exceeds remote content length")
    return _read_range(url, byte_range, timeout)


def fetch_http_prefix_to_file(
    url: str,
    output_path: Path,
    byte_count: int,
    *,
    timeout: int = 30,
    chunk_size: int = 1024 * 1024,
    expected_content_length: int | None = None,
) -> int:
    """Fetch the first byte_count bytes with a plain HTTP 200 GET."""
    if byte_count <= 0:
        raise ValueError("byte_count must be positive")
    if chunk_size <= 0:
        raise ValueError("chunk_size must be positive")
    if expected_content_length is not None and byte_count > expected_content_length:
        raise ValueError("requested prefix exceeds remote content length")

    output_path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = output_path.with_name(output_path.name + ".tmp")
    request = Request(url)
    attempts = len(READ_RANGE_RETRY_DELAYS) + 1
    for attempt in range(attempts):
        temp_path.unlink(missing_ok=True)
        total = 0
        try:
            with urlopen(request, timeout=timeout) as response:
                if response.status != 200:
                    raise ValueError(
                        f"object request returned HTTP {response.status}, expected 200"
                    )
                content_length_header = response.headers.get("Content-Length")
                if content_length_header is not None:
                    content_length = int(content_length_header)
                    if byte_count > content_length:
                        raise ValueError("requested prefix exceeds response content length")
                with temp_path.open("wb") as file_obj:
                    remaining = byte_count
                    while remaining > 0:
                        chunk = response.read(min(chunk_size, remaining))
                        if not chunk:
                            raise ValueError(
                                f"object request ended after {total} bytes, expected {byte_count}"
                            )
                        file_obj.write(chunk)
                        total += len(chunk)
                        remaining -= len(chunk)
            if total != byte_count:
                raise ValueError(f"object request returned {total} bytes, expected {byte_count}")
            os.replace(temp_path, output_path)
            return total
        except HTTPError as exc:
            temp_path.unlink(missing_ok=True)
            if exc.code not in RETRYABLE_HTTP_CODES or attempt == attempts - 1:
                raise ValueError(f"object request failed with HTTP {exc.code}") from exc
        except TRANSIENT_RANGE_ERRORS as exc:
            temp_path.unlink(missing_ok=True)
            if attempt == attempts - 1:
                raise ValueError(f"object request failed after {attempts} attempts") from exc
        except Exception:
            temp_path.unlink(missing_ok=True)
            raise
        time.sleep(READ_RANGE_RETRY_DELAYS[attempt])
    raise ValueError("object request failed")


def download_ranges_concurrently(
    requests: Iterable[RangeFetchRequest],
    *,
    max_workers: int,
    timeout: int = 30,
) -> list[RangeFetchResult]:
    planned = list(requests)
    if not planned:
        return []
    if max_workers < 1:
        raise ValueError("max_workers must be at least 1")

    def fetch_one(index_and_request: tuple[int, RangeFetchRequest]) -> RangeFetchResult:
        index, item = index_and_request
        payload = fetch_byte_range_with_retry(
            item.url,
            item.byte_range,
            timeout=timeout,
            remote_content_length=item.remote_content_length,
        )
        return RangeFetchResult(index, item.url, item.byte_range, payload)

    if max_workers == 1:
        return [fetch_one(item) for item in enumerate(planned)]

    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        futures = [executor.submit(fetch_one, item) for item in enumerate(planned)]
        results = [future.result() for future in as_completed(futures)]
    return sorted(results, key=lambda item: item.index)


def copy_byte_range_to_file(
    url: str,
    byte_range: ByteRange,
    file_obj: BinaryIO,
    *,
    timeout: int = 30,
    chunk_size: int = 1024 * 1024,
) -> int:
    if chunk_size <= 0:
        raise ValueError("chunk_size must be positive")
    request = Request(url, headers={"Range": byte_range.as_header()})
    attempts = len(READ_RANGE_RETRY_DELAYS) + 1
    for attempt in range(attempts):
        position = file_obj.tell()
        total = 0
        try:
            with urlopen(request, timeout=timeout) as response:
                if response.status != 206:
                    raise ValueError(
                        f"range request returned HTTP {response.status}, expected 206"
                    )
                while True:
                    chunk = response.read(chunk_size)
                    if not chunk:
                        break
                    file_obj.write(chunk)
                    total += len(chunk)
            if total != byte_range.length:
                raise ValueError(
                    f"range request returned {total} bytes, expected {byte_range.length}"
                )
            return total
        except HTTPError as exc:
            file_obj.seek(position)
            file_obj.truncate()
            if exc.code not in RETRYABLE_HTTP_CODES or attempt == attempts - 1:
                raise ValueError(f"range request failed with HTTP {exc.code}") from exc
        except TRANSIENT_RANGE_ERRORS as exc:
            file_obj.seek(position)
            file_obj.truncate()
            if attempt == attempts - 1:
                raise ValueError(
                    f"range request failed after {attempts} attempts for {byte_range.as_header()}"
                ) from exc
        except Exception:
            file_obj.seek(position)
            file_obj.truncate()
            raise
        time.sleep(READ_RANGE_RETRY_DELAYS[attempt])
    raise ValueError(f"range request failed for {byte_range.as_header()}")


def download_byte_ranges(
    url: str,
    ranges: Iterable[ByteRange],
    output_path: Path,
    *,
    relative_to: Path | None = None,
    timeout: int = 30,
    allow_full_object: bool = False,
) -> dict[str, Any]:
    object_info = probe_http_object(url, timeout=timeout)
    normalised = _normalise_ranges(ranges)
    if not object_info.accept_ranges:
        raise ValueError("remote object does not advertise byte range support")
    if not allow_full_object and _refuses_full_object(normalised, object_info.content_length):
        raise ValueError("refusing full-object download through range adapter")
    for item in normalised:
        if item.end >= object_info.content_length:
            raise ValueError("byte range exceeds remote content length")

    output_path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = output_path.with_name(output_path.name + ".tmp")
    try:
        with temp_path.open("wb") as file_obj:
            for item in normalised:
                file_obj.write(_read_range(url, item, timeout))
        os.replace(temp_path, output_path)
    except Exception:
        temp_path.unlink(missing_ok=True)
        raise

    base = relative_to if relative_to is not None else output_path.parent
    relative_path = output_path.relative_to(base)
    size = output_path.stat().st_size
    return {
        "path": str(relative_path).replace("\\", "/"),
        "bytes": size,
        "sha256": sha256_file(output_path),
        "source_url": url,
        "byte_ranges": [item.as_manifest() for item in normalised],
        "remote_content_length": object_info.content_length,
        "downloaded_bytes": sum(item.length for item in normalised),
    }
