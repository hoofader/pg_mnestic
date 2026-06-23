// SPDX-License-Identifier: MIT

// Drives the official supermemory sdk-ts (the npm `supermemory` package) against a live
// pg_mnestic, to prove the SDK is a drop-in client. It exercises the memory lifecycle the
// engine implements: add -> search.memories -> profile -> search.documents -> updateMemory ->
// forget. Point it with SUPERMEMORY_BASE_URL (the pg_mnestic server) and SUPERMEMORY_API_KEY
// (a tenant key from `issue-key`). Exits non-zero on the first failed check.

import assert from 'node:assert/strict';
import Supermemory from 'supermemory';

const baseURL = process.env.SUPERMEMORY_BASE_URL;
const apiKey = process.env.SUPERMEMORY_API_KEY;
if (!baseURL || !apiKey) {
  console.error('set SUPERMEMORY_BASE_URL (pg_mnestic) and SUPERMEMORY_API_KEY (a tenant key)');
  process.exit(2);
}

const client = new Supermemory({ baseURL, apiKey });
// A fresh container tag per run keeps repeats independent.
const tag = `sm-conf-${Date.now()}`;
const FACT = 'Ada Lovelace wrote the first algorithm in 1843.';

let passed = 0;
async function step(name, fn) {
  await fn();
  passed += 1;
  console.log(`ok  ${name}`);
}

try {
  await step('client.add stores a memory', async () => {
    const r = await client.add({ content: FACT, containerTag: tag });
    assert.ok(r.id, 'add returns a document id');
  });

  let memId;
  await step('client.search.memories finds it', async () => {
    const r = await client.search.memories({ q: 'algorithm', containerTag: tag });
    assert.ok(Array.isArray(r.results), 'results is an array');
    const hit = r.results.find((x) => (x.memory ?? '').includes('Ada Lovelace'));
    assert.ok(hit, 'the added memory is returned');
    assert.equal(typeof hit.similarity, 'number', 'a similarity score is present');
    memId = hit.id;
  });

  await step('client.profile carries the memory', async () => {
    const r = await client.profile({ containerTag: tag });
    const all = [...(r.profile?.static ?? []), ...(r.profile?.dynamic ?? [])].join(' | ');
    assert.ok(all.includes('Ada Lovelace'), 'profile includes the memory');
  });

  await step('client.search.documents responds', async () => {
    // A memory-task add has no document chunks, so this asserts the method works and returns
    // the expected shape rather than a specific hit.
    const r = await client.search.documents({ q: 'algorithm', containerTag: tag });
    assert.ok(Array.isArray(r.results), 'documents search returns a results array');
  });

  let v2Id;
  await step('client.memories.updateMemory versions it', async () => {
    const r = await client.memories.updateMemory({
      id: memId,
      newContent: 'Ada Lovelace, revised: she wrote the first algorithm.',
      containerTag: tag,
    });
    assert.equal(r.version, 2, 'update creates version 2');
    assert.equal(r.parentMemoryId, memId, 'the new version points at the prior one');
    v2Id = r.id;
  });

  await step('the update is searchable', async () => {
    const r = await client.search.memories({ q: 'revised', containerTag: tag });
    assert.ok(
      r.results.some((x) => (x.memory ?? '').includes('revised')),
      'the revised content is searchable',
    );
  });

  await step('client.memories.forget removes it', async () => {
    const r = await client.memories.forget({ id: v2Id, containerTag: tag });
    assert.equal(r.forgotten, true, 'forget reports the memory forgotten');
  });

  console.log(`\n${passed} checks passed against ${baseURL}`);
  process.exit(0);
} catch (e) {
  console.error(`\nFAILED after ${passed} checks: ${e?.message ?? e}`);
  process.exit(1);
}
