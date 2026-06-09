// SPDX-License-Identifier: Apache-2.0

/// Outcome of answering one benchmark question.
#[derive(Debug, Clone)]
pub struct QuestionResult {
    pub correct: bool,
    /// End-to-end query latency: the embedding call plus DB retrieval, in ms. Not
    /// DB-only, because recall embeds the query first.
    pub query_latency_ms: f64,
    /// Estimated tokens of the recalled context only (~4 chars/token). Excludes the
    /// system prompt and question scaffolding sent to the answerer, so it is a
    /// relative cost proxy across runs, not the model's true input size.
    pub recalled_context_tokens: usize,
}

/// memorybench reports quality against cost. The three dimensions stay separate
/// rather than folded into one number, so a regression in any one is visible.
#[derive(Debug, Clone, PartialEq)]
pub struct MemScore {
    pub n: usize,
    pub accuracy: f64,
    pub avg_query_latency_ms: f64,
    pub avg_recalled_context_tokens: f64,
}

impl MemScore {
    pub fn from_results(results: &[QuestionResult]) -> Self {
        let n = results.len();
        if n == 0 {
            return MemScore {
                n: 0,
                accuracy: 0.0,
                avg_query_latency_ms: 0.0,
                avg_recalled_context_tokens: 0.0,
            };
        }
        let correct = results.iter().filter(|r| r.correct).count();
        let latency: f64 = results.iter().map(|r| r.query_latency_ms).sum();
        let tokens: usize = results.iter().map(|r| r.recalled_context_tokens).sum();
        MemScore {
            n,
            accuracy: correct as f64 / n as f64,
            avg_query_latency_ms: latency / n as f64,
            avg_recalled_context_tokens: tokens as f64 / n as f64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(correct: bool, latency: f64, tokens: usize) -> QuestionResult {
        QuestionResult {
            correct,
            query_latency_ms: latency,
            recalled_context_tokens: tokens,
        }
    }

    #[test]
    fn aggregates_accuracy_latency_and_tokens() {
        let s = MemScore::from_results(&[r(true, 10.0, 100), r(false, 20.0, 200)]);
        assert_eq!(s.n, 2);
        assert!((s.accuracy - 0.5).abs() < 1e-9);
        assert!((s.avg_query_latency_ms - 15.0).abs() < 1e-9);
        assert!((s.avg_recalled_context_tokens - 150.0).abs() < 1e-9);
    }

    #[test]
    fn empty_is_zero_not_nan() {
        let s = MemScore::from_results(&[]);
        assert_eq!(s.n, 0);
        assert_eq!(s.accuracy, 0.0);
        assert_eq!(s.avg_query_latency_ms, 0.0);
    }
}
