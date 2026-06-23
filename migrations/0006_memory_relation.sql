-- SPDX-License-Identifier: MIT

-- Graph edges between memories beyond the supersession chain (the SDK's `updates`):
-- `extends` (one adds detail to another) and `derives` (one is inferred from another).
-- Detected by a post-commit, best-effort classifier, so this never sits on the write path.
CREATE TABLE mnestic_memory_relation (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id uuid NOT NULL REFERENCES mnestic_tenant(id),
  actor_id text NOT NULL,
  from_id uuid NOT NULL REFERENCES mnestic_memory(id),
  to_id uuid NOT NULL REFERENCES mnestic_memory(id),
  relation text NOT NULL CHECK (relation IN ('extends','derives')),
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, from_id, to_id, relation)
);
CREATE INDEX ON mnestic_memory_relation (tenant_id, from_id);
CREATE INDEX ON mnestic_memory_relation (tenant_id, to_id);
ALTER TABLE mnestic_memory_relation ENABLE ROW LEVEL SECURITY;
ALTER TABLE mnestic_memory_relation FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON mnestic_memory_relation
  USING      (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid)
  WITH CHECK (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid);
