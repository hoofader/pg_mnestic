# Head-to-head comparison

> Status: the harness is built and tested, but it has not been run for published numbers yet. No
> recall-quality parity is claimed until it is run and the results are posted here.

The `compare` harness runs the SAME benchmark through two memory engines and reports
how they differ. Because both engines speak the supermemory wire API, one HTTP client
pointed at two base URLs drives Mnestic's server and `api.supermemory.ai`
identically: same cases, same answerer, same judge. The only variable is the engine
behind the URL, so the comparison is apples-to-apples.

## What it measures

Quantitative, per backend (a `MemScore`):

- judge accuracy (overall and per question type),
- recall latency (ms per question),
- recalled context tokens (a ~4-chars/token proxy for retrieval cost).

Qualitative, per question:

- the recalled context, the model's answer, and the judge verdict for each backend,
- a Disagreements section: the questions where the backends split on correctness, which
  is the first thing to read when one engine wins and the other does not.

## Running it

```bash
export MNESTIC_COMPARE_A_NAME=pg_mnestic
export MNESTIC_COMPARE_A_URL=http://localhost:8080        # a pg_mnestic server
export MNESTIC_COMPARE_A_KEY=<tenant-key>
export MNESTIC_COMPARE_B_NAME=supermemory
export MNESTIC_COMPARE_B_URL=https://api.supermemory.ai
export MNESTIC_COMPARE_B_KEY=<supermemory-key>
export ANTHROPIC_API_KEY=<key>                            # answer + judge
# export MNESTIC_EVAL_RECALL_LIMIT=10                      # optional, default 10

cargo run -p mnestic-eval --features real --bin compare -- \
  crates/mnestic-eval/fixtures/scenarios.json
```

A backend is included only if its `_URL` is set. The markdown report prints to stdout;
the error list and a zero-score warning go to stderr. The run exits non-zero if either
backend scored zero questions (a bad URL, auth, or every case failing to ingest), so a
broken setup is not mistaken for a real result.

The dataset is either the hand-authored `fixtures/scenarios.json` (a quick qualitative
read: a superseded preference, a cross-session contradiction, a temporal question, a
multi-session fact, and an abstention question) or a normalized LongMemEval json from
the `lme-convert` binary for the rigorous run.

## Caveats

- A fair run needs pg_mnestic on real providers (real embeddings and extraction). Mock
  mode does not extract, so its recall is not representative.
- The supermemory wire `add` has no event-time field, so per-session dates are NOT sent
  over HTTP. Temporal fidelity is reduced, but symmetrically: both HTTP backends are
  treated identically, so the comparison stays fair even if absolute temporal scores are
  lower than the in-process engine path would produce.
- The judge is Claude, not the gpt-4o used upstream by LongMemEval. Treat the accuracy
  as close-but-not-official.
