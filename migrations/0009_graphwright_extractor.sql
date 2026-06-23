-- SPDX-License-Identifier: MIT

-- The GLiNER entity extractor for the knowledge graph. pg_graphwright's extractor seam is a SQL
-- function f(text) -> text[]; this one POSTs the row text to the graphwright-onnx sidecar through
-- pgsql-http and returns the entity surfaces it found.
--
-- Opt-in: this installs the function but does NOT set graphwright.extractor, so the built-in
-- tokenizer stays the default. An operator activates GLiNER by deploying the sidecar and setting
-- `mnestic.gliner_url` and `graphwright.extractor = 'mnestic_gliner_extract'` (see DEPLOYMENT).
-- Until then the graph resolves with the built-in tokenizer and this function is never called.
CREATE EXTENSION IF NOT EXISTS http;

CREATE OR REPLACE FUNCTION mnestic_gliner_extract(doc text) RETURNS text[]
LANGUAGE sql VOLATILE AS $$
  SELECT coalesce(array_agg(s), '{}')
  FROM json_array_elements_text(
    (http_post(
       current_setting('mnestic.gliner_url'),
       json_build_object('text', doc)::text,
       'application/json'
     )).content::json -> 'surfaces'
  ) AS s
$$;
