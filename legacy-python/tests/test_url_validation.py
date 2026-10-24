"""Tests for URL intake: ``src.core.url_validator.parse_url_lines``."""

from __future__ import annotations

import pytest

from src.core.url_validator import parse_url_lines

# Loopback literal IPs need no DNS resolution, keeping these tests offline
# and deterministic; they require ``allow_private=True`` to be accepted.
LOOP = "http://127.0.0.1:9"


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        # CRLF line endings
        (f"{LOOP}/a\r\n{LOOP}/b\r\n{LOOP}/c", [f"{LOOP}/a", f"{LOOP}/b", f"{LOOP}/c"]),
        # bare LF
        (f"{LOOP}/a\n{LOOP}/b", [f"{LOOP}/a", f"{LOOP}/b"]),
        # bare CR (old Mac style)
        (f"{LOOP}/a\r{LOOP}/b\r{LOOP}/c", [f"{LOOP}/a", f"{LOOP}/b", f"{LOOP}/c"]),
        # mixed endings in one paste
        (
            f"{LOOP}/a\r\n{LOOP}/b\n{LOOP}/c\r{LOOP}/d",
            [f"{LOOP}/a", f"{LOOP}/b", f"{LOOP}/c", f"{LOOP}/d"],
        ),
    ],
)
def test_line_endings(text: str, expected: list[str]) -> None:
    result = parse_url_lines(text, allow_private=True)
    assert result.accepted == expected
    assert result.rejected == []
    assert result.duplicates == []


def test_blank_and_whitespace_only_lines_skipped() -> None:
    text = f"\n\n  \n\t\n{LOOP}/a\n   \n\t\t\n{LOOP}/b\n\n"
    result = parse_url_lines(text, allow_private=True)
    assert result.accepted == [f"{LOOP}/a", f"{LOOP}/b"]
    assert result.rejected == []
    assert result.duplicates == []


def test_empty_input_yields_empty_result() -> None:
    for text in ("", "   ", "\r\n\r\n"):
        result = parse_url_lines(text)
        assert result.accepted == []
        assert result.rejected == []
        assert result.duplicates == []


def test_tab_separated_url_name_import() -> None:
    result = parse_url_lines(f"{LOOP}/file.zip\tinstaller package\n", allow_private=True)
    assert result.accepted == [f"{LOOP}/file.zip"]


@pytest.mark.parametrize(
    "bad",
    [
        "notaurl",  # no scheme
        "::::",  # garbage
        "ftp://example.com/file.zip",  # unsupported scheme
        "javascript:alert(1)",  # unsupported scheme
        "file:///C:/Windows/System32/calc.exe",  # unsupported scheme
        "http://",  # scheme but no host
        "https:///path/only",  # scheme but no host
    ],
)
def test_invalid_urls_rejected_with_reason(bad: str) -> None:
    result = parse_url_lines(bad)
    assert result.accepted == []
    assert len(result.rejected) == 1
    line, reason = result.rejected[0]
    assert line == bad
    assert reason, "every rejection must carry a reason"


def test_private_urls_rejected_by_default_then_accepted_with_flag() -> None:
    url = f"{LOOP}/a.zip"
    strict = parse_url_lines(url)
    assert strict.accepted == []
    assert strict.rejected[0][0] == url
    assert "private" in strict.rejected[0][1]

    relaxed = parse_url_lines(url, allow_private=True)
    assert relaxed.accepted == [url]
    assert relaxed.rejected == []


def test_ordered_dedup_first_occurrence_wins() -> None:
    a = f"{LOOP}/a"
    b = f"{LOOP}/b"
    text = f"{a}\n{b}\n{a}\n{b}\n{a}\n"
    result = parse_url_lines(text, allow_private=True)
    assert result.accepted == [a, b]
    assert result.duplicates == [a, b, a]


def test_duplicate_needs_exact_match() -> None:
    a = f"{LOOP}/a"
    b = f"{LOOP}/b"
    result = parse_url_lines(f"{a}\n{b}\n", allow_private=True)
    assert result.accepted == [a, b]
    assert result.duplicates == []


def test_accepted_order_preserves_input_order() -> None:
    urls = [f"{LOOP}/z", f"{LOOP}/m", f"{LOOP}/a", f"{LOOP}/k"]
    text = "\n".join(reversed(urls))  # urls pasted bottom-up
    result = parse_url_lines(text, allow_private=True)
    for parsed, expected in zip(result.accepted, reversed(urls), strict=True):
        assert parsed == expected


def test_mixed_accepted_and_rejected_in_one_batch() -> None:
    text = f"{LOOP}/ok.zip\nnotaurl\n{LOOP}/also-ok.bin\nftp://nope/x\n"
    result = parse_url_lines(text, allow_private=True)
    assert result.accepted == [f"{LOOP}/ok.zip", f"{LOOP}/also-ok.bin"]
    assert [reason for _, reason in result.rejected] == [
        "only http and https URLs are supported",
        "only http and https URLs are supported",
    ]
