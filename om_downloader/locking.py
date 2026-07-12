from __future__ import annotations

from contextlib import contextmanager
from datetime import datetime, timezone
import os
from pathlib import Path
import re
import time


_LOCK_PID_PATTERN = re.compile(r"(?:^|\s)pid=(\d+)(?:\s|$)")
_INCOMPLETE_LOCK_GRACE_SECONDS = 30


def _process_exists(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _existing_lock_is_active(path: Path) -> bool:
    try:
        payload = path.read_text(encoding="utf-8")
        modified_at = path.stat().st_mtime
    except FileNotFoundError:
        return False
    match = _LOCK_PID_PATTERN.search(payload)
    if match:
        return _process_exists(int(match.group(1)))
    return time.time() - modified_at < _INCOMPLETE_LOCK_GRACE_SECONDS


@contextmanager
def file_lock(path: Path):
    path.parent.mkdir(parents=True, exist_ok=True)
    flags = os.O_CREAT | os.O_EXCL | os.O_WRONLY
    fd = None
    for _attempt in range(2):
        try:
            fd = os.open(path, flags)
            break
        except FileExistsError as exc:
            if _existing_lock_is_active(path):
                raise RuntimeError(f"task already running, lock exists: {path}") from exc
            try:
                path.unlink()
            except FileNotFoundError:
                pass
    if fd is None:
        raise RuntimeError(f"task already running, lock exists: {path}")

    try:
        payload = f"pid={os.getpid()} acquired_at={datetime.now(timezone.utc).isoformat()}\n"
        with os.fdopen(fd, "w", encoding="utf-8") as file_obj:
            file_obj.write(payload)
        yield
    finally:
        try:
            path.unlink()
        except FileNotFoundError:
            pass
