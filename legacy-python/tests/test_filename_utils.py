"""Tests for ``src.core.filename_utils``: sanitisation, URLs, collisions."""

from __future__ import annotations

import pathlib

import pytest

from src.core.filename_utils import (
    MAX_FILENAME_LEN,
    FALLBACK_NAME,
    classify_extension,
    filename_from_url,
    is_reserved_windows_name,
    sanitize_filename,
    unique_path,
)


# -- sanitize_filename ----------------------------------------------------------


def test_invalid_windows_chars_replaced() -> None:
    assert sanitize_filename('a<b>c:d"e|f?g*h') == "a_b_c_d_e_f_g_h"
    assert sanitize_filename("a/b") == "a_b"
    assert sanitize_filename("a\\b") == "a_b"
    # ':' inside a drive-letter-style name is tamed too
    assert sanitize_filename("C:report.zip") == "C_report.zip"


def test_control_characters_replaced() -> None:
    assert sanitize_filename("bad\x00\x01\x1f\x7fname") == "bad____name"


def test_trailing_dots_and_spaces_stripped() -> None:
    assert sanitize_filename("name...") == "name"
    assert sanitize_filename("name . . ") == "name"  # mixed trailing dots/spaces
    assert sanitize_filename("name  ") == "name"
    assert sanitize_filename("  spaced.txt ") == "spaced.txt"
    assert sanitize_filename(" .hidden") == ".hidden"  # leading dots are kept


@pytest.mark.parametrize(
    ("reserved", "expected"),
    [
        ("CON", "_CON"),
        ("con.txt", "_con.txt"),
        ("PRN", "_PRN"),
        ("AUX", "_AUX"),
        ("NUL", "_NUL"),
        ("COM1", "_COM1"),
        ("com5", "_com5"),
        ("LPT3", "_LPT3"),
        ("CON.", "_CON"),  # trailing dot stripped first, then reservation applies
    ],
)
def test_reserved_device_names_neutralised(reserved: str, expected: str) -> None:
    assert sanitize_filename(reserved) == expected
    assert is_reserved_windows_name(expected.lstrip("_"))


def test_com10_is_not_reserved() -> None:
    assert sanitize_filename("COM10") == "COM10"
    assert not is_reserved_windows_name("COM10")


@pytest.mark.parametrize("empty", ["", "   ", "...", ". . ..."])
def test_empty_input_falls_back(empty: str) -> None:
    assert sanitize_filename(empty) == FALLBACK_NAME


def test_long_input_clamped_to_200() -> None:
    plain = sanitize_filename("a" * 250)
    assert len(plain) == MAX_FILENAME_LEN
    assert len(plain) <= 200

    with_ext = sanitize_filename("b" * 250 + ".zip")
    assert len(with_ext) <= MAX_FILENAME_LEN
    assert with_ext.endswith(".zip")


def test_long_input_with_implausible_extension_just_clamped() -> None:
    name = sanitize_filename("c" * 250 + "." + "x" * 11)
    assert len(name) == MAX_FILENAME_LEN
    assert not name.endswith("." + "x" * 11)


# -- filename_from_url ------------------------------------------------------------


def test_filename_from_url_strips_query_and_fragment_and_decodes() -> None:
    url = "https://example.com/dir/report%202024.zip?token=x&y=1#frag"
    assert filename_from_url(url) == "report 2024.zip"


def test_filename_from_url_query_only_urls() -> None:
    assert filename_from_url("https://example.com/f.pdf?x=1&y=2") == "f.pdf"


@pytest.mark.parametrize(
    ("url", "expected"),
    [
        ("https://example.com/", FALLBACK_NAME),
        ("http://example.com", FALLBACK_NAME),
        ("http://[", FALLBACK_NAME),  # unparseable -> fallback
    ],
)
def test_filename_from_url_fallbacks(url: str, expected: str) -> None:
    assert filename_from_url(url) == expected


def test_filename_from_url_sanitises_hostile_names() -> None:
    url = "https://example.com/win/CON.zip"
    assert filename_from_url(url) == "_CON.zip"


# -- unique_path --------------------------------------------------------------------


@pytest.mark.parametrize("policy", ["keep_existing", "rename", "overwrite", "skip"])
def test_unique_path_nonexistent_returns_desired_path(
    tmp_path: pathlib.Path, policy: str
) -> None:
    desired = tmp_path / "new.txt"
    assert unique_path(desired, policy) == desired


def test_unique_path_keep_existing_and_overwrite_keep_path(
    tmp_path: pathlib.Path,
) -> None:
    path = tmp_path / "file.txt"
    path.write_text("x")
    assert unique_path(path, "keep_existing") == path
    assert unique_path(path, "overwrite") == path


def test_unique_path_skip_returns_none(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "file.txt"
    path.write_text("x")
    assert unique_path(path, "skip") is None


def test_unique_path_rename_finds_free_variant(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "file.txt"
    path.write_text("x")
    first = unique_path(path, "rename")
    assert first == tmp_path / "file (1).txt"
    first.write_text("y")
    second = unique_path(path, "rename")
    assert second == tmp_path / "file (2).txt"
    assert not second.exists()


def test_unique_path_rename_multi_extension(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "archive.tar.gz"
    path.write_bytes(b"z")
    assert unique_path(path, "rename") == tmp_path / "archive.tar (1).gz"


def test_unique_path_unknown_policy_raises(tmp_path: pathlib.Path) -> None:
    with pytest.raises(ValueError, match="collision policy"):
        unique_path(tmp_path / "a.txt", "bogus")


# -- classify_extension ---------------------------------------------------------------


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        ("http://example.com/files/photo.PNG?dl=1#section", "png"),
        ("file.zip", "zip"),
        ("archive.tar.gz", "gz"),
        ("dir\\interop.docx", "docx"),
        ("  spaced.TXT  ", "txt"),
        ("missing", None),
        ("trailing.", None),
        ("too.long." + "x" * 11, None),  # >10 extension chars
        ("http://example.com/dir/", None),
        ("", None),
        (".gitignore", "gitignore"),  # documented: text after the last dot
    ],
)
def test_classify_extension(text: str, expected: str | None) -> None:
    assert classify_extension(text) == expected
