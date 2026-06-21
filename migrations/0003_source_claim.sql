-- SPDX-License-Identifier: Apache-2.0

-- Async-ingestion lease. A worker claims a pending source by stamping claimed_at, runs the
-- slow extraction without holding a row lock, then clears needs_extraction. If the worker
-- dies mid-job, the claim goes stale once claimed_at ages past the lease and another worker
-- reclaims it, so no enqueued source is stranded. Nullable, so this applies online.
ALTER TABLE mnestic_source ADD COLUMN claimed_at timestamptz;
