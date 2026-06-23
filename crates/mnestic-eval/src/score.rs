// SPDX-License-Identifier: AGPL-3.0-only

use std::collections::BTreeMap;

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
    /// The question's category (LongMemEval `question_type`), for the per-type
    /// breakdown. None for datasets that do not carry a type.
    pub category: Option<String>,
    /// Abstention questions form their own breakdown bucket, to match LongMemEval's
    /// separate abstention line rather than pooling them into a content type.
    pub abstention: bool,
    // The qualitative payload for the side-by-side comparison. `MemScore` ignores
    // these; they carry the per-question detail a human reads to see WHY two backends
    // disagreed, not just that they did.
    pub question: String,
    pub gold: String,
    pub predicted: String,
    pub recalled: Vec<String>,
}

/// Accuracy within one question category, for the per-type breakdown LongMemEval
/// reports alongside the overall number.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeScore {
    pub question_type: String,
    pub n: usize,
    pub accuracy: f64,
}

/// memorybench reports quality against cost. The three overall dimensions stay
/// separate rather than folded into one number, and `per_type` mirrors LongMemEval's
/// per-category accuracy so a regression in one question type is visible.
#[derive(Debug, Clone, PartialEq)]
pub struct MemScore {
    pub n: usize,
    pub accuracy: f64,
    pub avg_query_latency_ms: f64,
    pub avg_recalled_context_tokens: f64,
    pub per_type: Vec<TypeScore>,
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
                per_type: Vec::new(),
            };
        }
        let correct = results.iter().filter(|r| r.correct).count();
        let latency: f64 = results.iter().map(|r| r.query_latency_ms).sum();
        let tokens: usize = results.iter().map(|r| r.recalled_context_tokens).sum();

        // Group by category, sorted by name for a stable report.
        let mut groups: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for r in results {
            let key = if r.abstention {
                "abstention".to_string()
            } else {
                r.category.clone().unwrap_or_else(|| "untyped".to_string())
            };
            let entry = groups.entry(key).or_insert((0, 0));
            entry.1 += 1;
            if r.correct {
                entry.0 += 1;
            }
        }
        let per_type = groups
            .into_iter()
            .map(|(question_type, (c, total))| TypeScore {
                question_type,
                n: total,
                accuracy: c as f64 / total as f64,
            })
            .collect();

        MemScore {
            n,
            accuracy: correct as f64 / n as f64,
            avg_query_latency_ms: latency / n as f64,
            avg_recalled_context_tokens: tokens as f64 / n as f64,
            per_type,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(correct: bool, latency: f64, tokens: usize, category: Option<&str>) -> QuestionResult {
        QuestionResult {
            correct,
            query_latency_ms: latency,
            recalled_context_tokens: tokens,
            category: category.map(str::to_string),
            abstention: false,
            question: String::new(),
            gold: String::new(),
            predicted: String::new(),
            recalled: Vec::new(),
        }
    }

    #[test]
    fn aggregates_accuracy_latency_and_tokens() {
        let s = MemScore::from_results(&[
            r(true, 10.0, 100, Some("multi-session")),
            r(false, 20.0, 200, Some("multi-session")),
        ]);
        assert_eq!(s.n, 2);
        assert!((s.accuracy - 0.5).abs() < 1e-9);
        assert!((s.avg_query_latency_ms - 15.0).abs() < 1e-9);
        assert!((s.avg_recalled_context_tokens - 150.0).abs() < 1e-9);
    }

    #[test]
    fn breaks_down_by_type() {
        let s = MemScore::from_results(&[
            r(true, 1.0, 1, Some("temporal-reasoning")),
            r(false, 1.0, 1, Some("temporal-reasoning")),
            r(true, 1.0, 1, Some("knowledge-update")),
        ]);
        // Sorted by type name: knowledge-update (1/1), temporal-reasoning (1/2).
        assert_eq!(s.per_type.len(), 2);
        assert_eq!(s.per_type[0].question_type, "knowledge-update");
        assert!((s.per_type[0].accuracy - 1.0).abs() < 1e-9);
        assert_eq!(s.per_type[1].question_type, "temporal-reasoning");
        assert!((s.per_type[1].accuracy - 0.5).abs() < 1e-9);
    }

    #[test]
    fn abstention_is_its_own_bucket() {
        let mut answerable = r(true, 1.0, 1, Some("single-session-user"));
        let mut absten = r(false, 1.0, 1, Some("single-session-user"));
        answerable.abstention = false;
        absten.abstention = true;
        let s = MemScore::from_results(&[answerable, absten]);
        let names: Vec<&str> = s.per_type.iter().map(|t| t.question_type.as_str()).collect();
        assert!(names.contains(&"abstention"), "abstention should be its own bucket: {names:?}");
        assert!(names.contains(&"single-session-user"));
        let abs = s.per_type.iter().find(|t| t.question_type == "abstention").unwrap();
        assert_eq!(abs.n, 1);
    }

    #[test]
    fn empty_is_zero_not_nan() {
        let s = MemScore::from_results(&[]);
        assert_eq!(s.n, 0);
        assert_eq!(s.accuracy, 0.0);
        assert!(s.per_type.is_empty());
    }
}
