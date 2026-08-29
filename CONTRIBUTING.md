# Contributing to Tollgate

Thanks for your interest in contributing. This document covers what to expect when sending changes our way.

## Ground rules

- By contributing, you agree your work is licensed under the [MIT licence](./LICENSE) and that you have the right to grant that licence. We do not require a CLA.
- Be kind. The [Code of Conduct](./CODE_OF_CONDUCT.md) applies to every interaction in this repo and its associated channels.
- Tollgate is **single-tenant per deployment by design**. Please do not propose multi-tenant SaaS features (`tenants` table, `tenant_id` columns, admin tenant management). Each customer runs the gateway in their own GCP project; the deployment IS the tenancy boundary.

## How to propose a change

1. **Open an issue first** for anything beyond a typo or trivial fix. Describe the problem, the proposed approach, and any alternatives considered. This avoids wasted work.
2. **Fork and branch** off `main`. Use a descriptive branch name (e.g. `feat/budgets-soft-warn`, `fix/redis-reconnect`).
3. **Keep PRs focused.** One logical change per PR. Refactors that move a lot of code should be separate from behaviour changes.
4. **Follow the existing style.** Run the dev loop in [`DEVELOPMENT.md`](./DEVELOPMENT.md) - `cargo fmt --all`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` - and make sure all three are clean before opening the PR.

## Commit conventions

- One logical change per commit; we squash-merge, but a clean commit history during review still helps.
- Use [Conventional Commits](https://www.conventionalcommits.org/) prefixes (`feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`, `ci:`).
- Reference the issue in the body: `Refs #42` or `Closes #42`.

## Licence headers

Every source file carries an SPDX header. When you add a new file, copy this template and adjust the comment prefix to match the file type:

```
SPDX-License-Identifier: MIT
SPDX-FileCopyrightText: 2026 Andrew Stevens
```

If you contribute substantial work and want your name in headers on files you author, that's welcome - add a second `SPDX-FileCopyrightText:` line. We follow the [REUSE](https://reuse.software/) specification.

## Tests

- New behaviour needs tests. Integration tests against a real Postgres/Redis (via the compose file) are preferred over mocks for anything touching persistence.
- Don't reduce coverage of the budget hot path. Enforcement correctness is the core guarantee of the project.

## What we'll review

PRs are reviewed for: correctness, fit with the locked design, test coverage, performance impact on the gateway hot path, and security posture. We may push back on scope; please don't take it personally.

## Reporting security issues

**Do not open public issues for security problems.** See [`SECURITY.md`](./SECURITY.md) for the disclosure process.
