"""Default provider adapter for plain http(s) direct links.

``DirectHttpAdapter`` accepts any structurally valid http/https URL and
resolves it as-is — filename derived from the URL path, ``User-Agent``
from settings, no extra metadata. It is the registry's fallback that
guarantees every pasted http(s) link resolvable.
"""

from __future__ import annotations

import urllib.parse

from src.core import filename_utils, security
from src.core.security import UnsafeUrlError, validate_url_for_network
from src.database.models import Settings
from src.providers import base

__all__ = ["DirectHttpAdapter"]


class DirectHttpAdapter:
    """Resolve plain http(s) links without any provider-specific logic."""

    name: str = "direct-http"

    def can_handle(self, url: str) -> bool:
        """True for parseable absolute URLs with an ``http``/``https`` scheme.

        Deliberately cheap (no DNS): full network-safety validation,
        including private-address refusal, happens in :meth:`resolve`.
        """
        if not isinstance(url, str) or not url.strip():
            return False
        try:
            parsed = urllib.parse.urlsplit(url.strip())
        except ValueError:
            return False
        return parsed.scheme.lower() in ("http", "https") and bool(parsed.hostname)

    def resolve(self, url: str, settings: Settings) -> base.ResolvedDownload:
        """Validate the URL and return it unchanged as the download target.

        Raises:
            ProviderError: the URL is not a safe, direct http(s) link
                (private/loopback refusal honouring
                ``settings.allow_private_addresses``, bad schemes, ...).
        """
        try:
            validated = validate_url_for_network(
                url, allow_private=settings.allow_private_addresses
            )
        except UnsafeUrlError as exc:
            raise base.ProviderError(
                f"cannot download from {security.redact_url(url)}: {exc}"
            ) from exc
        return base.ResolvedDownload(
            url=validated.url,
            filename=filename_utils.filename_from_url(validated.url),
            headers={"User-Agent": settings.user_agent},
            content_type=None,
            size=None,
        )


base.register_adapter(DirectHttpAdapter())
