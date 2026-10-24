"""Tests for ``src.core.security``: network safety, redaction, redirects."""

from __future__ import annotations

import urllib.parse

import pytest

from src.core.security import (
    UnsafeUrlError,
    redact_url,
    validate_redirect,
    validate_url_for_network,
)


# -- validate_url_for_network: rejections -----------------------------------------


@pytest.mark.parametrize(
    "url",
    [
        "http://127.0.0.1/x",
        "http://localhost/x",
        "http://10.1.2.3/f.bin",
        "http://192.168.1.10/f.bin",
        "http://172.16.9.1/f.bin",
        "http://172.31.255.1/f.bin",
        "http://169.254.169.254/latest/meta-data/",
        "http://0.0.0.0/x",
        "http://[::1]/x",
        "http://[fe80::1]/x",
    ],
)
def test_private_targets_rejected(url: str) -> None:
    with pytest.raises(UnsafeUrlError):
        validate_url_for_network(url)


@pytest.mark.parametrize(
    "url",
    [
        "ftp://example.com/file.zip",
        "javascript:alert(1)",
        "file:///C:/evil.exe",
        "",
        "   ",
    ],
)
def test_bad_scheme_and_empty_rejected(url: str) -> None:
    with pytest.raises(UnsafeUrlError):
        validate_url_for_network(url)


def test_userinfo_rejected() -> None:
    # allow_private irrelevant: credentials-in-URL are rejected outright
    with pytest.raises(UnsafeUrlError, match="userinfo"):
        validate_url_for_network("http://user:secretpw@127.0.0.1/x", allow_private=True)


def test_port_out_of_range_rejected() -> None:
    # urllib.parse.urlsplit itself raises ValueError for ports > 65535
    with pytest.raises(ValueError, match="out of range"):
        validate_url_for_network("http://127.0.0.1:99999/x", allow_private=True)


# -- validate_url_for_network: allow_private escape hatch ---------------------------


def test_allow_private_accepts_literal_loopback() -> None:
    validated = validate_url_for_network("http://127.0.0.1:8000/data.bin", allow_private=True)
    assert validated.scheme == "http"
    assert validated.host == "127.0.0.1"
    assert validated.port == 8000
    assert validated.is_host_literal_ip is True
    assert tuple(map(str, validated.resolved_ips)) == ("127.0.0.1",)


def test_allow_private_accepts_localhost_hostname() -> None:
    validated = validate_url_for_network("http://localhost/files/a.zip", allow_private=True)
    assert validated.host == "localhost"
    assert validated.is_host_literal_ip is False
    assert validated.resolved_ips  # resolved to loopback addresses
    assert all(ip.is_loopback for ip in validated.resolved_ips)


def test_allow_private_accepts_ipv6_loopback_with_default_https_port() -> None:
    validated = validate_url_for_network("https://[::1]/x", allow_private=True)
    assert validated.scheme == "https"
    assert validated.port == 443


# -- redact_url ---------------------------------------------------------------------


def test_redact_url_masks_secret_query_params() -> None:
    original = (
        "https://example.com/download/f.zip?keep=yes&token=abc123"
        "&SIG=deadbeef&api_key=K9&notasecret=plain"
    )
    redacted = redact_url(original)

    assert "abc123" not in redacted
    assert "deadbeef" not in redacted
    assert "K9" not in redacted
    assert "plain" in redacted  # harmless parameter untouched
    assert "example.com/download/f.zip" in redacted

    query = urllib.parse.parse_qsl(urllib.parse.urlsplit(redacted).query)
    values = dict(query)
    assert values["token"] == "***"
    assert values["sig"] == "***"  # case-insensitive param matching
    assert values["api_key"] == "***"
    assert values["keep"] == "yes"
    assert values["notasecret"] == "plain"


def test_redact_url_strips_userinfo() -> None:
    redacted = redact_url(
        "https://alice:p4ssw0rd@example.com/x?a=1&password=hunter2"
    )
    assert "alice" not in redacted
    assert "p4ssw0rd" not in redacted
    assert "hunter2" not in redacted
    assert "example.com" in redacted
    values = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(redacted).query))
    assert values["a"] == "1"
    assert values["password"] == "***"


def test_redact_url_preserves_port_and_path() -> None:
    redacted = redact_url("https://example.com:8443/a/b/c.bin?k=1")
    assert ":8443" in redacted
    assert "/a/b/c.bin" in redacted


@pytest.mark.parametrize("bad", ["", "   ", "not a url", "scheme-only:"])
def test_redact_url_unparseable_collapses(bad: str) -> None:
    assert redact_url(bad) == "<redacted-url>"


def test_redact_url_non_string_input() -> None:
    assert redact_url(None) == "<redacted-url>"  # type: ignore[arg-type]
    assert redact_url(12345) == "<redacted-url>"  # type: ignore[arg-type]


# -- validate_redirect ----------------------------------------------------------------


def test_validate_redirect_blocks_https_to_http_downgrade() -> None:
    with pytest.raises(UnsafeUrlError, match="downgrade"):
        validate_redirect("https://example.com/start", "http://example.com/next")


def test_validate_redirect_blocks_private_target() -> None:
    with pytest.raises(UnsafeUrlError, match="private"):
        validate_redirect("http://example.com/start", "http://10.9.8.7/hit")
    with pytest.raises(UnsafeUrlError, match="private"):
        validate_redirect(
            "http://example.com/start",
            "http://169.254.169.254/latest/meta-data/",
        )


def test_validate_redirect_blocking_is_independent_of_allow_private() -> None:
    # without allow_private the private redirect is refused as usual
    with pytest.raises(UnsafeUrlError, match="private"):
        validate_redirect(
            "http://example.com/start",
            "http://127.0.0.1:9/hit",
        )


def test_validate_redirect_resolves_relative_location() -> None:
    validated = validate_redirect(
        "http://127.0.0.1:77/a/b/c", "/files/data.bin", allow_private=True
    )
    assert validated.url == "http://127.0.0.1:77/files/data.bin"
    assert validated.host == "127.0.0.1"
    assert validated.port == 77


def test_validate_redirect_allows_private_between_loopback_hosts() -> None:
    validated = validate_redirect(
        "http://127.0.0.1:77/start", "http://127.0.0.1:88/next", allow_private=True
    )
    assert validated.host == "127.0.0.1"
    assert validated.port == 88


def test_validate_redirect_empty_location_rejected() -> None:
    with pytest.raises(UnsafeUrlError, match="empty Location"):
        validate_redirect("https://example.com/start", "  ")


def test_validate_redirect_non_http_location_rejected() -> None:
    with pytest.raises(UnsafeUrlError, match="not an http"):
        validate_redirect("https://example.com/start", "javascript:alert(1)")
