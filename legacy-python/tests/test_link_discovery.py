"""Tests for ``src.core.link_discovery``: candidate extraction, classification,
HEAD enrichment and the never-raise error posture."""

from __future__ import annotations

import dataclasses

import pytest

from src.core.link_discovery import DiscoveryResult, LinkDiscovery
from src.database.models import Settings


@pytest.fixture()
def scanner() -> LinkDiscovery:
    """A fresh facade with a fresh HTTP adapter (own client + robots cache)."""
    return LinkDiscovery()


# -- /page scanning -----------------------------------------------------------------


def test_scan_page_finds_expected_candidates(
    scanner: LinkDiscovery, http_server, settings: Settings
) -> None:
    # Act
    result = scanner.scan(http_server.url("/page"), settings)

    # Assert: the anchors under /files, /downloads, /docs and /img surface
    # as absolute URLs (redirect target /files/data.bin is reachable too).
    by_url = {c.candidate_url: c for c in result.candidates}
    for path in (
        "/files/data.bin", "/files/big.zip", "/downloads/report.zip",
        "/docs/manual.pdf", "/img/photo.png",
    ):
        assert http_server.url(path) in by_url, f"missing candidate for {path}"
    # The scan itself never raises; errors is a list of strings.
    assert isinstance(result.errors, list)
    assert all(isinstance(err, str) for err in result.errors)


def test_scan_page_deduplicates_anchors(
    scanner: LinkDiscovery, http_server, settings: Settings
) -> None:
    # Act: /page links /files/data.bin three times (relative, absolute, dup).
    result = scanner.scan(http_server.url("/page"), settings)

    # Assert: one candidate per unique URL; no fragment residue.
    data_bin = http_server.url("/files/data.bin")
    assert sum(c.candidate_url == data_bin for c in result.candidates) == 1
    assert all("#" not in c.candidate_url for c in result.candidates)


def test_scan_page_classification(
    scanner: LinkDiscovery, http_server, settings: Settings
) -> None:
    # Act
    result = scanner.scan(http_server.url("/page"), settings)
    by_url = {c.candidate_url: c for c in result.candidates}

    # Assert: archive/image/docs extensions are likely files...
    for path, expected_url_ext in (
        ("/files/big.zip", "zip"),
        ("/downloads/report.zip", "zip"),
        ("/docs/manual.pdf", "pdf"),
        ("/img/photo.png", "png"),
    ):
        candidate = by_url[http_server.url(path)]
        assert candidate.is_likely_file, path
        assert candidate.extension == expected_url_ext
    # ...a plain page link is not...
    assert not by_url[http_server.url("/about")].is_likely_file
    # ...and .bin is unknown until include_unknown_types is opted into.
    data_bin = by_url[http_server.url("/files/data.bin")]
    assert data_bin.extension == "bin"
    assert not data_bin.is_likely_file


def test_include_unknown_types_admits_unknown_extensions(
    http_server, settings: Settings
) -> None:
    # Arrange
    scanner = LinkDiscovery()
    permissive = dataclasses.replace(settings, include_unknown_types=True)

    # Act
    result = scanner.scan(http_server.url("/page"), permissive)
    data_bin = http_server.url("/files/data.bin")
    candidate = next(c for c in result.candidates if c.candidate_url == data_bin)

    # Assert
    assert candidate.is_likely_file
    assert candidate.extension == "bin"


def test_head_enrichment_for_likely_candidates(
    scanner: LinkDiscovery, http_server, settings: Settings
) -> None:
    # Act: respect_rate_limit is True by default -> HEAD enrichment runs.
    result = scanner.scan(http_server.url("/page"), settings)
    by_url = {c.candidate_url: c for c in result.candidates}
    big_zip = by_url[http_server.url("/files/big.zip")]

    # Assert: the fake site answered the HEAD with size + content type.
    assert big_zip.size == 16384
    assert big_zip.content_type == "application/zip"
    assert http_server.hits("/files/big.zip") >= 1
    # data.bin is not a likely candidate -> it is never probed.
    assert http_server.hits("/files/data.bin") == 0


def test_robots_txt_404_is_permissive(
    scanner: LinkDiscovery, http_server, settings: Settings
) -> None:
    # Act: no robots.txt exists on the fake site (every unknown path 404s).
    result = scanner.scan(http_server.url("/page"), settings)

    # Assert: the scan still proceeded and produced candidates.
    assert result.candidates
    assert result.errors == []


# -- safety: redirects and refusal paths ---------------------------------------------


def test_redirect_to_private_address_blocked(
    scanner: LinkDiscovery, http_server, settings: Settings
) -> None:
    # Act: the redirect chain either refuses the private hop or fails to
    # reach it; in both cases it must end up as a string error, not an
    # exception, and must never report the private address as a candidate.
    result = scanner.scan(http_server.url("/redirect-private"), settings)

    # Assert
    joined = "\n".join(result.errors)
    assert result.errors, "a blocked/failed redirect must be reported"
    assert "169.254.169.254" in joined
    assert all(
        "169.254.169.254" not in c.candidate_url for c in result.candidates
    )


def test_page_404_reported_as_error_not_exception(
    scanner: LinkDiscovery, http_server, settings: Settings
) -> None:
    # Act
    result = scanner.scan(http_server.url("/status/404"), settings)

    # Assert: actionable message, no exception, no candidates.
    assert result.errors, "a 404 page must be reported"
    assert "404" in "\n".join(result.errors)
    assert result.candidates == []


def test_binary_page_scanned_gracefully(
    scanner: LinkDiscovery, http_server, settings: Settings
) -> None:
    # Act: a page that is actually binary data (2048 bytes, every byte value).
    result = scanner.scan(http_server.url("/files/data.bin"), settings)

    # Assert: tolerated without raising and without inventing candidates.
    assert isinstance(result, DiscoveryResult)
    assert result.candidates == []


@pytest.mark.parametrize(
    "path", ["/page", "/status/404", "/files/data.bin", "/redirect-private"]
)
def test_scan_never_raises(
    http_server, settings: Settings, path: str
) -> None:
    # Act + Assert: none of the network outcomes may raise out of scan().
    result = LinkDiscovery().scan(http_server.url(path), settings)
    assert isinstance(result, DiscoveryResult)
    assert isinstance(result.errors, list)
