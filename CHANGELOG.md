<!--
SPDX-License-Identifier: MIT
SPDX-FileCopyrightText: 2026 Andrew Stevens
-->

# Changelog

Notable changes per release. Dates are the tag date.

## v0.2.2

Long-context pricing, and a budget counter that survives losing its cache.

There is no v0.2.1. The work was cut as one release rather than shipped in
pieces.

Upgrading from v0.2.0 requires migration 0010.

### Added

- **Long-context tiering, per model.** Several current models bill the whole
  request at a higher rate once the prompt crosses a size threshold, commonly 2x
  on input and 1.5x on output above 200k tokens. A price is one rate per class
  and cannot express that, so before this release every long request on such a
  model was under-charged by the tier multiple. Migration 0010 puts the threshold
  and separate input and output multiples on the price row, resolved field by
  field against a deployment default. Set them with `admin price set
  --long-context-threshold --long-context-input-permille
  --long-context-output-permille`. Off until set, because a model may not tier at
  all and defaulting to an uplift would invent a charge.
- `admin price set` prints the tier the model will actually run with, marking
  fields inherited from the deployment default.
- A soft budget now logs a WARN when it is exceeded, so `--hard-stop false`
  produces a cap that reports rather than one that is silently inert.

### Fixed

- **A lost Valkey counter no longer resets a budget.** The reserve script used
  `INCRBY`, which recreates a missing key at the reserve amount, so a flush, a
  failover to a cold replica, or an eviction silently restarted every budget's
  period until someone restarted the gateway. Reserve now refuses on an absent
  counter, rebuilds it from the ledger, and retries once; if the ledger cannot be
  reached the request is refused rather than admitted unenforced. Settle skips a
  counter that vanished instead of resurrecting it holding one request's delta.
- **`--hard-stop` could not be set to false.** A bare `bool` in the CLI derive is
  a presence flag, so `--hard-stop false` was an error and absence gave true.
  Soft budgets existed in the schema and in the enforcement path and could not be
  created.
- A reservation on an upstream whose cache convention is unverified now sizes the
  prompt at double, matching what the billing already assumes there. Without it,
  such a request could settle at the long-context multiple against a reservation
  taken at 1x.
- A deployment-wide long-context multiple above 1.0x with a threshold of 0 is
  refused at boot. It re-rated nothing and suppressed the under-charge warning
  too, so it looked configured and did neither.
- `admin price set --long-context-threshold 0`, the documented per-model disable,
  was refused on any model that had ever been given a multiple.

### Changed

- `--hard-stop` now takes a value. A script passing it as a bare flag will break;
  pass `--hard-stop true` or drop it, since budgets are hard by default.
- `x-tollgate-reason` now uses one vocabulary on every route. The native
  Anthropic path used to emit the SDK's error kind, so a budget refusal on
  `/v1/messages` reported `permission_error` while the same refusal on
  `/v1/chat/completions` reported `budget_exceeded`, and an alert keyed on the
  latter missed every Claude refusal. The response body is unchanged: it still
  carries Anthropic's vocabulary so SDK exception classes resolve.
- A price book loads whole or not at all. One model resolving to an invalid tier
  is fatal at boot, and on a reload keeps the previous book rather than swapping
  in a partial one.
- The Cloud Run request timeout in the Terraform module goes from 60s to 960s. It
  predated streaming, so every stream was being killed by the platform at one
  minute and charged its full reservation. The Valkey instance now sets
  `maxmemory-policy noeviction`.
- Every path that creates a budget counter now goes through one script, which
  refuses to overwrite a counter live traffic just created and stamps the
  creation with a random generation. A reservation records the generation it saw
  and a settlement declines if it has moved, so a settlement in flight across a
  cache loss cannot subtract a reservation the rebuilt counter never held.
- A prompt above the long-context threshold on a model with no tier configured
  logs a possible under-charge on the streaming path as well as the buffered one.
- A price row whose own multiples are stranded against a zero threshold is warned
  about on every config reload.

### Upgrading

Complete the rollout before relying on the counter repair. A v0.2.0 instance
still recreates a missing counter at the reserve amount and settles without
checking a generation, so during a mixed rollout the old behaviour is live on the
old instances. Restart once the rollout is finished.

### Documentation

A full pass for accuracy. The README no longer claims `exact` admission is a
strict cap without qualification, the routes and their accepted auth headers are
documented, the price example sets cache rates rather than silently falling back
to a 10x over-charge, and several statements that were never true have been
corrected or removed. `docs/OPERATIONS.md` gains the table-ownership step its
least-privilege recipe was missing, without which partition maintenance fails
silently and the ledger stops recording weeks later. `SECURITY.md` gains a threat
model.

## v0.2.0

Streaming, an OpenAI-compatible endpoint, a native Anthropic route, and
prompt-cache pricing. See the release notes for detail.

## v0.1.2

Metering corrections. Vertex thinking and tool-use tokens were dropped from the
output leg, and Anthropic cached requests were under-counted by roughly 99%. Both
were live under-charges.

## v0.1.1 and earlier

The budget-enforcement core, provider adapters, console, config hot-reload and
ledger retention. See the Git history.
