-- SPDX-License-Identifier: Apache-2.0

-- API-key lifecycle. `label` lets an operator tell keys apart in a listing; `revoked_at`
-- cuts off a leaked or rotated key without deleting the row, so the record that the key
-- existed and when it was revoked survives. auth::authenticate now requires revoked_at IS
-- NULL, so revocation takes effect on the next request. Both columns are nullable, so this
-- applies online with no backfill.
ALTER TABLE mnestic_api_key
  ADD COLUMN label      text,
  ADD COLUMN revoked_at timestamptz;
