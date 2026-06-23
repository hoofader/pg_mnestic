// SPDX-License-Identifier: MIT
//
// k6 load test for the mnestic server. Drives a mix of saves (/v4/memories) and recalls
// (/v4/search) against one tenant, under bearer auth. See load/README.md to run it.
//
// Env:
//   BASE_URL      server base url (default http://127.0.0.1:8080)
//   MNESTIC_TOKEN the sm_ API key (required; mint one with the issue-key binary)
//   CONTAINER_TAG scope for saves/searches (default user:load)
//   VUS           concurrent virtual users (default 10)
//   DURATION      test length (default 30s)
//   SAVE_RATIO    fraction of requests that are saves vs searches (default 0.3)
//   SAVE_DREAMING "instant" (sync, runs the model in-request, slow + costs tokens) or
//                 "dynamic" (enqueue, returns fast; a worker extracts later). Default instant.

import http from 'k6/http';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const BASE = __ENV.BASE_URL || 'http://127.0.0.1:8080';
const TOKEN = __ENV.MNESTIC_TOKEN;
const TAG = __ENV.CONTAINER_TAG || 'user:load';
const SAVE_RATIO = parseFloat(__ENV.SAVE_RATIO || '0.3');
const DREAMING = __ENV.SAVE_DREAMING || 'instant';

// 429 is an expected outcome under the per-key limit, not a failure; count it separately so
// http_req_failed reflects only genuine errors (5xx, connection).
http.setResponseCallback(http.expectedStatuses(200, 429));
const rateLimited = new Counter('rate_limited');

export const options = {
  vus: parseInt(__ENV.VUS || '10', 10),
  duration: __ENV.DURATION || '30s',
  thresholds: {
    http_req_failed: ['rate<0.01'], // under 1% genuine errors
    http_req_duration: ['p(95)<2000'], // p95 under 2s; tune to your target and hardware
  },
};

const QUERIES = [
  'where does the user live',
  'what does the user do for work',
  'what are the user preferences',
  'recent activity',
  'pets',
];
const FACTS = [
  'the user enjoys climbing on weekends',
  'the user lives in Lisbon',
  'the user works at Globex as a staff engineer',
  'the user prefers tea over coffee',
  'the user adopted a cat named Pixel',
];

function pick(a) {
  return a[Math.floor(Math.random() * a.length)];
}

export default function () {
  if (!TOKEN) {
    throw new Error('set MNESTIC_TOKEN to an sm_ key (see load/README.md)');
  }
  const headers = { 'Content-Type': 'application/json', Authorization: `Bearer ${TOKEN}` };

  let res;
  if (Math.random() < SAVE_RATIO) {
    const body = {
      content: `${pick(FACTS)} (${Date.now()})`,
      containerTag: TAG,
    };
    if (DREAMING === 'dynamic') {
      body.dreaming = 'dynamic';
    }
    res = http.post(`${BASE}/v4/memories`, JSON.stringify(body), { headers });
  } else {
    const body = { q: pick(QUERIES), containerTag: TAG, limit: 10 };
    res = http.post(`${BASE}/v4/search`, JSON.stringify(body), { headers });
  }

  if (res.status === 429) {
    rateLimited.add(1);
  }
  check(res, { 'ok or rate-limited': (r) => r.status === 200 || r.status === 429 });
}
