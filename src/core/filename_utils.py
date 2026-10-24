"""Filename sanitisation, derivation from URLs and collision-policy paths.

Windows-hostile names are tamed here. Path-traversal defence happens in
:func:`resolve_destination`, which refuses any destination that escapes the
requested directory.
"""

from __future__ import annotations

import pathlib
import re
import urllib.parse

__all__ = [
    "MAX_FILENAME_LEN",
    "FALLBACK_NAME",
    "classify_extension",
    "filename_from_url",
    "is_reserved_windows_name",
    "resolve_destination",
    "sanitize_filename",
    "unique_path",
]

MAX_FILENAME_LEN = 200
FALLBACK_NAME = "download"

_WINDOWS_INVALID_CHARS = '<>:"/\\|?*'
_CONTROL_CHARS_RE = re.compile(r"[\x00-\x1f\x7f]")
_RESERVED_NAME_RE = re.compile(
    r"^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(\..*)?$", re.IGNORECASE
)


def sanitize_filename(name: str) -> str:
    """Sanitise ``name`` into a safe single Windows pathname component.

    Replaces invalid Windows characters (``<>:"/\\|?*``) and control
    characters with ``_``, strips trailing dots/spaces (which Windows ignores
    anyway), neutralises reserved device names (``CON``, ``NUL``, ``LPT1``...)
    by prefixing ``_``, and clamps to 200 characters while preserving the
    extension. Empty input yields ``"download"``.
    """
    cleaned = _CONTROL_CHARS_RE.sub("_", name or "")
    cleaned = "".join("_" if ch in _WINDOWS_INVALID_CHARS else ch for ch in cleaned)
    cleaned = cleaned.rstrip(" .").strip()
    if not cleaned:
        return FALLBACK_NAME
    cleaned = _truncate_preserving_extension(cleaned)
    if _RESERVED_NAME_RE.match(cleaned):
        cleaned = f"_{cleaned}"
    return cleaned or FALLBACK_NAME


def _truncate_preserving_extension(name: str) -> str:
    """Clamp ``name`` to :data:`MAX_FILENAME_LEN` keeping a plausible extension."""
    if len(name) <= MAX_FILENAME_LEN:
        return name
    dot = name.rfind(".")
    if 0 < dot:
        ext = name[dot + 1:]
        if 0 < len(ext) <= 10 and ext.isalnum():
            keep = MAX_FILENAME_LEN - len(ext) - 1
            return f"{name[:keep]}.{ext}"
    return name[:MAX_FILENAME_LEN]


def filename_from_url(url: str) -> str:
    """Derive a filename from a URL path (fragment+query stripped, decoded)."""
    try:
        parsed = urllib.parse.urlsplit(url)
    except ValueError:
        return FALLBACK_NAME
    basename = (parsed.path or "").rsplit("/", 1)[-1]
    try:
        basename = urllib.parse.unquote(basename)
    except UnicodeDecodeError:
        basename = urllib.parse.unquote(basename, errors="replace")
    return sanitize_filename(basename)


def resolve_destination(destination_dir: str | pathlib.Path, filename: str) -> pathlib.Path:
    """Join ``filename`` into ``destination_dir`` and refuse path traversal.

    The result is guaranteed (after resolution) to live inside
    ``destination_dir``; names like ``../../evil.exe`` are re-anchored to the
    base directory rather than escaped.
    """
    base = pathlib.Path(destination_dir).expanduser().resolve()
    safe_name = sanitize_filename(filename)
    candidate = (base / safe_name).resolve()
    if candidate != base and base not in candidate.parents:
        candidate = base / FALLBACK_NAME
    return candidate


def unique_path(path: pathlib.Path, policy: str) -> pathlib.Path | None:
    """Apply a filename-collision policy and return the path to write to.

    Args:
        path: desired destination path.
        policy: ``keep_existing`` (existing file is authoritative; the same
            path is returned and the caller must not overwrite it),
            ``rename`` (find a free ``stem (2).ext`` variant), ``overwrite``
            (return the path; caller replaces the file) or ``skip`` (``None``;
            the caller skips without touching disk).

    Raises:
        ValueError: unknown policy.
    """
    if policy not in ("keep_existing", "rename", "overwrite", "skip"):
        raise ValueError(f"unknown collision policy: {policy!r}")
    if not path.exists():
        return path
    if policy == "rename":
        return _next_free_variant(path)
    if policy == "skip":
        return None
    return path  # keep_existing and overwrite both keep the same path


def _next_free_variant(path: pathlib.Path) -> pathlib.Path:
    """Return the first free ``stem (n).ext`` variant of an existing path."""
    for n in range(1, 1000):
        candidate = path.with_name(f"{path.stem} ({n}){path.suffix}")
        if not candidate.exists():
            return candidate
    return path.with_name(f"{path.stem} (too-many){path.suffix}")


def is_reserved_windows_name(name: str) -> bool:
    """True if ``name`` is a reserved Windows device name (CON, LPT2, ...)."""
    return bool(_RESERVED_NAME_RE.match(name))


def classify_extension(url_or_name: str) -> str | None:
    """Extract a lowercased file extension (without the dot) or ``None``.

    Accepts a URL (fragments/query are stripped first) or a bare filename.
    Returns the extension of the last path segment when it looks like a
    plausible extension (alphanumeric, at most 10 chars); ``None`` if the
    name has no extension or it is not plausible.
    """
    text = url_or_name or ""
    try:
        path = urllib.parse.urlsplit(text).path
    except ValueError:
        path = text
    basename = (path or text).rsplit("/", 1)[-1].rsplit("\\", 1)[-1]
    if "." not in basename:
        return None
    ext = basename.rsplit(".", 1)[-1].strip().lower()
    if ext and ext.isalnum() and len(ext) <= 10:
        return ext
    return None
