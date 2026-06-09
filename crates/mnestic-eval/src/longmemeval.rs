// SPDX-License-Identifier: Apache-2.0

//! Convert LongMemEval (`longmemeval_s.json` / `_m` / `_oracle`) into Mnestic's
//! normalized dataset. The file is a JSON array of instances; each carries a
//! question plus a "haystack" of dated sessions. `haystack_dates` is parallel to
//! `haystack_sessions`. Each question carries its `question_type` so the judge can
//! apply LongMemEval's per-type prompt, and abstention instances (question_id ending
//! `_abs`) are included and flagged so the judge applies the unanswerable-question
//! prompt instead of matching gold.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;

use crate::dataset::{Case, Qa, Session, Turn};

/// LongMemEval date strings look like "2023/05/30 (Tue) 23:40". The "(Ddd)" weekday
/// token is stripped before parsing (see `parse_date`), so the format has no `%a`.
const DATE_FMT: &str = "%Y/%m/%d %H:%M";

#[derive(Deserialize)]
struct LmeTurn {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct LmeInstance {
    question_id: String,
    question_type: String,
    question: String,
    answer: String,
    #[serde(default)]
    haystack_dates: Vec<String>,
    #[serde(default)]
    haystack_sessions: Vec<Vec<LmeTurn>>,
}

fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    // Drop the "(Ddd)" weekday token. chrono's `%a` rejects a weekday that disagrees
    // with the date, so a single inconsistent source row would otherwise abort the
    // whole conversion. The weekday is redundant with the date and unused here.
    let cleaned = s
        .split_whitespace()
        .filter(|tok| !tok.starts_with('('))
        .collect::<Vec<_>>()
        .join(" ");
    let naive = NaiveDateTime::parse_from_str(&cleaned, DATE_FMT)
        .with_context(|| format!("parsing date {s:?}"))?;
    Ok(naive.and_utc())
}

/// Convert LongMemEval JSON into normalized cases. Abstention questions are included
/// and flagged (`Qa::abstention`); each question carries its `question_type`.
pub fn convert(raw: &str) -> Result<Vec<Case>> {
    let instances: Vec<LmeInstance> =
        serde_json::from_str(raw).context("parsing LongMemEval JSON")?;

    let mut cases = Vec::with_capacity(instances.len());
    for inst in instances {
        let abstention = inst.question_id.ends_with("_abs");
        if inst.haystack_dates.len() != inst.haystack_sessions.len() {
            return Err(anyhow!(
                "instance {}: haystack_dates ({}) and haystack_sessions ({}) lengths differ",
                inst.question_id,
                inst.haystack_dates.len(),
                inst.haystack_sessions.len()
            ));
        }

        let sessions = inst
            .haystack_sessions
            .into_iter()
            .zip(inst.haystack_dates.iter())
            .map(|(turns, date)| {
                Ok(Session {
                    date: Some(parse_date(date)?),
                    turns: turns
                        .into_iter()
                        .map(|t| Turn {
                            role: t.role,
                            content: t.content,
                        })
                        .collect(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        cases.push(Case {
            id: inst.question_id,
            sessions,
            questions: vec![Qa {
                question: inst.question,
                answer: inst.answer,
                question_type: Some(inst.question_type),
                abstention,
            }],
        });
    }

    Ok(cases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lme_date_format() {
        let d = parse_date("2023/05/30 (Tue) 23:40").unwrap();
        assert_eq!(d.to_rfc3339(), "2023-05-30T23:40:00+00:00");
    }

    #[test]
    fn inconsistent_weekday_still_parses() {
        // 2023/05/30 is a Tuesday; a row mislabeling it "(Wed)" must not abort the run.
        let d = parse_date("2023/05/30 (Wed) 23:40").unwrap();
        assert_eq!(d.to_rfc3339(), "2023-05-30T23:40:00+00:00");
    }

    #[test]
    fn converts_instance_and_flags_abstention() {
        // Unknown fields (question_date, has_answer, answer_session_ids,
        // haystack_session_ids) are ignored, which is what a faithful converter wants.
        let raw = r#"[
          {"question_id":"q1","question_type":"single-session-user","question":"Where do I live?",
           "answer":"San Francisco","question_date":"2023/05/30 (Tue) 23:40",
           "answer_session_ids":["s1"],
           "haystack_session_ids":["s1"],
           "haystack_dates":["2023/04/02 (Sun) 10:15"],
           "haystack_sessions":[[{"role":"user","content":"I live in San Francisco.","has_answer":true},
                                 {"role":"assistant","content":"Nice city."}]]},
          {"question_id":"q2_abs","question_type":"single-session-user","question":"When did I move to NYC?",
           "answer":"never mentioned","haystack_dates":[],"haystack_sessions":[]}
        ]"#;
        let cases = convert(raw).unwrap();
        assert_eq!(cases.len(), 2, "abstention instance is included, not skipped");

        let q1 = &cases[0];
        assert_eq!(q1.id, "q1");
        assert_eq!(q1.questions[0].question_type.as_deref(), Some("single-session-user"));
        assert!(!q1.questions[0].abstention);
        assert_eq!(q1.sessions.len(), 1);
        assert_eq!(q1.sessions[0].turns[0].content, "I live in San Francisco.");
        assert!(q1.sessions[0].date.is_some());

        let abs = &cases[1];
        assert_eq!(abs.id, "q2_abs");
        assert!(abs.questions[0].abstention, "_abs question must be flagged");
        assert_eq!(abs.questions[0].question_type.as_deref(), Some("single-session-user"));
    }
}
