"""Tests for ``src.core.retry_policy``: classification, backoff, Retry-After."""

from __future__ import annotations

import email.utils
import time
from collections.abc import Callable

import httpx
import pytest

from src.core.retry_policy import (
    PERMANENT_STATUS_CODES,
    TRANSIENT_STATUS_CODES,
    RetryPolicy,
    TransientHttpError,
)


def _status_error(status: int, headers: dict[str, str]) -> httpx.HTTPStatusError:
    """Build an httpx.HTTPStatusError with a synthetic response."""
    request = httpx.Request("GET", "http://127.0.0.1:9/x")
    response = httpx.Response(status, headers=headers, request=request)
    return httpx.HTTPStatusError(f"HTTP {status}", request=request, response=response)


# -- construction validation -------------------------------------------------------


def test_invalid_policy_arguments_rejected() -> None:
    with pytest.raises(ValueError, match="max_retries"):
        RetryPolicy(max_retries=-1)
    with pytest.raises(ValueError, match="base_delay"):
        RetryPolicy(base_delay=-1)
    with pytest.raises(ValueError, match="max_delay"):
        RetryPolicy(base_delay=5.0, max_delay=1.0)


# -- exponential backoff for transient statuses --------------------------------------


def test_transient_statuses_retry_with_exponential_backoff() -> None:
    policy = RetryPolicy(max_retries=6, base_delay=0.5, max_delay=8.0)
    for status in (429, 500, 502, 503, 504, 408):
        assert policy.should_retry(status, attempt=0) == (True, 0.5)
    # 0.5 * 2**attempt, capped at max_delay
    assert policy.should_retry(503, attempt=1) == (True, 1.0)
    assert policy.should_retry(503, attempt=2) == (True, 2.0)
    assert policy.should_retry(503, attempt=3) == (True, 4.0)
    assert policy.should_retry(503, attempt=4) == (True, 8.0)  # capped
    assert policy.should_retry(503, attempt=5) == (True, 8.0)  # still capped


def test_max_delay_cap_holds() -> None:
    policy = RetryPolicy(max_retries=10, base_delay=2.0, max_delay=3.0)
    delay = policy.should_retry(429, attempt=7)[1]
    assert delay == 3.0


# -- permanent statuses never retry ---------------------------------------------------


@pytest.mark.parametrize("status", sorted(PERMANENT_STATUS_CODES))
def test_permanent_status_never_retries(status: int) -> None:
    policy = RetryPolicy(max_retries=5, base_delay=0.1)
    assert policy.should_retry(status, attempt=0) == (False, 0.0)
    assert policy.should_retry(status, attempt=4) == (False, 0.0)


@pytest.mark.parametrize("status", [404, 410, 403])
def test_task_mandated_permanent_statuses(status: int) -> None:
    policy = RetryPolicy(max_retries=9)
    assert policy.should_retry(status, attempt=0) == (False, 0.0)


def test_transient_and_permanent_sets_do_not_overlap() -> None:
    assert TRANSIENT_STATUS_CODES.isdisjoint(PERMANENT_STATUS_CODES)


# -- Retry-After honoured ----------------------------------------------------------------


def test_retry_after_honoured_honest_values() -> None:
    policy = RetryPolicy(max_retries=3, base_delay=2.0, max_delay=5.0)

    def decide(retry_after: float | str | None) -> tuple[bool, float]:
        return policy.should_retry(TransientHttpError(429, retry_after=retry_after), 0)

    # integer delta-seconds strings are honoured over backoff
    assert decide("3") == (True, 3.0)
    assert decide("0") == (True, 0.0)
    # float retry_after passed programmatically is honoured as-is
    assert decide(2.5) == (True, 2.5)
    # non-integer, non-date strings are untrusted -> exponential backoff
    # (RFC 9110 delta-seconds must be an integer, so "2.5" is treated as junk)
    assert decide("2.5") == (True, 2.0)


def test_retry_after_zero_and_garbage_fall_back_to_backoff() -> None:
    policy = RetryPolicy(max_retries=3, base_delay=0.5)
    # garbage text is unparseable -> falls back to exponential backoff
    should, delay = policy.should_retry(TransientHttpError(429, retry_after="soonish"), 0)
    assert (should, delay) == (True, 0.5)
    # empty string -> backoff as well
    should, delay = policy.should_retry(TransientHttpError(429, retry_after=""), 0)
    assert (should, delay) == (True, 0.5)


def test_retry_after_http_date() -> None:
    policy = RetryPolicy(max_retries=3, base_delay=0.25)
    future = email.utils.formatdate(time.time() + 30, usegmt=True)
    should, delay = policy.should_retry(TransientHttpError(429, retry_after=future), 0)
    assert should is True
    assert 20 <= delay <= 40

    past = email.utils.formatdate(time.time() - 60, usegmt=True)
    should, delay = policy.should_retry(TransientHttpError(429, retry_after=past), 0)
    assert (should, delay) == (True, 0.0)  # negative deltas clamp to zero


def test_transient_http_error_without_retry_after_uses_backoff() -> None:
    policy = RetryPolicy(max_retries=3, base_delay=1.0)
    assert policy.should_retry(TransientHttpError(503), attempt=1) == (True, 2.0)


# -- exception classification (httpx + stdlib) ------------------------------------------


@pytest.mark.parametrize(
    "factory",
    [
        lambda: httpx.ConnectError("connection refused"),
        lambda: httpx.ReadTimeout("timed out"),
        lambda: httpx.WriteTimeout("timed out"),
        lambda: httpx.ConnectTimeout("timed out"),
        lambda: httpx.ReadError("read failed"),
        lambda: httpx.RemoteProtocolError("server disconnected"),
    ],
)
def test_httpx_transport_errors_are_transient(factory: Callable[[], Exception]) -> None:
    policy = RetryPolicy(max_retries=3)
    should, delay = policy.should_retry(factory(), attempt=0)
    assert (should, delay) == (True, policy.base_delay)


def test_httpx_status_error_uses_response_status_and_retry_after() -> None:
    policy = RetryPolicy(max_retries=3, base_delay=0.5)
    assert policy.should_retry(_status_error(503, {}), 0) == (True, 0.5)
    assert policy.should_retry(
        _status_error(429, {"Retry-After": "7"}), 0
    ) == (True, 7.0)
    assert policy.should_retry(_status_error(404, {}), 0) == (False, 0.0)


@pytest.mark.parametrize(
    "factory",
    [
        lambda: ConnectionError("socket closed"),
        lambda: TimeoutError("took too long"),
    ],
)
def test_stdlib_network_errors_are_transient(factory: Callable[[], Exception]) -> None:
    policy = RetryPolicy(max_retries=3, base_delay=1.5)
    assert policy.should_retry(factory(), 0) == (True, 1.5)


def test_unknown_exception_is_permanent() -> None:
    policy = RetryPolicy(max_retries=3)
    assert policy.should_retry(ValueError("bad data"), 0) == (False, 0.0)


# -- max_retries cap ----------------------------------------------------------------------


def test_max_retries_cap_stops_retries() -> None:
    policy = RetryPolicy(max_retries=2, base_delay=0.1)
    assert policy.should_retry(503, attempt=1) == (True, 0.2)
    assert policy.should_retry(503, attempt=2) == (False, 0.0)
    assert policy.should_retry(TransientHttpError(429, retry_after="1"), attempt=2) == (
        False,
        0.0,
    )


def test_zero_retries_policy_never_retries() -> None:
    policy = RetryPolicy(max_retries=0)
    assert policy.should_retry(503, attempt=0) == (False, 0.0)
