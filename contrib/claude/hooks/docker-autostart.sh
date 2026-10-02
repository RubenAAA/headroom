#!/usr/bin/env bash
# docker-autostart.sh: start the local Docker engine after an assistant message
# mentions Docker, then continue the turn with the confirmed state injected.
#
# Stop and SubagentStop provide the completed assistant text. When Docker is
# down, this hook opens Docker Desktop on macOS/Windows/WSL or starts the
# available Docker service on Linux. Exit 2 feeds the result back to the model
# as a system message. A per-session marker suppresses repeat starts during
# that continuation; the next user prompt clears it.
set -u

INPUT=$(cat) || exit 0
EVENT=$(printf '%s' "$INPUT" | jq -r '.hook_event_name // empty' 2>/dev/null)
SESSION=$(printf '%s' "$INPUT" | jq -r '.session_id // empty' 2>/dev/null | tr -cd 'A-Za-z0-9_-')
[ -n "$SESSION" ] || exit 0

STATE_DIR="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/headroom-docker-autostart-${UID:-$(id -u)}"
MARKER="$STATE_DIR/$SESSION.injected"

if [ "$EVENT" = "UserPromptSubmit" ]; then
  rm -f "$MARKER" 2>/dev/null || true
  exit 0
fi

case "$EVENT" in
  Stop|SubagentStop) ;;
  *) exit 0 ;;
esac

MESSAGE=$(printf '%s' "$INPUT" | jq -r '.last_assistant_message // empty' 2>/dev/null)
# An incidental mention ("Docker is not needed here") must not launch Docker
# Desktop. Trigger on a docker command, docker-compose, or the engine itself.
DOCKER_CMD='docker([[:space:]]+compose)?[[:space:]]+(run|ps|build|buildx|compose|exec|pull|push|images|info|logs|stop|start|restart|up|down|network|volume|container|image|system|login|rm|rmi|inspect|cp)([^[:alnum:]_]|$)'
DOCKER_ENGINE='docker-compose|docker[[:space:]]+(daemon|engine|desktop)|docker\.sock|docker[[:space:]]+is[[:space:]]+(not[[:space:]]+)?running'
printf '%s' "$MESSAGE" | grep -qiE "(^|[^[:alnum:]_-])($DOCKER_CMD|$DOCKER_ENGINE)" || exit 0

mkdir -p "$STATE_DIR" 2>/dev/null || exit 0
chmod 700 "$STATE_DIR" 2>/dev/null || true
# Atomic create: overlapping Stop/SubagentStop hooks for this session should
# share one startup attempt and one continuation notice.
(set -o noclobber; : > "$MARKER") 2>/dev/null || exit 0

# A stopped engine refuses the connection at once, so the limit only matters
# for a slow one. A busy machine (a release build, say) can take over 2 s to
# answer `docker info`; reading that as "down" made the hook report a running
# Docker as dead.
PROBE_SECONDS=10

docker_ready() {
  if command -v timeout >/dev/null 2>&1; then
    timeout "$PROBE_SECONDS" docker info >/dev/null 2>&1
    return $?
  fi
  if command -v gtimeout >/dev/null 2>&1; then
    gtimeout "$PROBE_SECONDS" docker info >/dev/null 2>&1
    return $?
  fi

  # macOS does not ship `timeout`; bound the probe without adding a dependency.
  docker info >/dev/null 2>&1 &
  local docker_pid=$!
  (sleep "$PROBE_SECONDS"; kill "$docker_pid" 2>/dev/null || true) &
  local watchdog_pid=$!
  wait "$docker_pid" 2>/dev/null
  local result=$?
  kill "$watchdog_pid" 2>/dev/null || true
  wait "$watchdog_pid" 2>/dev/null || true
  return "$result"
}

