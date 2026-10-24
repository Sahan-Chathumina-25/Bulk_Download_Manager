"""Selection → queue planning for the Discover page (Qt-free, pure Python).

The Discover page shows scan results whose include-checkboxes feed a
batch queueing action. To keep that decision logic testable headless
(the GUI test suite stays Qt-free) the per-URL accept / duplicate /
reject decisions live in this module and are executed against the real
:class:`src.core.download_manager.DownloadManager.add_download` by
:func:`execute_queue_plan`.

The planner NEVER persists anything and NEVER touches the network; it
only collects decisions. Persistence happens exclusively through an
injected ``add_fn`` (``DownloadManager.add_download``) — no new
persistence scheme is introduced.
"""

from __future__ import annotations

import logging
import urllib.parse
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from src.core import security
from src.core.link_discovery import DiscoveryCandidate

if TYPE_CHECKING:  # pragma: no cover - typing only
    from src.database.models import DownloadRecord

__all__ = [
    "ExecutionReport",
    "QueuePlan",
    "checked_candidates",
    "execute_queue_plan",
    "plan_discovery_queue",
]

log = logging.getLogger(__name__)


def checked_candidates(
    candidates: Iterable[DiscoveryCandidate],
) -> list[DiscoveryCandidate]:
    """The rows the user ticked with the Include checkbox (source rows).

    The page model stores the tick state on each
    :class:`DiscoveryCandidate` (``is_likely_file``) rather than in Qt
    check-state bookkeeping, so selection is a plain list filter here.
    """
    return [candidate for candidate in candidates if candidate.is_likely_file]


@dataclass
class QueuePlan:
    """Outcome of planning one batch of selected candidates.

    Attributes:
        to_add: candidates eligible for the queue (first occurrence of
            each URL wins; later repeats land in ``duplicates``).
        duplicates: ``(label, reason)`` pairs for URLs already present
            in the database/queue or repeated inside the batch.
        rejected: ``(label, reason)`` pairs for unusable URLs
            (non-http scheme, unsafe address, malformed URL).
    """

    to_add: list[DiscoveryCandidate] = field(default_factory=list)
    duplicates: list[tuple[str, str]] = field(default_factory=list)
    rejected: list[tuple[str, str]] = field(default_factory=list)

    @property
    def is_empty(self) -> bool:
        """``True`` when nothing was selected at all."""
        return not (self.to_add or self.duplicates or self.rejected)


def plan_discovery_queue(
    candidates: Iterable[DiscoveryCandidate],
    *,
    existing_urls: Iterable[str] = (),
    allow_private: bool = False,
) -> QueuePlan:
    """Decide per candidate: add, skip as duplicate, or reject.

    Args:
        candidates: the candidates the user ticked (already filtered via
            :func:`checked_candidates` — unchecked rows never get here).
        existing_urls: source URLs already known to the repository
            (queued/active/history records). Matched on the normalized URL
            only (:func:`_normalized_url` — stripped, scheme/host
            lowercased, fragment dropped); no fuzzy matching, and entries
            that fail to parse are ignored rather than poisoning the batch.
        allow_private: settings escape hatch for LAN/loopback URLs.

    Returns:
        A :class:`QueuePlan` — never raises for candidate content.
    """
    known: set[str] = set()
    for raw in existing_urls:
        key = _normalized_url(raw)
        if key:
            known.add(key)
    plan = QueuePlan()
    seen_in_batch: set[str] = set()

    for candidate in candidates:
        label = candidate.filename or candidate.candidate_url
        raw_url = (candidate.candidate_url or "").strip()
        reason = _reject_reason(raw_url, allow_private=allow_private)
        if reason is not None:
            plan.rejected.append((label, reason))
            continue
        key = _normalized_url(raw_url)
        url = key if key is not None else raw_url
        if url in known:
            plan.duplicates.append((label, "already in the download list or history"))
            continue
        if url in seen_in_batch:
            plan.duplicates.append((label, "repeated within the same selection"))
            continue
        seen_in_batch.add(url)
        plan.to_add.append(candidate)
    return plan


