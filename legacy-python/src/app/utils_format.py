"""Pure formatting helpers shared by pages and models (no Qt imports)."""

from __future__ import annotations

__all__ = ["format_bytes", "format_eta", "format_speed"]

_BYTE_UNITS: tuple[str, ...] = ("B", "KB", "MB", "GB", "TB", "PB")
_TIME_DIVISIONS: tuple[tuple[int, str], ...] = (
    (3600, "h"),
    (60, "m"),
    (1, "s"),
)


def format_bytes(n: int | float | None) -> str:
    """Human-readable byte count: ``1.2 MB`` style, ``—`` for unknown/zero-less.

    Args:
        n: byte count; ``None`` or negative renders as an em dash.
    """
    if n is None or n < 0:
        return "—"
    value = float(n)
    for unit in _BYTE_UNITS:
        if value < 1024 or unit == _BYTE_UNITS[-1]:
            return f"{value:,.1f} {unit}" if unit != "B" else f"{int(value)} B"
        value /= 1024
    return f"{value:,.1f} {_BYTE_UNITS[-1]}"  # pragma: no cover - unreachable


def format_speed(bps: float | None) -> str:
    """Human-readable speed: ``3.4 MB/s``; ``—`` for unknown, ``0 B/s`` for idle."""
    if bps is None or bps < 0:
        return "—"
    if bps < 1:
        return "0 B/s"
    units_iter = iter(_BYTE_UNITS)
    value = float(bps)
    unit = next(units_iter)
    while value >= 1024:
        try:
            unit = next(units_iter)
        except StopIteration:  # pragma: no cover - absurdly large speeds
            break
        value /= 1024
    if unit == "B":
        return f"{int(round(value))} B/s"
    return f"{value:,.1f} {unit}/s"


def format_eta(seconds: int | float | None) -> str:
    """Human-readable remaining time: ``h:mm:ss`` / ``m:ss`` / ``<1s`` / ``—``."""
    if seconds is None or seconds < 0:
        return "—"
    total = int(seconds)
    if total <= 0:
        return "<1s"
    hours = total // 3600
    minutes = (total % 3600) // 60
    secs = total % 60
    if hours:
        return f"{hours}:{minutes:02d}:{secs:02d}"
    return f"{minutes}:{secs:02d}"
