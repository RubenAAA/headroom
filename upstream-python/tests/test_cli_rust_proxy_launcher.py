from __future__ import annotations

import pytest
from click.testing import CliRunner

from headroom.cli import proxy as proxy_mod
from headroom.cli.main import main


class _Completed:
    returncode = 0


def test_proxy_defaults_to_rust_binary(monkeypatch):
    monkeypatch.delenv("HEADROOM_USE_PYTHON_PROXY", raising=False)
    captured: dict[str, object] = {}

    def fake_run(command, env):  # noqa: ANN001
        captured["command"] = command
        captured["env"] = env
        return _Completed()

    monkeypatch.setattr(proxy_mod.subprocess, "run", fake_run)

    result = CliRunner().invoke(
        main,
        ["proxy"],
        env={"HEADROOM_PROXY_BINARY": "/tmp/headroom-proxy"},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, result.output
    command = captured["command"]
    assert command[0] == "/tmp/headroom-proxy"
    assert command[command.index("--listen") + 1] == "127.0.0.1:8787"
    assert command[command.index("--upstream") + 1] == "https://api.anthropic.com"
    assert "--compression" in command
    assert command[command.index("--compression-mode") + 1] == "all_messages"
    assert command[command.index("--mode") + 1] == "token"


def test_proxy_no_optimize_maps_to_rust_passthrough(monkeypatch):
    monkeypatch.delenv("HEADROOM_USE_PYTHON_PROXY", raising=False)
    captured: dict[str, object] = {}

    def fake_run(command, env):  # noqa: ANN001
        captured["command"] = command
        return _Completed()

    monkeypatch.setattr(proxy_mod.subprocess, "run", fake_run)

    result = CliRunner().invoke(
        main,
        ["proxy", "--no-optimize"],
        env={"HEADROOM_PROXY_BINARY": "/tmp/headroom-proxy"},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, result.output
    command = captured["command"]
    assert "--compression" not in command
    assert "--compression-mode" not in command


def test_proxy_no_ccr_proactive_expansion_maps_to_rust_flag(monkeypatch):
    monkeypatch.delenv("HEADROOM_USE_PYTHON_PROXY", raising=False)
    captured: dict[str, object] = {}

    def fake_run(command, env):  # noqa: ANN001
        captured["command"] = command
        return _Completed()

    monkeypatch.setattr(proxy_mod.subprocess, "run", fake_run)

    result = CliRunner().invoke(
        main,
        ["proxy", "--no-ccr-proactive-expansion"],
        env={"HEADROOM_PROXY_BINARY": "/tmp/headroom-proxy"},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, result.output
    assert "--ccr-proactive-expansion=false" in captured["command"]


def test_python_proxy_escape_hatch_keeps_legacy_path(monkeypatch):
    pytest.importorskip("fastapi")

    captured: dict[str, object] = {}

    def fake_run_server(config, **kwargs):  # noqa: ANN001
        captured["config"] = config
        captured["kwargs"] = kwargs

    import headroom.proxy.server as server_mod

    monkeypatch.setattr(server_mod, "run_server", fake_run_server)

    result = CliRunner().invoke(
        main,
        ["proxy"],
        env={"HEADROOM_USE_PYTHON_PROXY": "1"},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, result.output
    assert captured["config"].host == "127.0.0.1"
    assert captured["kwargs"]["print_banner"] is False


def test_active_unsupported_flag_fails_loudly_under_rust(monkeypatch):
    monkeypatch.delenv("HEADROOM_USE_PYTHON_PROXY", raising=False)

    result = CliRunner().invoke(
        main,
        ["proxy", "--code-graph"],
        env={"HEADROOM_PROXY_BINARY": "/tmp/headroom-proxy"},
    )

    assert result.exit_code != 0
    assert "not supported by the Rust proxy in Phase 1" in result.output
    assert "--code-graph" in result.output


def test_memory_runs_on_the_rust_proxy(monkeypatch):
    """`--memory` used to be refused outright, which left the Rust proxy's
    working memory implementation unreachable from the launcher."""
    monkeypatch.delenv("HEADROOM_USE_PYTHON_PROXY", raising=False)
    captured: dict[str, object] = {}

    def fake_run(command, env):  # noqa: ANN001
        captured["env"] = env
        return _Completed()

    monkeypatch.setattr(proxy_mod.subprocess, "run", fake_run)

    result = CliRunner().invoke(
        main,
        ["proxy", "--memory", "--memory-top-k", "25", "--no-memory-context"],
        env={"HEADROOM_PROXY_BINARY": "/tmp/headroom-proxy"},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, result.output
    env = captured["env"]
    assert env["HEADROOM_MEMORY_ENABLED"] == "1"
    assert env["HEADROOM_MEMORY_TOP_K"] == "25"
    assert env["HEADROOM_MEMORY_INJECT_CONTEXT"] == "0"
    assert "HEADROOM_MEMORY_INJECT_TOOLS" not in env


@pytest.mark.parametrize(
    ("argv", "expected"),
    [
        (["--memory", "--memory-storage", "global"], "--memory-storage"),
        (["--memory", "--memory-project-root", "/tmp/x"], "--memory-project-root"),
        (["--memory", "--memory-qdrant-url", "http://q:6333"], "--memory-qdrant-*"),
    ],
)
def test_a_store_the_rust_proxy_cannot_open_is_still_refused(monkeypatch, argv, expected):
    monkeypatch.delenv("HEADROOM_USE_PYTHON_PROXY", raising=False)

    result = CliRunner().invoke(
        main,
        ["proxy", *argv],
        env={"HEADROOM_PROXY_BINARY": "/tmp/headroom-proxy"},
    )

    assert result.exit_code != 0
    assert expected in result.output


def test_rust_proxy_env_forwarding(monkeypatch):
    """The launcher must inject HEADROOM_MEMORY_ENABLED, RPM, TPM, cache
    TTL, and cache max entries into the subprocess env so the Rust proxy
    can read them."""
    monkeypatch.delenv("HEADROOM_USE_PYTHON_PROXY", raising=False)
    captured: dict[str, object] = {}

    def fake_run(command, env):  # noqa: ANN001
        captured["command"] = command
        captured["env"] = env
        return _Completed()

    monkeypatch.setattr(proxy_mod.subprocess, "run", fake_run)

    result = CliRunner().invoke(
        main,
        ["proxy"],
        env={"HEADROOM_PROXY_BINARY": "/tmp/headroom-proxy"},
        catch_exceptions=False,
    )

    assert result.exit_code == 0, result.output
    env = captured["env"]
    # Defaults: memory disabled, rpm=60, tpm=100000
    assert env["HEADROOM_MEMORY_ENABLED"] == "0"
    assert env["HEADROOM_RPM"] == "60"
    assert env["HEADROOM_TPM"] == "100000"
