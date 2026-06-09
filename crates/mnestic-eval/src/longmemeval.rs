// SPDX-License-Identifier: Apache-2.0

//! Convert LongMemEval (`longmemeval_s.json` / `_m` / `_oracle`) into Mnestic's
//! normalized dataset. The file is a JSON array of instances; each carries a
//! question plus a "haystack" of dated sessions. `haystack_dates` is parallel to
//! `haystack_sessions`. Abstention instances (question_id ending `_abs`) are skipped
//! because grading them needs a dedicated "did the model abstain" judge, not the
//! generic correctness judge.

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

/// Convert LongMemEval JSON into normalized cases. Returns the cases plus the number
/// of abstention instances skipped, so the caller can report coverage honestly.
pub fn convert(raw: &str) -> Result<(Vec<Case>, usize)> {
    let instances: Vec<LmeInstance> =
        serde_json::from_str(raw).context("parsing LongMemEval JSON")?;

    let mut cases = Vec::new();
    let mut skipped_abstention = 0usize;

    for inst in instances {
        if inst.question_id.ends_with("_abs") {
            skipped_abstention += 1;
            continue;
        }
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
            }],
        });
    }

    Ok((cases, skipped_abstention))
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
    fn converts_instance_and_skips_abstention() {
        // Unknown fields (question_type, question_date, has_answer, answer_session_ids)
        // are ignored, which is what a faithful converter wants.
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
        let (cases, skipped) = convert(raw).unwrap();
        assert_eq!(skipped, 1, "abstention instance should be skipped");
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].id, "q1");
        assert_eq!(cases[0].sessions.len(), 1);
        assert_eq!(cases[0].sessions[0].turns.len(), 2);
        assert_eq!(cases[0].sessions[0].turns[0].content, "I live in San Francisco.");
        assert!(cases[0].sessions[0].date.is_some());
        assert_eq!(cases[0].questions[0].answer, "San Francisco");
    }
}
