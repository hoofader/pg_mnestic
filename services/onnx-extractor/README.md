# onnx-extractor

The GLiNER entity-extraction sidecar for the knowledge graph. A small Node HTTP service that
wraps [`graphwright-onnx`](https://github.com/hoofader/graphwright-onnx) (zero-shot NER through
ONNX), so `pg_graphwright`'s extractor seam can reach real named-entity extraction instead of the
built-in tokenizer. Memory text stays on your own infrastructure (no third-party call).

```bash
docker build -t mnestic-onnx .
docker run -d --name onnx -p 8081:8081 mnestic-onnx
```

The GLiNER model (`onnx-community/gliner_small-v2.1` by default) is fetched on first start, not
bundled, so the first boot is slow; mount a volume at `/root/.cache/huggingface` to cache it.

- `GET /health` -> `200 ok` once the model is loaded, `503 loading` before.
- `POST /extract {"text": "..."}` -> `{"surfaces": ["...", ...]}` (the entity surfaces).

Env: `PORT` (default 8081), `GLINER_MODEL` (default `onnx-community/gliner_small-v2.1`),
`GLINER_THRESHOLD` (default 0.5).

To wire it into the database, see the "Knowledge graph extractor" section in `DEPLOYMENT.md`
(set `mnestic.gliner_url` to this service's `/extract` and `graphwright.extractor` to
`mnestic_gliner_extract`). It is opt-in; without it the graph uses the built-in tokenizer.
