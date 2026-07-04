"""Bash-search lossless fold.

`bash` is not an excluded tool, so its output normally takes the lossy strategy
path. But a read-only search run through it (grep/rg/git grep) produces byte-
losslessly foldable output — the router detects the *command* and folds it with
the same ripgrep --heading transform excluded Grep gets, instead of lossy
compression. Non-search bash commands (cat/build/mutate) are untouched.
"""

from __future__ import annotations

import json

import pytest

from headroom.providers import OpenAIProvider
from headroom.tokenizer import Tokenizer
from headroom.transforms.content_router import (
    ContentRouter,
    ContentRouterConfig,
    _bash_command_is_search,
    _bash_program,
)
from headroom.transforms.lossless_compaction import search_unheading

SEARCH = frozenset({"grep", "egrep", "fgrep", "rg", "ripgrep", "ag", "ack"})
GREP = "".join(
    f"src/module_{f}.py:{ln * 3}:matched occurrence with some real content here\n"
    for f in range(6)
    for ln in range(15)
)
CODE = "def foo(x):\n    return x + 1\n\nclass Bar:\n    value = 42\n" * 30


@pytest.fixture
def tokenizer():
    provider = OpenAIProvider()
    return Tokenizer(provider.get_token_counter("gpt-4o"), "gpt-4o")


# --- command parsing: peel wrappers, detect search programs ---


@pytest.mark.parametrize(
    "command",
    [
        "grep -rn foo .",
        "rtk grep def headroom/transforms",  # the user's token-proxy wrapper
        "rg --heading pattern src/",
        "git grep -n TODO",
        "sudo grep root /etc/passwd",
        "timeout 30 rg foo",  # wrapper takes a numeric arg
        "FOO=1 BAR=2 grep foo",  # env assignments
        "/usr/bin/grep -rn foo .",  # absolute path
        'bash -lc "grep -rn foo ."',  # Codex-style shell -c
        "nice -n 5 ack pattern",  # wrapper with option arg
    ],
)
def test_detects_search_commands(command):
    assert _bash_command_is_search(command, SEARCH) is True


@pytest.mark.parametrize(
    "command",
    [
        "cat headroom/server.py",
        "cargo test",
        "pytest tests/ -x",
        "git diff HEAD~1",  # diff, NOT search
        "echo grep",  # echo, not a real grep
        "python script.py",
        "rm -rf build",
        "ls -la",
    ],
)
def test_ignores_non_search_commands(command):
    assert _bash_command_is_search(command, SEARCH) is False


def test_bash_program_peels_wrappers():
    assert _bash_program("rtk grep foo")[0] == "grep"
    assert _bash_program("timeout 30 rg x")[0] == "rg"
    assert _bash_program("FOO=1 /usr/bin/grep y")[0] == "grep"
    assert _bash_program("")[0] == ""


# --- end-to-end through the router (both wire formats) ---


def _openai(command: str, content: str, tokenizer):
    router = ContentRouter(ContentRouterConfig())
    messages = [
        {
            "role": "assistant",
            "tool_calls": [
                {
                    "id": "c1",
                    "function": {"name": "bash", "arguments": json.dumps({"command": command})},
                }
            ],
        },
        {"role": "tool", "tool_call_id": "c1", "content": content},
    ]
    result = router.apply(messages, tokenizer, compress_user_messages=True)
    return result.messages[1]["content"], result.transforms_applied


def _anthropic(command: str, content: str, tokenizer):
    router = ContentRouter(ContentRouterConfig())
    messages = [
        {
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": command}}
            ],
        },
        {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": content}],
        },
    ]
    result = router.apply(messages, tokenizer, compress_user_messages=True)
    return result.messages[1]["content"][0]["content"], result.transforms_applied


def test_openai_bash_grep_folds_and_recovers(tokenizer):
    out, transforms = _openai("grep -rn foo .", GREP, tokenizer)
    assert "router:bash:lossless_search" in transforms
    assert len(out) < len(GREP)
    assert search_unheading(out) == GREP  # byte-exact


def test_anthropic_bash_rtk_grep_folds_and_recovers(tokenizer):
    out, transforms = _anthropic("rtk grep foo headroom/", GREP, tokenizer)
    assert "router:bash:lossless_search" in transforms
    assert search_unheading(out) == GREP


def test_non_search_bash_command_not_folded(tokenizer):
    # `cat` is not a search — must NOT take the bash-search fold.
    _out, transforms = _openai("cat headroom/server.py", GREP, tokenizer)
    assert "router:bash:lossless_search" not in transforms


def test_source_output_from_search_command_untouched(tokenizer):
    # Command is a search, but the output isn't path:line:content — the
    # reversibility guard makes compact_lossless return it unchanged.
    out, transforms = _openai("grep -l foo", CODE, tokenizer)
    assert "router:bash:lossless_search" not in transforms
    assert out == CODE
