"""Provider adapters. No GUI imports.

Importing this package side-effect-registers the built-in adapters
(:mod:`src.providers.direct_http`) and re-exports the public names of
:mod:`src.providers.base` so callers can ``from src.providers import
ResolvedDownload, get_adapter_for_url``.
"""

from src.providers.base import (
    ProviderAdapter,
    ProviderError,
    ResolvedDownload,
    get_adapter_for_url,
    register_adapter,
)
from src.providers import direct_http  # noqa: F401  (side-effect registration)

__all__ = [
    "ProviderAdapter",
    "ProviderError",
    "ResolvedDownload",
    "get_adapter_for_url",
    "register_adapter",
]
