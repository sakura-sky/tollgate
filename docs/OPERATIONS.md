# Operations

Operational notes for running Tollgate in production: how spend state survives
restarts, what the network posture is, and the reference architectures for
locking egress down further.

## Budget durability and persistence

Tollgate keeps two copies of every charge.

The **ledger in Postgres** is the system of record. Every admitted request writes
an append-only usage row with its exact cost once the provider response is
settled. Postgres is the source of truth for spend and for billing.

The **counters in Valkey** are a fast cache used on the hot path. Reserving and
settling budget against Valkey with an atomic script is what lets the gateway
hard-stop before overspend in well under a millisecond, without a database round
trip per request.

Because the ledger is authoritative, the cache is disposable: on startup the
gateway runs `reconcile_counters`, which sums the ledger over each budget's
current period window and rebuilds the Valkey counters from it. A flushed or
cold cache cannot reset a budget to zero and let spend through.

Valkey is still configured for durability so a routine restart does not force a
full reconcile storm and a momentary cold window:

- **Local (compose):** `valkey-server --appendonly yes` with a named volume, so
  the append-only file survives `docker compose restart`.
- **GCP (Memorystore for Valkey):** `persistence_config` is set to `AOF` with
  `append_fsync = EVERY_SEC`, trading at most one second of cached counter
  writes on a hard node failure. The ledger still backs the reconcile, so even a
  total cache loss is recoverable, just slower on the next boot.

If you deliberately want a cold cache (for example after changing budget periods),
restart the gateway and let reconcile rebuild the counters from the ledger.

## Ledger retention

`usage_events` is append-only: a trigger rejects `UPDATE` and `DELETE` for every
role, including the table owner, so spend history cannot be quietly rewritten.
That immutability is deliberate, but it means the ledger cannot be trimmed with a
`DELETE` purge. Retention is therefore done by time partitioning.

The table is `RANGE`-partitioned by `started_at` into monthly partitions. The
gateway runs a maintenance routine at startup and every few hours that:

- creates the current and next month's partitions ahead of need, so inserts
  always have a home; and
- drops any partition whose whole month is older than
  `TOLLGATE_RETENTION__WINDOW` (default 90 days). Dropping a partition is DDL, so
  it bypasses the append-only row trigger while the rows that remain stay
  immutable.

Set `TOLLGATE_RETENTION__WINDOW=0s` to keep everything (create-ahead only, no
drops). Granularity is monthly, so at least the current month is always retained
and the effective retention rounds up to a whole month. The budget counters are
unaffected: they are driven by the current period's rows, which are always well
within any sane retention window, and Postgres partition pruning on `started_at`
keeps the spend-sum queries fast as the ledger grows.

For very high volume, reduce the window or switch the partition granularity to
weekly or daily in `tollgate_usage_maintain` (migration `0006`); the same
create-ahead and drop-old logic applies.

## Network and egress posture (baseline)

The default Terraform deploys a private posture, not an open one:

- Cloud Run runs with no public egress path of its own; it reaches Cloud SQL and
  Valkey over internal ranges through the Serverless VPC Access connector.
- The provider adapters only ever connect to fixed provider hostnames. A caller
  supplies a provider name and a path suffix, both allowlisted; they never supply
  a destination host. There is no user-controlled URL in the forward path.
- The outbound HTTP client refuses redirects (`redirect::Policy::none()`), so a
  `3xx` from an upstream cannot bounce the request, and its credentials, to a
  different host.
- Secrets (database URL, API-key pepper) are read from Secret Manager by
  reference and scoped to the runtime service account per-secret, not granted
  project-wide.

The practical consequence is that Tollgate's server-side request forgery surface
is small by construction: the set of hosts it will connect to is fixed in code
and configuration. The reference architecture below closes the residual gap,
which is that the *network* itself still permits arbitrary outbound TLS.

### Reference architecture A: baseline (shipped default)

