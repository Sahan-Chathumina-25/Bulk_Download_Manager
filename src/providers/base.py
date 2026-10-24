"""Provider adapter contract and the module-level adapter registry.

A *provider adapter* turns a share page / short link into one concrete
``ResolvedDownload`` (final URL, filename suggestion, headers...). The
download engine stays provider-agnostic: it resolves a URL first and then
hands the resolved values to :class:`src.core.download_manager.DownloadManager`.

The registry (:data:`_REGISTRY`) is populated by ``register_adapter`` —
typically at import time from the sibling adapter modules (see
:mod:`src.providers.direct_http`). Resolution picks the **first**
adapter whose ``can_handle`` matches in registration order.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Protocol, runtime_checkable

from src.database.models import Settings

__all__ = [
    "ProviderAdapter",
    "ProviderError",
    "ResolvedDownload",
    "get_adapter_for_url",
    "register_adapter",
]


@dataclass(frozen=True)
class ResolvedDownload:
    """One concrete download target produced by a provider adapter."""

    url: str
    filename: str | None
    headers: dict[str, str] = field(default_factory=dict)
    content_type: str | None = None
    size: int | None = None


class ProviderError(Exception):
    """A provider cannot resolve the URL (message is user-actionable).

    Handlers should surface ``str(exc)`` directly: examples — "this share
    requires a password", "link expired", "provider rate-limited us; retry
    in N minutes".
    """


@runtime_checkable
class ProviderAdapter(Protocol):
    """Structural contract for URL-resolving provider adapters.

    Adapters are stateless or thread-safe; ``resolve`` may perform network
    I/O but never downloads file bodies.
    """

    name: str

    def can_handle(self, url: str) -> bool:
        """Cheap structural check: does this adapter plausibly own ``url``?"""
        ...

    def resolve(self, url: str, settings: Settings) -> ResolvedDownload:
        """Turn ``url`` into one :class:`ResolvedDownload`.

        Raises:
            ProviderError: with an actionable message when the URL cannot
                be resolved.
        """
        ...


_REGISTRY: list[ProviderAdapter] = []


def register_adapter(adapter: ProviderAdapter) -> None:
    """Add ``adapter`` to the registry (idempotent by ``name``, keeps first position)."""
    name = getattr(adapter, "name", None)
    if any(getattr(existing, "name", None) == name for existing in _REGISTRY):
        return
    _REGISTRY.append(adapter)


def get_adapter_for_url(url: str) -> ProviderAdapter | None:
    """First registered adapter that claims ``url`` (registration order)."""
    for adapter in _REGISTRY:
        try:
            if adapter.can_handle(url):
                return adapter
        except Exception:  # noqa: BLE001 - a broken adapter must not break intake
            continue
    return None
