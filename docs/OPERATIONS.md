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

Because the ledger is authoritative, the cache is disposable, and it is rebuilt
from the ledger in two places.

At startup the gateway runs `reconcile_counters`, which sums the ledger over each
budget's current period window and raises every counter to that figure. It only
ever raises: a ledger write can be lost after its counter was already
incremented, and setting the counter back down to the ledger sum would permit
overspend for the rest of the period.

At request time the reserve script refuses to act on a counter that is not there,
and the gateway rebuilds it from the ledger before retrying. An absent counter is
not a counter at zero, it is a counter whose history Valkey has lost, and an
`INCRBY` cannot tell those apart. Without this a flush, a failover to a cold
replica, or an eviction would silently restart every budget's period. If the
ledger cannot be reached to rebuild it, the request is refused with a `503`
rather than admitted unenforced.

Three consequences worth planning for. The first request of a new budget period
costs one ledger query per applicable budget, because that period's counter does
not exist yet. A Postgres outage during a period rollover refuses traffic rather
than admitting it, which is the correct direction but is a hard dependency.

And every request that was in flight at the moment of the loss settles against a
counter that has since been rebuilt, so its settlement is declined. The cost
still reaches the ledger, but the counter does not pick it up until the next
startup reconcile raises it. For that period the counter under-states spend by
the actual cost of whatever was in flight. That is bounded and it is the safe
direction relative to the alternative, which was losing the entire period, but it
is a reason to restart the gateway after a cache incident rather than leaving it.

**Run Valkey as a single shard.** The budget counter keys carry no hash tag, so
a request that touches several budgets at once, which is the normal case (a key
budget, a provider budget, a model budget, a global one), hands several keys to
one Lua script. In a sharded Valkey those keys hash to different slots and the
script is refused with `CROSSSLOT`, which surfaces as a `503` on every request
that spans more than one budget. The Terraform module defaults to
`valkey_shard_count = 1` for this reason. Raising it will not scale the gateway,
it will break enforcement, until the keys are given a common hash tag so they
land in one slot. That change has to be made deliberately, because it also moves
every existing counter to a new key name.

Run Valkey with `maxmemory-policy noeviction` and do not share the instance with
other workloads. Counters carry a 40-day expiry, so under any `volatile-*` policy
a live budget counter is an eviction candidate. Eviction is now repaired rather
than silently ignored, but repairing it costs a ledger query per affected budget,
and under memory pressure that can be every budget on every request.

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

- creates the current month's partition and the two after it, so inserts
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

- Cloud Run reaches Cloud SQL and Valkey over internal ranges through the
  Serverless VPC Access connector (`egress = PRIVATE_RANGES_ONLY`). Provider
  traffic still leaves over Google's default public path; the application
  restricts destinations, the network does not.
- The provider adapters only ever connect to fixed provider hostnames. A caller
  supplies a provider name and a path suffix, both allowlisted; they never supply
  a destination host. There is no user-controlled URL in the forward path.
- The outbound HTTP client refuses redirects (`redirect::Policy::none()`), so a
  `3xx` from an upstream cannot bounce the request, and its credentials, to a
  different host.
- Outbound TLS is validated against the CA store in the running image, not
  against a bundle compiled into the binary. Trust rotates when you rebase, and
  a private CA added to the image is honoured, which is what makes the Secure
  Web Proxy architecture below workable. The flip side is that an image without
  CA certificates cannot reach any provider; the distroless runtime image ships
  them, so treat a base-image change as a change to trust.
- Secrets (database URL, API-key pepper) are read from Secret Manager by
  reference and scoped to the runtime service account per-secret, not granted
  project-wide.

