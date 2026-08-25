# CompanyOS

**A self-improving organization of LLM agents that builds itself by using itself, governed by a mechanical harness.**

CompanyOS is an autonomous "software company": four agents (PM, Architect, Implementer, CEO) collaborate through a formal review and governance process to design, implement and evolve projects, including CompanyOS itself. Every change to the system goes through its own rules: task-request, RFC, triaxial review, approval, write permit, implementation, knowledge capture.

What sets this project apart is not the multi-agent orchestration. It is the **harness**: important rules do not live in prompts that models can ignore, they live in code that makes violations impossible.

---

## Guiding principles

**1. Harness before prose.**
A rule written in a prompt is a wish. A rule encoded in a hook, a JSON schema, an MCP tool or a pre-commit is a fact. The project's litmus test: if the YAML memory were wiped tomorrow (`make clean-company`), everything the organization has learned must survive, because learning lives in mechanisms, not in text.

**2. The LLM decides, the algorithm keeps the records.**
Lifecycle transitions (RFC approved, permit sealed, statuses synchronized, indexing) are algorithmic and automatic. Judgment (architecture, curation, arbitration) stays with the agents. No mechanism removes a legitimate decision point; no agent does bookkeeping a machine can do.

**3. The mechanical invariant.**
An anti-pattern is never fixed with a usage rule: the parameter is removed, the schema is locked, the server rejects. The same principle applies at the code level: invariants live in types, never in String content conventions.

---

## Architecture

```
company-os/
├── company/               The system itself
│   ├── personas/          Contracts of the 4 agents (PM, Architect, Implementer, CEO)
│   ├── schemas/           14 JSON Schemas, locked down (unevaluatedProperties)
│   ├── config/            Shared rules, review protocol, protected zones
│   ├── plugins/           JS harness: defense-in-depth (hooks), mcp-proxy (supervision)
│   ├── rfcs/              Request For Change (39+ RFCs, all full-cycle)
│   ├── lessons/           Collective memory (60+ lessons, chained graph)
│   ├── roadmaps/          Domain tracking, auto-synchronized statuses
│   └── scripts/           Enforcement tooling (protected zone)
├── crates/                The Rust server
│   ├── orchestrator/      Engine: hybrid index, review rounds, write permits
│   ├── mcp-servers/       MCP servers (orchestrator, yaml-validator)
│   ├── validation/        Schema validation + kind/path placement
│   └── config/            Config loading, watcher, protected zones
├── projects/              Managed projects (task-requests, design-docs, plans...)
├── .githooks/             Pre-commit: validation, permits, make ci (protected zone)
└── tests/                 Workspace integration tests
```

### The harness, three layers

| Layer | Mechanism | What it makes impossible |
|---|---|---|
| Hooks (JS plugin) | write/edit/bash interception, automatic revert | Writing to a protected zone without a nominative permit, writing under another agent's permit, any file write by the CEO |
| Server (Rust, MCP) | Guards inside the tools | Self-review, approve votes carrying findings, permits without an approved RFC, consuming a permit on a dirty worktree, reviewer lists below the protocol minimum, grants on governance files without confirmed user approval |
| Pre-commit (git) | YAML validation, permit audit, blocking `make ci` | Committing an invalid or misplaced artifact, committing to a protected zone without a permit, merging red code |

### The workflow

```
task-request ──▶ design-doc / RFC ──▶ review round ──▶ CEO approval
     ▲                                (triaxial,           │
     │                                 3 reviewers,        ▼
  lesson-learned ◀── implementation ◀── write permit (sealed in git,
  (chained memory)    (reviewed plan)    nominative, scope pre-check)
```

Every review covers three mandatory axes (nominal, negative, edge cases), a shape enforced by schema. A reviewer cannot approve while carrying corrective findings: the contradiction is rejected server-side. An author cannot review their own artifact: rejected server-side. Write permits are granted by the CEO on approved RFCs only (mechanically verified), cover precise paths, are bound to their grantee, and their scope is automatically compared against the files announced by the RFC.

### Collective memory

YAML artifacts are the source of truth, automatically indexed into SQLite (FTS5 BM25 + deterministic local embeddings + RRF fusion, measured recall 0.895). Lessons-learned form a chained graph (supersedes, derived-from, related) with mechanical detection of dangling links and asymmetric supersessions. The index is a cache: it rebuilds entirely from the YAML files, permits included.

### Resilience

Served binaries live in `target/serve/`, never touched by builds: updating the server is an explicit atomic promotion (`make deploy-serve`). The MCP proxy supervises the server with no terminal state: backoff with re-arming, respawn conditioned on a present binary, transparent FIFO buffering during unavailability. Agents never retry: at worst they receive a structured error that prescribes human escalation.

---

## Getting started

Prerequisites: stable Rust, Node.js 20+, git, [opencode](https://opencode.ai).

```bash
make setup          # release build + CI + promotion of served binaries
opencode            # start a session: the PM is the single interface
```

Useful commands:

```bash
make ci             # fmt + clippy + Rust tests + JS tests + YAML validation + naming
make deploy-serve   # atomic promotion of MCP binaries to target/serve/
make validate       # schema validation of all artifacts
make test-js        # JS harness tests
```

The user only talks to the PM. The PM clarifies intent, creates task-requests and orchestrates the other agents. Structural decisions (protected zones, personas, schemas) are escalated to the user through mechanical triggers.

---

## Project status

**V1** program in progress: a complete overhaul of the system by the system, domain by domain (hygiene, memory, process hardening, personas, schemas, automation, code audits, harness refactor, v1 tag). Progress is tracked in `company/roadmaps/`, every step through a full-cycle RFC. Out of the 62 process rules inventoried, 27 are now mechanical guarantees that would survive a memory wipe.

This repository is both the product and the demonstration: the git history contains the complete decision cycles (RFCs, reviews, sealed permits, lessons) that produced every line.

## License

Personal experimental project. All rights reserved.
