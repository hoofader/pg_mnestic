-- SPDX-License-Identifier: MIT

-- The knowledge-graph layer over memory content. pg_graphwright is required (the deployment
-- runs the docker/pg image that carries it); CREATE EXTENSION needs a superuser migration role.
-- The index marks each memory row on write; graphwright.maintain() (driven by the worker)
-- resolves the entities and edges off the write path. Graph rows carry RLS that delegates to
-- mnestic_memory, so an entity is visible exactly when its source memory is.
CREATE EXTENSION IF NOT EXISTS pg_graphwright;

CREATE INDEX IF NOT EXISTS mnestic_memory_kg ON mnestic_memory USING graphwright (content);
