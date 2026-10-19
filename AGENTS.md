# outline-mcp — pointers for coding agents

Claude Code reads `CLAUDE.md` rather than this name, so `.claude/CLAUDE.md` is a
symlink back to this file and the only thing under `.claude/` this repository
tracks.

- Architecture and crate layout: [README.md](README.md#architecture)
- Disclosure policy — read before anything ships:
  [PUBLIC_DEVELOPMENT.md](PUBLIC_DEVELOPMENT.md)
- Issues, branches, verification, commits, pull requests, and the automated
  pipeline: [CONTRIBUTING.md](CONTRIBUTING.md)
- API detail lives in rustdoc (`cargo doc`); code documentation outranks stale
  issue text.
- Never work on `main`; one worktree per issue under `.worktrees/`.
- `just check` is the definition of green and `just pre-push` is the gate before
  anything is handed over or pushed. Report what was actually run.
- This repository is the whole context. Do not rely on, quote, or ask for
  material from outside it.
