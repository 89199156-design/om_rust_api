#!/usr/bin/env python3
"""Scan the working tree and reachable Git history without printing secret values."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import os
from pathlib import Path
import re
import subprocess
import sys


MAX_BYTES = 32 * 1024 * 1024


@dataclass(frozen=True)
class Rule:
    name: str
    pattern: re.Pattern[bytes]
    value_group: int | None = None


RULES = (
    Rule("private-key", re.compile(rb"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----")),
    Rule("aws-access-key", re.compile(rb"(?:AKIA|ASIA)[A-Z0-9]{16}")),
    Rule("github-token", re.compile(rb"(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,})")),
    Rule("openai-key", re.compile(rb"sk-[A-Za-z0-9_-]{20,}")),
    Rule("google-api-key", re.compile(rb"AIza[0-9A-Za-z_-]{35}")),
    Rule("slack-token", re.compile(rb"xox[baprs]-[A-Za-z0-9-]{10,}")),
    Rule(
        "credential-assignment",
        re.compile(
            rb"(?i)(?:api[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|"
            rb"password|passwd|secret)\s*[:=]\s*[\"']([^\"'\r\n]{8,})[\"']"
        ),
        1,
    ),
    Rule(
        "credential-url",
        re.compile(rb"(?i)\b(?:https?|postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis)://[^\s:/]+:([^\s/@]{8,})@"),
        1,
    ),
)

PLACEHOLDERS = (
    b"example", b"placeholder", b"replace", b"change-me", b"changeme",
    b"your-", b"your_", b"dummy", b"not-a-secret", b"top-secret", b"${", b"{{", b"<",
)


@dataclass(frozen=True)
class Finding:
    rule: str
    path: str
    line: int
    source: str


def git(repo: Path, *args: str, input_bytes: bytes | None = None) -> bytes:
    process = subprocess.run(
        ["git", "-C", os.fspath(repo), *args],
        input=input_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if process.returncode:
        raise RuntimeError(process.stderr.decode("utf-8", "replace").strip())
    return process.stdout


def placeholder(value: bytes) -> bool:
    normalized = value.strip().lower()
    if any(part in normalized for part in PLACEHOLDERS):
        return True
    if normalized in {b"password", b"secret", b"none", b"null", b"undefined"}:
        return True
    meaningful = bytes(char for char in normalized if chr(char).isalnum())
    return bool(meaningful) and len(set(meaningful)) <= 2


def scan(path: str, content: bytes, source: str) -> set[Finding]:
    if len(content) > MAX_BYTES or b"\0" in content[:8192]:
        return set()
    findings: set[Finding] = set()
    for rule in RULES:
        for match in rule.pattern.finditer(content):
            if rule.value_group is not None and placeholder(match.group(rule.value_group)):
                continue
            findings.add(Finding(rule.name, path, content.count(b"\n", 0, match.start()) + 1, source))
    return findings


def working_tree(repo: Path) -> list[tuple[str, bytes, str]]:
    paths = git(repo, "ls-files", "--cached", "--others", "--exclude-standard", "-z").split(b"\0")
    result = []
    for raw in paths:
        if not raw:
            continue
        name = raw.decode("utf-8", "surrogateescape")
        path = repo / name
        if path.is_file():
            result.append((name, path.read_bytes(), "working-tree"))
    return result


def staged(repo: Path) -> list[tuple[str, bytes, str]]:
    paths = git(repo, "diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR").split(b"\0")
    return [
        (raw.decode("utf-8", "surrogateescape"), git(repo, "show", f":{raw.decode('utf-8', 'surrogateescape')}"), "staged")
        for raw in paths if raw
    ]


def history(repo: Path) -> list[tuple[str, bytes, str]]:
    object_paths: dict[str, str] = {}
    object_ids = []
    for line in git(repo, "rev-list", "--objects", "--all").splitlines():
        raw_oid, _, raw_path = line.partition(b" ")
        oid = raw_oid.decode("ascii")
        object_ids.append(oid)
        if raw_path:
            object_paths.setdefault(oid, raw_path.decode("utf-8", "surrogateescape"))
    if not object_ids:
        return []
    checks = git(repo, "cat-file", "--batch-check", input_bytes=("\n".join(object_ids) + "\n").encode())
    blobs = []
    for line in checks.splitlines():
        fields = line.decode("ascii", "replace").split()
        if len(fields) >= 3 and fields[1] == "blob" and int(fields[2]) <= MAX_BYTES:
            blobs.append((fields[0], int(fields[2])))
    payload = git(repo, "cat-file", "--batch", input_bytes=("\n".join(oid for oid, _ in blobs) + "\n").encode())
    result = []
    cursor = 0
    for expected_oid, expected_size in blobs:
        header_end = payload.index(b"\n", cursor)
        header = payload[cursor:header_end].decode("ascii", "replace").split()
        if len(header) < 3 or header[0] != expected_oid or header[1] != "blob" or int(header[2]) != expected_size:
            raise RuntimeError("unexpected git cat-file response")
        start = header_end + 1
        end = start + expected_size
        result.append((object_paths.get(expected_oid, f"<blob:{expected_oid[:12]}>") , payload[start:end], f"history:{expected_oid[:12]}"))
        cursor = end + 1
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--staged", action="store_true")
    parser.add_argument("--history", action="store_true")
    args = parser.parse_args()
    repo = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").decode().strip())
    sources = staged(repo) if args.staged else working_tree(repo)
    if args.history:
        sources.extend(history(repo))
    findings: set[Finding] = set()
    for path, content, source in sources:
        findings.update(scan(path, content, source))
    if findings:
        print("Potential credentials detected; values are redacted:", file=sys.stderr)
        for item in sorted(findings, key=lambda value: (value.path, value.line, value.rule, value.source)):
            print(f"- rule={item.rule} path={item.path} line={item.line} source={item.source}", file=sys.stderr)
        print("Remove the value and rotate it if it was ever real or committed.", file=sys.stderr)
        return 1
    print("Secret scan passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