def _normalized_url(url: str) -> str | None:
    """Canonical duplicate-check key for one URL, or ``None`` when malformed.

    Normalization is deliberately exact-style, not heuristic: surrounding
    whitespace is stripped, the scheme and host are lowercased and the
    fragment is dropped (it names the same resource). Path, query and port
    are preserved verbatim, so distinct URLs normalize to distinct keys.
    Malformed URLs (e.g. invalid IPv6 brackets) yield ``None`` so one bad
    ``existing_urls`` entry can never match — or be matched — accidentally.
    """
    try:
        parsed = urllib.parse.urlsplit(url.strip())
        return urllib.parse.urlunsplit(
            (
                parsed.scheme.lower(),
                parsed.netloc.lower(),
                parsed.path,
                parsed.query,
                "",  # fragment dropped on purpose
            )
        )
    except ValueError:
        return None


def _reject_reason(candidate_url: str, *, allow_private: bool) -> str | None:
    """A user-actionable rejection reason, or ``None`` when acceptable.

    Mirrors :func:`src.core.url_validator.parse_url_lines` semantics:
    only ``http``/``https`` schemes are accepted and the URL must pass
    :func:`src.core.security.validate_url_for_network` (no bypassing).
    The plan-time check is deliberately a *preflight* (``resolve_dns=False``):
    it never DNS-resolves a hostname, because the fail-closed refusal of
    unresolvable hosts belongs at request time — :meth:`DownloadManager
    .add_download` reruns the full validation anyway, and doing live
    resolver lookups here would make queueing depend on the DNS server
    (an offline machine could not even queue a valid URL). Literal
    private/loopback hosts are still refused (honouring ``allow_private``).
    """
    if not candidate_url:
        return "the candidate has no URL"
    try:
        parsed = urllib.parse.urlsplit(candidate_url)
    except ValueError as exc:
        return f"malformed URL: {exc}"
    if parsed.scheme.lower() not in ("http", "https"):
        return "only http and https URLs are supported"
    try:
        security.validate_url_for_network(
            candidate_url, allow_private=allow_private, resolve_dns=False
        )
    except security.UnsafeUrlError as exc:
        return f"unsafe URL: {exc}"
    except ValueError as exc:
        return f"invalid URL: {exc}"
    return None


@dataclass
class ExecutionReport:
    """What actually happened when the plan was applied to the manager.

    Attributes:
        added: the persisted records returned by ``add_fn``, in plan order.
        failed: ``(label, reason)`` pairs for candidates whose
            ``add_fn`` call raised (``TypeError``/``ValueError``/
            ``RuntimeError`` are all captured; the batch never aborts).
    """

    added: list["DownloadRecord"] = field(default_factory=list)
    failed: list[tuple[str, str]] = field(default_factory=list)


AddDownloadFn = Callable[..., "DownloadRecord"]


def execute_queue_plan(
    plan: QueuePlan,
    destination_dir: str,
    add_fn: AddDownloadFn,
    *,
    start_fn: Callable[[], int] | None = None,
) -> ExecutionReport:
    """Persist every planned candidate through an injected ``add_fn``.

    ``add_fn`` is normally :meth:`DownloadManager.add_download`; any
    rejection (``TypeError`` for a bad call signature included) is
    collected into the report instead of raising, so one bad row never
    sinks the batch. ``start_fn`` (``DownloadManager.start_all``) runs
    once after the batch when at least one download was added.
    """
    report = ExecutionReport()
    for candidate in plan.to_add:
        label = candidate.filename or candidate.candidate_url
        try:
            record = add_fn(
                candidate.candidate_url, destination_dir, filename=candidate.filename
            )
        except (TypeError, ValueError, RuntimeError) as exc:
            reason = str(exc) or type(exc).__name__
            log.warning("candidate %s rejected by add_download: %s", label, reason)
            report.failed.append((label, reason))
        else:
            report.added.append(record)
    if report.added and start_fn is not None:
        start_fn()
    return report
