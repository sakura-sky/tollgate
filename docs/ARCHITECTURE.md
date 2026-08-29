# Architecture

How Tollgate is put together, for operators and contributors. For persistence,
egress hardening, and retention runbooks see [`OPERATIONS.md`](./OPERATIONS.md).

## Components

```mermaid
flowchart TB
  subgraph client["Client side"]
    app["Your apps"]
    ops["Operator / finance"]
  end

  subgraph tollgate["Tollgate gateway (single binary)"]
    router["HTTP router (axum)"]
    core["GatewayCore<br/>auth · pricing · admission · settle"]
    console["Read-only console<br/>/console, /console/budgets, /console/usage"]
    reload["Config hot-reload task"]
    maint["Ledger maintenance task"]
  end

  subgraph state["State"]
    pg[("Postgres<br/>keys · budgets · prices<br/>usage ledger (partitioned) · audit")]
    vk[("Valkey<br/>budget counters (hot path)")]
  end

  prov["Anthropic / Vertex"]

  app -->|"x-tollgate-key"| router --> core
  ops --> console
  core -->|"verify key"| pg
  core <-->|"reserve / settle (atomic Lua)"| vk
  core -->|"append usage"| pg
  core -->|"forward"| prov
  console -->|"read"| pg
  reload -->|"load budgets / prices"| pg
  reload -->|"swap live config"| core
  maint -->|"create-ahead / drop old partitions"| pg
```

The whole gateway is one Rust binary. The same request flow (`GatewayCore`) backs
both `tollgate demo` (in-memory) and `tollgate serve` (Postgres + Valkey), so the
demo exercises the real logic.

## Request lifecycle

```mermaid
sequenceDiagram
  participant C as Client
  participant T as Tollgate
  participant DB as Postgres
  participant V as Valkey
  participant P as Provider
  C->>T: POST /v1/{provider}/{path}
  T->>T: parse key, HMAC-SHA256 verify (constant time)
  Note over T: unknown provider/model -> 402 (fail closed)
  T->>T: price by token (integer micros)
  opt admission = exact
    T->>P: pre-flight count_tokens
  end
  T->>V: reserve worst-case cost (atomic check + incr)
  alt would exceed a hard cap
    T->>DB: record decision = rejected_budget
    T-->>C: 429 (never reaches provider)
  else within budget
    T->>P: forward request
    P-->>T: response + usage
    T->>V: settle reservation to actual cost
    T->>DB: append usage row (cost, tokens, overhead)
    T-->>C: 200 + cost + x-tollgate-overhead-us
  end
```

Budgets that apply to a request are its API key, the provider, the model, and the
mandatory global backstop; every applicable hard cap is checked before the
forward. A request that matches no budget is denied, never allowed (fail closed).

## Data model

```mermaid
erDiagram
  api_keys ||--o{ usage_events : "spends under"
  api_keys ||--o{ budgets : "may be scoped to"
  model_prices ||--o{ usage_events : "prices"
  api_keys {
    uuid id PK
    text prefix "public lookup handle"
    text key_hash "HMAC-SHA256 under pepper"
    timestamptz revoked_at
  }
  budgets {
    text scope_type "global|api_key|provider|model"
    text scope_id
    text period "daily|weekly|monthly"
    bigint limit_micros
    bool hard_stop
  }
  model_prices {
    text provider
    text model
    bigint input_per_1m_micros
    bigint output_per_1m_micros
    timestamptz effective_to "null = current"
  }
  usage_events {
    uuid id
    uuid api_key_id FK
    bigint cost_micros
    bigint gateway_micros "our overhead"
    text decision
    timestamptz started_at "partition key"
  }
```

`usage_events` is append-only (triggers reject UPDATE, DELETE, and TRUNCATE) and
monthly range-partitioned so retention can drop old partitions without violating
immutability. `audit_log` is likewise append-only.

## Money and enforcement invariants

- **Integer micros, never floats.** All money is `1e-6` of the deployment's
  configured currency unit; cost math uses `i128` intermediates and rounds once.
- **Reserve then settle.** The worst-case cost is reserved before the forward and
  settled to the provider's exact reported usage after, so the ledger is exact and
  a hard cap cannot be blown by an unknown output length.
- **Atomic enforcement.** The Valkey reserve is a single Lua check-and-increment;
  the settle is a single Lua apply. A cache failure fails closed (503), not a
  budget bypass.
- **Postgres is the system of record.** Valkey counters are a hot-path cache; on
  startup they are reconciled from the ledger, and reconcile only ever raises a
  counter (`max(counter, ledger)`), never lowers it.
- **Mandatory backstop.** A request matching no budget is denied.
- **Auth.** Keys are `tgk_<pub>_<secret>`; only an HMAC of the secret under a
  server-side pepper is stored, compared in constant time, with a dummy-hash
  verify on prefix miss so hit and miss cost the same.

## Storage split

Postgres holds the durable truth: keys, budgets, prices, the usage ledger, and
the audit log. Valkey holds only the current-period spend counters for the hot
path (sub-millisecond reserve/settle), reconciled from Postgres so a cache flush
cannot reset a budget. Retention is by monthly partition drop on the ledger; the
counters expire on their own TTL and are period-scoped, so they are unaffected.

## Configuration reload and retention

Two background tasks keep a running gateway current without restarts: a reload
task swaps budgets and prices in atomically on an interval, and a maintenance task
creates ledger partitions ahead of need and drops those older than the retention
window. Both are described in [`OPERATIONS.md`](./OPERATIONS.md).
