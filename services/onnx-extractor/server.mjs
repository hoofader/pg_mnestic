// SPDX-License-Identifier: AGPL-3.0-only

// HTTP wrapper around graphwright-onnx's GLiNER extractor. pg_graphwright's extractor seam is a
// SQL function f(text) -> text[]; the in-database side (the `http` extension) POSTs here and this
// returns the entity surfaces. Kept dependency-light: Node's built-in http server, the model
// loaded once at startup. A slow load never blocks a memory write, since extraction runs in the
// graph maintenance pass, not on the write path.

import { createServer } from 'node:http';
import { GlinerExtractor } from 'graphwright-onnx';

const MODEL = process.env.GLINER_MODEL || 'onnx-community/gliner_small-v2.1';
const THRESHOLD = Number(process.env.GLINER_THRESHOLD || '0.5');
const PORT = Number(process.env.PORT || '8081');

const extractor = new GlinerExtractor({ modelId: MODEL, threshold: THRESHOLD });
let ready = false;
extractor
  .initialize()
  .then(() => {
    ready = true;
    console.log(`gliner ready (${MODEL})`);
  })
  .catch((e) => {
    console.error('gliner init failed', e);
    process.exit(1);
  });

// graphwright's ExtractedEntities splits mentions into people/places/concepts; the seam wants a
// flat list of surfaces, so fold the three kinds into one array.
function surfaces(extracted) {
  const out = [];
  for (const kind of ['people', 'places', 'concepts']) {
    for (const e of extracted?.[kind] ?? []) {
      if (e?.surface_form) out.push(e.surface_form);
    }
  }
  return out;
}

const server = createServer((req, res) => {
  if (req.method === 'GET' && req.url === '/health') {
    res.writeHead(ready ? 200 : 503, { 'content-type': 'text/plain' }).end(ready ? 'ok' : 'loading');
    return;
  }
  if (req.method === 'POST' && req.url === '/extract') {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', async () => {
      try {
        const { text } = JSON.parse(body || '{}');
        const extracted = await extractor.extract(String(text ?? ''));
        res
          .writeHead(200, { 'content-type': 'application/json' })
          .end(JSON.stringify({ surfaces: surfaces(extracted) }));
      } catch (e) {
        res
          .writeHead(500, { 'content-type': 'application/json' })
          .end(JSON.stringify({ error: String(e) }));
      }
    });
    return;
  }
  res.writeHead(404).end();
});
server.listen(PORT, () => console.log(`onnx-extractor listening on :${PORT}`));