One gap in that posture, stated plainly because the module ships it this way:
the Valkey connection has neither AUTH nor in-transit encryption. Budget counters
travel unauthenticated and in cleartext, and the only thing keeping the instance
off other networks is Private Service Connect. Anyone who reaches that address
can read and write the counters that decide whether spending continues. Enable Memorystore AUTH and carry the credential in `TOLLGATE_REDIS__URL` if an
attacker inside the VPC is in your threat model. In-transit encryption is not
available in this build: the Valkey client is compiled without a TLS feature, so
a `rediss://` URL is refused at startup rather than silently downgraded. Adding
it is open work. The ledger
bounds the consequence: counters are rebuilt from Postgres at startup and
whenever one goes missing, so tampering suppresses enforcement until the next
rebuild rather than corrupting the record of what was spent.

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
   lowest-priority rule that **denies** everything else. If you have configured
   an OpenAI-compatible upstream with
   `TOLLGATE_PROVIDERS__OPENAI__UPSTREAM=custom`, add that host too, or every
   request through it fails at the proxy rather than at the gateway.
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
enforced inside the database by triggers that reject `UPDATE` and `DELETE`.

The trigger stops accidental and application-level mutation. It is not proof
against the table owner, who can `ALTER TABLE ... DISABLE TRIGGER`, drop the
trigger, or drop a whole partition in one statement. Retention relies on exactly
that last ability. So the real control is the role split below, plus database
audit logging; the trigger is what makes tampering deliberate rather than
possible by accident.

As a second layer, run the application against a non-owner Postgres role rather
than the role that owns the schema, and revoke the mutating grants it does not
need:

```sql
-- Run migrations as the owner, then create a least-privilege app role.
CREATE ROLE tollgate_app LOGIN PASSWORD '...';
GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA public TO tollgate_app;
-- No UPDATE on api_keys: the gateway only reads them, and revocation is a CLI
-- operation run as the owner. Granting it here would hand the application the
-- one table an attacker with this role would most want to write.
REVOKE UPDATE, DELETE ON usage_events, audit_log FROM tollgate_app;

-- REQUIRED, and easy to miss. Partition maintenance runs on the gateway's
-- schedule but must execute with the OWNER's rights, because creating and
-- dropping partitions of usage_events requires owning the parent and the
-- partitions. Marking the function SECURITY DEFINER does that without handing
-- the application role any ownership of its own.
ALTER FUNCTION tollgate_usage_maintain(interval) SECURITY DEFINER;
ALTER FUNCTION tollgate_usage_maintain(interval) SET search_path = public;
REVOKE ALL ON FUNCTION tollgate_usage_maintain(interval) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION tollgate_usage_maintain(interval) TO tollgate_app;
```

The application only ever inserts into the ledger, so it never needs `UPDATE` or
`DELETE` on it. With the trigger, the revoked grant, and no ownership, rewriting
history requires compromising the owner role, not just the application. On Cloud
SQL, create the app role separately from the migration role and point
`TOLLGATE_DATABASE__URL` at the app role.

**Do not skip the `ALTER FUNCTION` lines, and do not reach for
`ALTER TABLE ... OWNER` instead.** `tollgate_usage_maintain` ships as a plain
`plpgsql` function and runs as whoever calls it. Transferring the table to the
app role looks like the fix and is not: on Postgres 15 and later, which includes
the Postgres 16 this project ships, an ordinary role no longer holds `CREATE` on
the `public` schema, so partition creation still fails; `ALTER TABLE ... OWNER`
does not recurse to existing partitions, so retention's `DROP TABLE` fails on
every partition the migration made; and it hands the application the ownership
that lets it disable the append-only trigger, which is the thing this section
exists to prevent.

Get it wrong and the failure is quiet. Every maintenance sweep logs a `WARN`
while the gateway serves normally. Migration 0006 creates the current month's
partition and the two after it, so nothing breaks for roughly three months. After
that every ledger insert fails with "no partition found", and ledger writes are
best effort: the failure is logged and the request still succeeds. Budgets go on
binding from Valkey while the billing record silently stops, the console goes
stale, and retention never runs. Worse, a later cache loss rebuilds counters from
a ledger missing everything since the sweeps stopped.

Alert on two log messages. `usage ledger maintenance failed` is the early
warning, weeks before anything visibly breaks. `failed to write usage event` is
the ledger already losing rows. Neither affects the response a client gets, which
is exactly why they need an alert rather than a dashboard.