start_windows_desktop() {
  local powershell
  if command -v powershell.exe >/dev/null 2>&1; then
    powershell=$(command -v powershell.exe)
  elif command -v pwsh.exe >/dev/null 2>&1; then
    powershell=$(command -v pwsh.exe)
  else
    return 1
  fi

  local script='
$paths = @(
  (Join-Path $env:ProgramFiles "Docker\Docker\Docker Desktop.exe"),
  (Join-Path $env:LOCALAPPDATA "Programs\Docker\Docker\Docker Desktop.exe")
)
$exe = $paths | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $exe) { exit 1 }
Start-Process -FilePath $exe
'
  MSYS_NO_PATHCONV=1 "$powershell" -NoLogo -NoProfile -NonInteractive \
    -ExecutionPolicy Bypass -Command "$script" >/dev/null 2>&1
}

start_linux_docker() {
  local uid
  uid=$(id -u 2>/dev/null || echo 1)

  # Rootless Docker commonly has a per-user systemd unit.
  if [ "$uid" -ne 0 ] && command -v systemctl >/dev/null 2>&1 &&
     systemctl --user start docker >/dev/null 2>&1; then
    return 0
  fi

  if command -v systemctl >/dev/null 2>&1; then
    if [ "$uid" -eq 0 ]; then
      systemctl start docker >/dev/null 2>&1 && return 0
    elif command -v sudo >/dev/null 2>&1; then
      sudo -n systemctl start docker >/dev/null 2>&1 && return 0
    fi
  fi

  if command -v service >/dev/null 2>&1; then
    if [ "$uid" -eq 0 ]; then
      service docker start >/dev/null 2>&1 && return 0
    elif command -v sudo >/dev/null 2>&1; then
      sudo -n service docker start >/dev/null 2>&1 && return 0
    fi
  fi
  return 1
}

start_docker() {
  local os release
  os=$(uname -s 2>/dev/null || echo unknown)
  release=$(uname -r 2>/dev/null || echo '')

  # Check WSL before Linux: Docker Desktop runs on the Windows host.
  if [ -n "${WSL_DISTRO_NAME:-}" ] || [ -n "${WSL_INTEROP:-}" ] ||
     printf '%s' "$release" | grep -qiE 'microsoft|wsl'; then
    start_windows_desktop && return 0
    # No Docker Desktop on the host: a Docker engine installed inside the
    # distro is the other WSL setup.
    start_linux_docker && return 0
    START_FAILURE='could not launch Docker Desktop from WSL or start a Docker service in the distro'
    return 1
  fi

  case "$os" in
    Darwin)
      if command -v open >/dev/null 2>&1 && open -a Docker >/dev/null 2>&1; then
        return 0
      fi
      START_FAILURE='could not open Docker Desktop on macOS'
      ;;
    Linux)
      if start_linux_docker; then
        return 0
      fi
      START_FAILURE='no Docker service start command succeeded on Linux (a privileged service may need manual startup)'
      ;;
    MINGW*|MSYS*|CYGWIN*)
      if start_windows_desktop; then
        return 0
      fi
      START_FAILURE='could not launch Docker Desktop through PowerShell on Windows'
      ;;
    *)
      START_FAILURE="no Docker startup method is configured for $os"
      ;;
  esac
  return 1
}

if ! command -v docker >/dev/null 2>&1; then
  NOTICE='Docker was mentioned, but the Docker CLI is not installed in this environment, so Headroom could not check or start Docker. Do not assume Docker is available.'
  printf '%s\n' "$NOTICE" >&2
  exit 2
fi

if docker_ready; then
  rm -f "$MARKER" 2>/dev/null || true
  exit 0
fi

START_FAILURE='Docker did not become ready after the startup attempt'
if start_docker; then
  ready=0
  attempt=0
  while [ "$attempt" -lt 30 ]; do
    if docker_ready; then
      ready=1
      break
    fi
    sleep 1
    attempt=$((attempt + 1))
  done
  if [ "$ready" -eq 1 ]; then
    NOTICE='Docker was started automatically and `docker info` confirms it is responding. Continue with the Docker-related task.'
  else
    NOTICE="Headroom tried to start Docker, but it is still not responding to docker info ($START_FAILURE). Do not assume Docker is available."
  fi
else
  NOTICE="Docker was not running. Headroom could not start it: $START_FAILURE. Do not assume Docker is available."
fi

printf '%s\n' "$NOTICE" >&2
exit 2
