-- SPDX-License-Identifier: MIT

-- Versioned update (the SDK's PATCH /v4/memories): a new content row supersedes the prior
-- one while history is preserved. version counts the edits along a lineage; root_memory_id
-- points every version back at the first row, so the whole chain is reachable from any link.
-- Both default-friendly (a constant default, a nullable column), so this applies online.
ALTER TABLE mnestic_memory ADD COLUMN version int NOT NULL DEFAULT 1;
ALTER TABLE mnestic_memory ADD COLUMN root_memory_id uuid;
