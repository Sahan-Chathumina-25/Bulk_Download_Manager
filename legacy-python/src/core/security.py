"""Network-safety validation for URLs — runs at intake and after every redirect.

.. warning::
   DNS-rebinding caveat: ``validate_url_for_network`` resolves the hostname
   once and checks the resolved IPs at validation time. A hostile DNS server
   can return a safe address during validation and a private address on the
   actual connection later (classic rebind attack). Full mitigation requires
   pinning the validated IP onto the connection; callers that re-validate
   right before each request shrink, but do not eliminate, this window.
"""

from __future__ import annotations

import ipaddress
import socket
import urllib.parse
from dataclasses import dataclass, field

__all__ = [
    "UnsafeUrlError",
    "ValidatedUrl",
    "redact_url",
    "validate_url_for_network",
]

_ALLOWED_SCHEMES = frozenset({"http", "https"})

# Hostname groups that must never be dialed from a user's machine.
_PRIVATE_V4_NETWORKS: tuple[ipaddress.IPv4Network, ...] = (
    ipaddress.ip_network("0.0.0.0/8"),          # "this network"
    ipaddress.ip_network("10.0.0.0/8"),
    ipaddress.ip_network("127.0.0.0/8"),        # loopback
    ipaddress.ip_network("169.254.0.0/16"),     # link-local incl. metadata svc
    ipaddress.ip_network("172.16.0.0/12"),
    ipaddress.ip_network("192.168.0.0/16"),
)

_PRIVATE_V6_NETWORKS: tuple[ipaddress.IPv6Network, ...] = (
    ipaddress.ip_network("::1/128"),            # loopback
    ipaddress.ip_network("::/128"),             # unspecified "::"
    ipaddress.ip_network("fc00::/7"),           # unique local
    ipaddress.ip_network("fe80::/10"),          # link-local
)

# Query parameter names that carry secrets and are stripped from log output.
_SECRET_PARAM_NAMES = frozenset(
    {
        "token", "access_token", "refresh_token", "id_token", "auth",
        "authorization", "api_key", "apikey", "api-key", "key", "secret",
        "client_secret", "signature", "sig", "sigv2", "sigv4", "hmac",
        "password", "passwd", "pwd", "pass", "session", "sessionid",
        "session_id", "sess", "jsessionid", "phpsessid", "aspsessionid",
        "creds", "credentials", "credential", "oauth", "oidc", "sso",
        "saml", "saslauth", "sas", "shared_access_signature", "sts",
        "awsaccesskeyid", "x-amz-signature", "x-goog-signature",
        "x-goog-credential", "x-goog-date", "keyid", "jwt", "bearer",
    }
)


@dataclass(frozen=True)
class ValidatedUrl:
    """A URL that passed network-safety validation."""

    url: str
    scheme: str
    host: str
    port: int
    resolved_ips: tuple[ipaddress.IPv4Address | ipaddress.IPv6Address, ...] = field(
        default=()
    )
    is_host_literal_ip: bool = False


class UnsafeUrlError(ValueError):
    """Raised when a URL must not be dialed (bad scheme, private IP, ...)."""


def _is_private(ip: ipaddress.IPv4Address | ipaddress.IPv6Address) -> bool:
    """True if the address falls into any blocked network range."""
    if isinstance(ip, ipaddress.IPv4Address):
        return any(ip in net for net in _PRIVATE_V4_NETWORKS)
    return any(ip in net for net in _PRIVATE_V6_NETWORKS)


def _resolve_host(host: str) -> tuple[ipaddress.IPv4Address | ipaddress.IPv6Address, ...]:
    """Resolve a hostname to every address getaddrinfo reports.

    Raises:
        UnsafeUrlError: if resolution fails entirely (fail-closed).
    """
    try:
        infos = socket.getaddrinfo(host, None, family=socket.AF_UNSPEC, type=socket.SOCK_STREAM)
    except (socket.gaierror, OSError, UnicodeError) as exc:
        raise UnsafeUrlError(f"could not resolve host {host!r}: {exc}") from exc
    if not infos:
        raise UnsafeUrlError(f"host {host!r} did not resolve to any address")
    ips: list[ipaddress.IPv4Address | ipaddress.IPv6Address] = []
    for _family, _type, _proto, _canon, sockaddr in infos:
        address = sockaddr[0]
        try:
            ips.append(ipaddress.ip_address(address))
        except ValueError:
            continue  # non-address-family entry; skip rather than trust blindly
    if not ips:
        raise UnsafeUrlError(f"host {host!r} resolved without usable IP addresses")
    return tuple(ips)


