# The twelve-factor methodology

An application built with this kit is a [twelve-factor](https://12factor.net)
application, and not just for configuration — **every factor is load-bearing**.
A factor it violates is a defect, the same as a failed test, because
each one is what lets the process be run, scaled, and disposed of anywhere
without special handling.

This is the authoritative statement of how each factor is realized and enforced.
It is domain-agnostic: it names the *shape* each factor takes here, not any
particular application's realization (PureCard, the decoder this kit currently
builds, exercises a subset — a pure library plus a PyO3 boundary, with no network
service). It grows through reviewer-approved PRs like the rest of the ledger — as
an application gains a config surface, backing services, and a shutdown path, the
corresponding rows gain their concrete enforcement.

| #    | Factor                                                       | What it requires here                                                                                  | Enforced by                                             |
| ---- | ------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------- |
| I    | **Codebase** — one codebase, many deploys                    | One git repo; one change → one worktree → one PR                                                       | constitution §2                                         |
| II   | **Dependencies** — explicit and isolated                     | `Cargo.toml` + `Cargo.lock`; every pin is latest-stable-verified and vetted                            | constitution §2/§4, `cargo deny`                        |
| III  | **Config** — in the environment                              | Every setting loads from the environment through one config struct; nothing baked into the image       | the generated config reference + its drift test (below) |
| IV   | **Backing services** — attached resources                    | Any attached resource (database, queue, object store) is bound by URL/creds at run, swappable in code  | the `infra` layer boundary                              |
| V    | **Build, release, run** — strict separation                  | Image build → immutable release → run; config is bound at run, not build                               | the release pipeline (release-plz)                      |
| VI   | **Processes** — stateless, share-nothing                     | The process holds nothing durable; state lives in a backing service, so any process serves any request | the layering (state lives in `infra`, not `app`)        |
| VII  | **Port binding** — export services via port binding          | The server binds its own port and exports its service directly, with no injected external webserver    | the `server` crate's entry point                        |
| VIII | **Concurrency** — scale out via the process model            | Scale by adding stateless processes; the process is the unit of scale                                  | the stateless-process design                            |
| IX   | **Disposability** — fast startup, graceful shutdown          | Fast startup; on SIGTERM the process drains in-flight work before exit, losing nothing                 | the shutdown handler                                    |
| X    | **Dev/prod parity** — keep environments similar              | Dev and CI exercise the *real* dependencies in throwaway containers, not mocks standing in for them    | integration tests (`testcontainers`), e2e               |
| XI   | **Logs** — treat as event streams                            | Structured `tracing` to stdout only; never a log file, never `println!`                                | constitution §1                                         |
| XII  | **Admin processes** — run one-off admin as one-off processes | One-off admin tasks are `cargo xtask` subcommands and `just` targets, not endpoints                    | `xtask`, `just` targets                                 |

## Config, specifically

Factor III is the one with the most surface area, so it has its own machinery.
The pattern — apply it the moment the application grows its first setting — is what
keeps configuration honest:

- **One source of truth.** A single config struct declares every setting with
  its env var name, default, and a doc-comment. Nothing reads the environment
  ad hoc; a setting that isn't a field on the struct doesn't exist.
- **Self-documenting.** The configuration reference — a single page a human reads
  to learn every knob — is **generated** from that struct, not hand-written.
- **Verifiable.** A `*_doc_stays_current` test (run in CI) regenerates the
  reference and fails if the checked-in copy has drifted, so the reference can
  **never lie**. Regenerate it with the generator `just` target.

Adding a setting means adding a field to the struct and regenerating — the doc,
the env var, and the default all follow from the one declaration. This is the
same "a figure a doc cites must be machine-asserted against its source, not
hand-copied" principle the lessons ledger records: the config reference is a
generated figure, and its drift test is what stops it rotting.
