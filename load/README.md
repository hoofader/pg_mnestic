# Load testing

`loadtest.js` is a [k6](https://k6.io) script that drives a mix of saves (`/v4/memories`) and
recalls (`/v4/search`) against a running server. k6 runs the load; the server, database, and
provider keys are yours.

## 1. Run a server

Point it at a database and the providers (see DEPLOYMENT.md). For a local run:

```bash
export DATABASE_URL=postgres://postgres:postgres@localhost:5433/postgres
export OPENAI_API_KEY=... ANTHROPIC_API_KEY=...
cargo run -p mnestic-server --features serve --bin serve
```

For throughput numbers that aren't dominated by model latency, use dynamic saves (below) and
raise the limiter (`MNESTIC_RATE_LIMIT_PER_MIN=100000`) or disable it (`=0`).

## 2. Mint a key

```bash
cargo run -p mnestic-server --features cli --bin issue-key -- loadtest
# prints: token (shown once, store it now): sm_...
export MNESTIC_TOKEN=sm_...
```

## 3. Run the load

```bash
brew install k6   # or see k6.io/docs/get-started/installation
k6 run -e BASE_URL=http://127.0.0.1:8080 load/loadtest.js
```

With the defaults this spends real tokens: ~30% of iterations are instant saves that call
extraction and embedding. Read the caveats below before a long run.

Tunables (all `-e NAME=value`): `VUS` (default 10), `DURATION` (30s), `SAVE_RATIO` (0.3),
`CONTAINER_TAG` (user:load), `SAVE_DREAMING` (instant | dynamic).

## What to measure, and the caveats

- **Saves run the models.** With `SAVE_DREAMING=instant`, each save calls extraction and
  embedding in-request, so its latency is provider latency (seconds) and it spends tokens.
  That measures the model path, not the server. To load the server and database instead, set
  `SAVE_DREAMING=dynamic` (saves enqueue and return fast; run the `worker`, with the same
  `DATABASE_URL` and provider keys as the server, to drain them) or drop `SAVE_RATIO` and run
  search-heavy.
- **Searches need data.** On an empty database `/v4/search` returns nothing but still measures
  the recall query cost. Run a save phase (or a prior dynamic+worker pass) first so searches
  hit real rows; recall cost grows with the corpus, so test at a realistic size.
- **Rate limiting.** A key over `MNESTIC_RATE_LIMIT_PER_MIN` gets 429s; the script counts them
  in the `rate_limited` metric and does not treat them as failures. The bucket bursts to the
  full per-minute capacity before throttling, so a short, low-VU run may not trip the default
  600 at all (`rate_limited` stays 0). If you see many, raise or disable the limit for the
  test, or add virtual users with their own keys.
- **Watch the server side too.** The `mnestic::tokens` log events show provider spend during
  the run, and the per-request spans show server-side latency. Compare those to k6's
  client-side `http_req_duration` to separate model time from server time.

## Reading k6 output

- `http_req_duration` p95/p99: end-to-end latency. The script's threshold (`p(95)<2000`) is a
  placeholder; set it to your target.
- `http_reqs` rate: throughput (requests/sec).
- `http_req_failed`: genuine error rate (5xx, connection). 429 is excluded by design.
- `rate_limited`: how many requests were throttled.
- `checks`: the share of responses that were 200 or 429.
