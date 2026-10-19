# Public development and information boundaries

outline-mcp develops in public. Everything in this repository — code, issues,
pull requests, commit messages, review records — is written on the assumption
that anyone can read it. This document defines what may cross from private
context into those public artifacts. Coding agents, including the automated
pipeline described in [CONTRIBUTING.md](CONTRIBUTING.md#automated-pipeline), use
it to decide whether something may be written down.

The first line of defence is not a scan but what the author holds: an automated
contribution runs in a clean clone, with this repository as its only context,
so private material is not there to be copied. This policy covers what remains.

An outcome under this policy is never permission to act. ALLOW means a piece of
information may appear in a public artifact; it says nothing about whether a
push, merge, or release may happen. Those operations are governed by
[CONTRIBUTING.md](CONTRIBUTING.md).

## Default

Design choices, code, plans, and the reasoning behind them are public by default
when the project can decide their disclosure without exposing information
entrusted to it by someone else.

Private does not mean "commercial" or "important." It means that another person
or organisation has not agreed to publication, or that disclosure would expose
credentials or an unresolved security issue.

## Classification

### ALLOW

- public code, design alternatives, decisions, and their rationale;
- an internal task identifier, or a plain internal URL without credentials or
  access-bearing query parameters;
- an anonymised statement of a need, such as "users need this capability";
- facts that can be supported by public sources or by this repository.

An internal identifier or plain URL is not a leak by itself. Anonymising a
source must not invent evidence: do not turn one request into "multiple users".

### WARN

Allowed to exist, worth correcting because a self-contained explanation is
easier to maintain — not because it is a disclosure:

- the problem, acceptance criteria, or rationale exists only in an internal
  system;
- an internal link is the sole explanation for a public change.

A WARN is not a publication blocker. Add the public problem and rationale on a
best-effort basis; do not build a guard to make WARN impossible.

### BLOCK

Must not be copied into a public artifact:

- the contents of a private issue, note, or conversation, unless each disclosed
  part has been cleared for publication;
- customer or contributor identity, private conversation, or identifying account
  data without permission;
- contract, NDA, personnel, or other third-party-confidential material;
- credentials, tokens, session material, signed URLs, or URLs with
  access-bearing parameters — including local configuration such as `.env`
  files, MCP client settings, and hosting secrets;
- non-public vulnerability details before coordinated disclosure;
- private measurements or business data whose disclosure rights are not
  established.

If a BLOCK item is found before publication, remove it and carry on. If it is
found after, see [After a disclosure](#after-a-disclosure).

## Third-party material

Committing a file redistributes it. Before adding a file that did not originate
here, state where it came from, its terms, and the notices it needs, beside the
file or in the script that produces it. If that cannot be stated, generate an
equivalent or fetch it at build time and leave it untracked.

## After a disclosure

Once protected content is published, it has been disclosed; deleting the
commit, force-pushing, or making the repository private does not undo that.

- **It can be invalidated** (credentials, tokens, signed URLs): revoke or rotate
  first. Rewriting history is then a tidiness decision, not containment. A crate
  version is not deleted by `cargo yank`.
- **It cannot be invalidated** (personal data, third-party confidential
  material, an unpatched vulnerability): establish what was exposed, for how
  long and how far, and meet the obligations the content carries. Removal still
  happens; it is mitigation, not resolution.

## From private context to public work

Work that starts from private context passes a publication gate first: a new,
self-contained public issue stating the problem, public evidence or an
anonymised need, the desired outcome, public constraints, open questions, and
the relevant locations in this repository. That issue is the contract the
implementation works from, so the implementation never needs the private
source.

## Agent publication check

Before producing a public artifact, an agent checks in this order:

1. **BLOCK:** does the draft contain protected third-party content, credentials,
   token-bearing URLs, local configuration, or non-public security material?
2. **WARN:** can a reader understand the problem and the decision without an
   internal system?
3. **ALLOW:** keep internal IDs and plain URLs when they add provenance; do not
   report them as leaks.
4. **Redistribution:** does the change commit a file that originated elsewhere?
   See [Third-party material](#third-party-material).
