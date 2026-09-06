"""PR-F3 — subscription tracker token hardening.

The tracker must never hold a raw OAuth bearer in memory: `notify_active`
retains only a one-way hash + last-4 (`_current_token_id`), and polling reads
the token from the credentials file / env var at poll time.
"""

from __future__ import annotations

import pytest

from headroom.subscription.tracker import SubscriptionTracker, _token_id

SECRET = "sk-ant-oat01-verySecretBearerMaterial-Zw9q"


@pytest.fixture()
def tracker(monkeypatch: pytest.MonkeyPatch) -> SubscriptionTracker:
    monkeypatch.setattr(SubscriptionTracker, "_load_persisted_state", lambda self: None)
    return SubscriptionTracker(enabled=True)


def test_raw_token_not_stored_in_memory(tracker: SubscriptionTracker) -> None:
    tracker.notify_active(f"Bearer {SECRET}")

    # No attribute on the tracker may contain the raw bearer (the 8-char
    # format prefix in _full_tokens is scheme identification, not secret
    # material — check the secret body instead).
    secret_body = SECRET[len("sk-ant-oat01-") :]
    for name, value in vars(tracker).items():
        assert secret_body not in repr(value), f"raw token leaked into {name}"

    token_id = tracker._current_token_id
    assert token_id is not None
    assert token_id.startswith("sha256:")
    assert token_id.endswith(SECRET[-4:])


def test_token_id_is_one_way_and_deterministic() -> None:
    a = _token_id(SECRET)
    assert a == _token_id(SECRET)
    assert SECRET not in a
    # 16 hex chars of sha256 + last-4 only.
    assert len(a) == len("sha256:") + 16 + 1 + 4


def test_poll_reads_token_from_credentials_not_memory(
    tracker: SubscriptionTracker, monkeypatch: pytest.MonkeyPatch
) -> None:
    tracker.notify_active(f"Bearer {SECRET}")
    seen: list[str | None] = []

    async def fake_fetch(token: str | None):
        seen.append(token)
        return None

    tracker._client = type("C", (), {"fetch": staticmethod(fake_fetch)})()
    monkeypatch.setattr(
        "headroom.subscription.client.read_cached_oauth_token", lambda: "disk-token"
    )

    import asyncio

    asyncio.run(tracker._maybe_poll())
    assert seen == ["disk-token"], "poll must use the credentials-file token"
