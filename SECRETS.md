# Secrets

Two kinds of secret matter here, and they are handled differently.

## Process secrets (provider keys, database URL)

`mnestic-server` reads these from the environment:

- `DATABASE_URL` (Postgres DSN; carries the database password)
- `OPENAI_API_KEY` (embeddings)
- `ANTHROPIC_API_KEY` (extraction)

In production these come from a secrets manager (AWS Secrets Manager, GCP Secret Manager,
Vault, Kubernetes Secrets), injected into the process environment at start. Do not commit
them and do not bake them into an image.

The repo-root `.env` is for local development only. It is gitignored. The keys shared in chat
during early development are in it and **must be rotated** before any real deployment, since
they have been exposed in plaintext.

Operational rules:

- Give the database role only the privileges it needs (DML on the `mnestic_*` tables plus the
  ability to run migrations). It does not need superuser.
- Provider keys are sent only in upstream request headers, never logged. The generic 500 path
  logs an error detail to stderr but not request bodies or credentials; keep it that way.
- Rotate provider keys on a schedule and on any suspected exposure. Rotation is a config push
  (new value in the secrets manager, restart), no code change.

## Tenant API keys (the `sm_` bearers)

One key maps to one tenant, the RLS boundary (doc 04 §2). Only the SHA-256 digest is stored;
the cleartext is shown once at issuance and is unrecoverable. Manage them with the CLI
(built with `--features cli`):

```bash
# issue a key for a tenant (optionally labelled), prints the token once
cargo run -p mnestic-server --features cli --bin issue-key -- acme "ci pipeline"

# list a tenant's keys: <digest-hex>  <created_at>  <status>  <label>
cargo run -p mnestic-server --features cli --bin list-keys -- acme

# revoke a key by its digest (from list-keys) or by the cleartext token
cargo run -p mnestic-server --features cli --bin revoke-key -- <digest-hex>
```

To rotate a tenant's key with no downtime: issue a new key, switch the client to it, then
revoke the old one. Both keys authenticate until the old one is revoked, so there is no gap.
Revocation takes effect on the next request (`authenticate` filters `revoked_at IS NULL`).