def validate_url_for_network(
    url: str, *, allow_private: bool = False, resolve_dns: bool = True
) -> ValidatedUrl:
    """Validate that ``url`` is safe to connect to from this machine.

    Checks performed:
    - scheme must be exactly ``http`` or ``https``
    - a host must be present; embedded userinfo is rejected (never dial URLs
      that hide credentials — callers can pass credentials via headers)
    - port, if present, must be a valid TCP number
    - literal IP hosts are checked directly; hostnames are resolved via DNS
      and rejected when *every* resolved address is private/link-local
      (see the DNS-rebinding caveat in the module docstring)

    Args:
        url: the URL to validate.
        allow_private: explicit opt-in to dial private/loopback addresses
            (needed by the local test server and trusted-LAN deployments).
        resolve_dns: preflight-only mode. When ``False`` a *hostname* is not
            DNS-resolved, so its resolved-address privacy cannot be checked
            and the fail-closed refusal for unresolvable hosts does not
            apply; every other check still runs. Preflight-only by design:
            the full default validation (with DNS) must still run before
            anything actually connects — the download manager re-validates
            at :meth:`~src.core.download_manager.DownloadManager.add_download`
            time and the worker re-validates before each request and after
            every redirect. Never dial a URL that was only preflighted.

    Raises:
        UnsafeUrlError: describing exactly why the URL is unsafe.
    """
    if not isinstance(url, str) or not url.strip():
        raise UnsafeUrlError("URL is empty")
    parsed = urllib.parse.urlsplit(url.strip())

    if parsed.scheme.lower() not in _ALLOWED_SCHEMES:
        raise UnsafeUrlError(
            f"unsupported URL scheme {parsed.scheme!r}; only http/https are allowed"
        )
    if not parsed.hostname:
        raise UnsafeUrlError("URL has no host")
    if parsed.username is not None or parsed.password is not None:
        raise UnsafeUrlError(
            "URL embeds userinfo (user:password@); put authentication in headers instead"
        )
    if parsed.port is not None and not (0 < parsed.port <= 65535):
        raise UnsafeUrlError(f"URL port out of range: {parsed.port}")
    port = parsed.port if parsed.port is not None else (443 if parsed.scheme.lower() == "https" else 80)

    is_literal = False
    resolved: tuple[ipaddress.IPv4Address | ipaddress.IPv6Address, ...] = ()
    try:
        literal = ipaddress.ip_address(parsed.hostname)
    except ValueError:
        if resolve_dns:
            resolved = _resolve_host(parsed.hostname)
            if not allow_private and all(_is_private(ip) for ip in resolved):
                raise UnsafeUrlError(
                    f"all resolved addresses for host {parsed.hostname!r} are "
                    "private/loopback; refusing to connect"
                ) from None
    else:
        is_literal = True
        resolved = (literal,)
        if _is_private(literal) and not allow_private:
            raise UnsafeUrlError(
                f"refusing to connect to private/loopback address {literal} "
                f"(host {parsed.hostname!r})"
            )

    return ValidatedUrl(
        url=url.strip(),
        scheme=parsed.scheme.lower(),
        host=parsed.hostname,
        port=port,
        resolved_ips=resolved,
        is_host_literal_ip=is_literal,
    )


def validate_redirect(current: str, target: str, *, allow_private: bool = False) -> ValidatedUrl:
    """Validate a redirect hop, rejecting silent https-to-http downgrades.

    Applies :func:`validate_url_for_network` to the target and additionally
    refuses to follow a redirect that downgrades the scheme (https -> http),
    which would silently expose the request to interception.

    Args:
        current: URL of the response issuing the redirect.
        target: ``Location`` value to follow (may be relative).
        allow_private: passed through to the underlying validation.

    Raises:
        UnsafeUrlError: on private targets, bad schemes or downgrades.
    """
    current_parsed = urllib.parse.urlsplit(current)
    target_parsed = _resolve_location_target(current_parsed, target)

    if (current_parsed.scheme.lower() == "https"
            and target_parsed.scheme.lower() == "http"):
        raise UnsafeUrlError("refusing https-to-http downgrade during redirect")

    if target_parsed.hostname and current_parsed.hostname and (
        target_parsed.hostname.lower() != current_parsed.hostname.lower()
    ):
        # Cross-host redirects get full network validation as usual; no extra
        # restriction beyond the scheme check above.
        pass

    return validate_url_for_network(
        target_parsed.geturl(), allow_private=allow_private
    )


def _resolve_location_target(
    current: urllib.parse.SplitResult, location: str
) -> urllib.parse.SplitResult:
    """Resolve a ``Location`` header value (``urljoin`` semantics, fail-closed)."""
    if not location or not location.strip():
        raise UnsafeUrlError("redirect has an empty Location header")
    joined = urllib.parse.urljoin(
        current.geturl() if current.scheme else "", location.strip()
    )
    if not joined.lower().startswith(("http://", "https://")):
        raise UnsafeUrlError(f"redirect target is not an http(s) URL: {location!r}")
    return urllib.parse.urlsplit(joined)


def redact_url(url: str) -> str:
    """Return a log-safe version of ``url``.

    Removes ``user:password@`` userinfo entirely and masks the value of any
    query parameter whose name matches a secret-like name (token, sig,
    api_key, ...) case-insensitively. Harmless parameter names, casing and
    values are preserved verbatim; a masked one is rendered with its name
    canonicalised to lowercase (``SIG=abc`` becomes ``sig=***``) so redacted
    output is deterministic in logs. The redacted URL is never re-usable as
    a request URL. Invalid/unparseable input collapses to
    ``"<redacted-url>"``.
    """
    if not isinstance(url, str):
        return "<redacted-url>"
    try:
        parsed = urllib.parse.urlsplit(url)
        netloc = parsed.hostname or ""
        if parsed.port is not None:
            netloc = f"{netloc}:{parsed.port}"
        if not netloc:
            return "<redacted-url>"
        pairs = urllib.parse.parse_qsl(parsed.query, keep_blank_values=True)
        safe_pairs: list[tuple[str, str]] = []
        for name, value in pairs:
            if name.lower() in _SECRET_PARAM_NAMES:
                safe_pairs.append((name.lower(), "***"))
            else:
                safe_pairs.append((name, value))
        query = urllib.parse.urlencode(safe_pairs)
        return urllib.parse.urlunsplit(
            (parsed.scheme, netloc, parsed.path, query, "")
        )
    except ValueError:
        return "<redacted-url>"
