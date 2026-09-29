#!/bin/bash
# Run an explicit poster action. gitlab_api.py loads only its non-secret
# machine-local settings; credentials stay in its token file.
set -eo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$HERE/review_post.py" "$@"