```
                 Serverless VPC Access connector
   ┌────────────┐   (private ranges only)      ┌──────────────────┐
   │ Cloud Run  │ ───────────────────────────► │ Cloud SQL (PSA)  │
   │  Tollgate  │ ───────────────────────────► │ Valkey (PSC)     │
   │            │                               └──────────────────┘
   │            │        public egress (TLS)     ┌──────────────────┐
   │            │ ─────────────────────────────► │ api.anthropic.com│
   │            │ ─────────────────────────────► │ *.googleapis.com │
   └────────────┘                                │ metadata server  │
                                                 └──────────────────┘
```

Private dependencies are pinned to the VPC. Provider egress leaves over Google's
default path; the *application* restricts destinations, the network does not.

### Reference architecture B: pinned egress (Secure Web Proxy)

For deployments that need the network itself to enforce the destination
allowlist (regulated environments, strict data-exfiltration controls), route all
egress through a Secure Web Proxy (SWP) and deny everything not on the list.

```
   ┌────────────┐   all egress via connector    ┌──────────────────────┐
   │ Cloud Run  │ ────────────────────────────► │ Secure Web Proxy     │
   │  Tollgate  │  (egress = ALL_TRAFFIC)        │  FQDN allowlist:     │
   │            │                                │   api.anthropic.com  │
   └────────────┘                                │   *.googleapis.com   │
        │ private ranges                         │   metadata server    │
        ▼                                        └───────────┬──────────┘
   ┌──────────────────┐                                      │ allow
   │ Cloud SQL (PSA)  │                          deny all ◄──┘
   │ Valkey (PSC)     │                          other hosts
   └──────────────────┘
```

To deploy this lockdown:

1. Set the Cloud Run `vpc_access` egress to `ALL_TRAFFIC` so every outbound
   flow, not just private ranges, traverses the connector.
2. Provision a Cloud NAT on the connector subnet (public providers need a source
   for their return path) and a Secure Web Proxy gateway
   (`google_network_services_gateway` of type `SECURE_WEB_GATEWAY`) with a
   `google_network_security_gateway_security_policy`.
3. Add policy rules that **allow** TLS to `api.anthropic.com`,
   `*.googleapis.com`, and the metadata server `169.254.169.254`, and a
   lowest-priority rule that **denies** everything else.
4. Point the workload at the proxy and confirm a call to any other host is
   refused while provider calls still succeed.

This is deliberately left out of the default module: it is billable, always-on
infrastructure, and the allowlist must be validated against your own project and
the exact provider endpoints you call. Treat architecture A as the secure
default and architecture B as the hardened profile you enable when the threat
model calls for it.

Note that a pure VPC firewall cannot substitute for the proxy here: firewall
rules match on IP, and Anthropic's API is served from a large rotating set of
CDN addresses, so IP allowlisting can pin the Google APIs (which have a stable
restricted VIP) but not the Anthropic path. Domain-level pinning needs SWP.

## Database privileges (defence in depth)

The `usage_events` and `audit_log` tables are append-only, and that guarantee is
enforced inside the database by triggers that reject `UPDATE` and `DELETE`. The
trigger holds even against the table owner, so it is the primary control.

As a second layer, run the application against a non-owner Postgres role rather
than the role that owns the schema, and revoke the mutating grants it does not
need:

```sql
-- Run migrations as the owner, then create a least-privilege app role.
CREATE ROLE tollgate_app LOGIN PASSWORD '...';
GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA public TO tollgate_app;
GRANT SELECT, UPDATE ON api_keys TO tollgate_app;      -- last_used_at, revoked_at
REVOKE UPDATE, DELETE ON usage_events, audit_log FROM tollgate_app;
```

The application only ever inserts into the ledger and the audit log, so it never
needs `UPDATE` or `DELETE` on them. With both the trigger and the revoked grant
in place, rewriting history requires compromising the owner role, not just the
application. On Cloud SQL, create the app role separately from the migration
role and point `TOLLGATE_DATABASE__URL` at the app role.
