#!/bin/sh
set -eu

# code-server maintains extension metadata next to installed extensions. The
# image root is intentionally read-only, so reconcile the first-party
# extensions into this session's private writable runtime before starting the
# server. This also removes stale `.obsolete` entries and upgrades the
# extension registry without disturbing user-installed extensions.
runtime_extensions=/run/jexactyl-webide/extensions
mkdir -p "$runtime_extensions"
/opt/code-server/lib/node /opt/jexactyl/bin/reconcile-extensions.cjs "$runtime_extensions"

# Copilot Chat stays at code-server's original built-in extension path. The
# product metadata declares GitHub.copilot-chat as a built-in extension and
# its sign-in flow asks the workbench to resolve that built-in ID. Moving it
# into the user extension directory makes code-server try (and fail) to find
# it in the marketplace. It is filtered and pinned at image build time, so no
# runtime download or update is needed.

exec "$@"
