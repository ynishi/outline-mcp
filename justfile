# Gates for this repository. `check` is the definition of green; `pre-push` is the gate
# before anything is pushed. CI runs `check` and `test-pg`.

crates := "outline-mcp-core outline-mcp-rmcp outline-mcp"

default: check

# Formatting, clippy (warnings are errors) and the tests of every crate.
check: fmt-check clippy test

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy -p outline-mcp-core -p outline-mcp-rmcp -p outline-mcp --all-targets -- -D warnings

test:
    for c in {{crates}}; do cargo test -q -p "$c"; done

# Postgres integration tests. Needs OUTLINE_MCP_TEST_DATABASE_URL; a run that skipped is a failure.
test-pg:
    #!/usr/bin/env bash
    set -euo pipefail
    : "${OUTLINE_MCP_TEST_DATABASE_URL:?set OUTLINE_MCP_TEST_DATABASE_URL to a disposable Postgres}"
    out=$(cargo test -q -p outline-mcp-core --test pg -- --nocapture 2>&1) || { printf '%s\n' "$out"; exit 1; }
    printf '%s\n' "$out" | grep -E '^test result' || true
    if printf '%s' "$out" | grep -q 'did NOT run'; then echo "Postgres tests did not run"; exit 1; fi

# Refuse to hand over work done on main, or on a branch not based on the fetched origin/main.
branch-check:
    #!/usr/bin/env bash
    set -euo pipefail
    branch=$(git rev-parse --abbrev-ref HEAD)
    if [ "$branch" = "main" ]; then echo "on main: work on a branch"; exit 1; fi
    git merge-base --is-ancestor origin/main HEAD || { echo "branch is not based on origin/main: rebase or recut it"; exit 1; }

# Commit messages on this branch must not carry AI attribution.
commit-msg-check:
    #!/usr/bin/env bash
    set -euo pipefail
    bad=$(git log --format='%H%n%B' origin/main..HEAD | grep -inE '^(co-authored-by:|signed-off-by:)|generated with' || true)
    if [ -n "$bad" ]; then printf 'AI attribution in commit messages:\n%s\n' "$bad"; exit 1; fi

pre-push: branch-check commit-msg-check check
