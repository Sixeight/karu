use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::candidate::{JevInput, Nouls};

const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
const MODEL: &str = "jev-latest";
pub const CHUNK_SIZE: usize = 40;
const MAX_RETRIES: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum TypesafeError {
    #[error("TypeSafe API error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("TypeSafe request failed: {0}")]
    Http(#[from] reqwest::Error),
}

pub struct Client {
    http: reqwest::blocking::Client,
    api_key: String,
    base_url: String,
}

impl Client {
    /// `TYPESAFE_BASE_URL` points the client somewhere else (a proxy, or a
    /// local stand-in in tests).
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        let base_url = std::env::var("TYPESAFE_BASE_URL")
            .ok()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Self::with_base_url(api_key, base_url)
    }

    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        })
    }

    pub fn judge_chunk(&self, default_branch: &str, candidates: &[JevInput]) -> Result<Vec<Nouls>> {
        let request = chunk_request(default_branch, candidates);
        let response = self.post_system_one(&request)?;
        parse_chunk_answers(&response.answers, candidates.len())
    }

    fn post_system_one(&self, body: &Value) -> Result<SystemOneResponse> {
        let url = format!("{}/v1/systemone", self.base_url);
        let mut last_status = 0;
        let mut last_body = String::new();
        for attempt in 0..MAX_RETRIES {
            let response = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .map_err(TypesafeError::Http)?;
            let status = response.status();
            if status.is_success() {
                return response.json().context("invalid TypeSafe response JSON");
            }
            last_status = status.as_u16();
            last_body = response.text().unwrap_or_default();
            if matches!(last_status, 429 | 529) && attempt + 1 < MAX_RETRIES {
                thread::sleep(Duration::from_millis(200 * 2u64.pow(attempt)));
                continue;
            }
            break;
        }
        Err(TypesafeError::Api {
            status: last_status,
            body: last_body,
        }
        .into())
    }
}

pub fn judge_all(
    client: &Client,
    default_branch: &str,
    candidates: &[JevInput],
) -> Result<Vec<Nouls>> {
    judge_chunks(candidates, |chunk| {
        client.judge_chunk(default_branch, chunk)
    })
}

/// Chunks go out at the same time; answers come back in candidate order.
fn judge_chunks<F>(candidates: &[JevInput], judge: F) -> Result<Vec<Nouls>>
where
    F: Fn(&[JevInput]) -> Result<Vec<Nouls>> + Sync,
{
    let answers: Vec<Result<Vec<Nouls>>> = std::thread::scope(|s| {
        let handles: Vec<_> = candidates
            .chunks(CHUNK_SIZE)
            .map(|chunk| {
                let judge = &judge;
                s.spawn(move || judge(chunk))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("judging thread panicked")))
            })
            .collect()
    });
    let mut out = Vec::with_capacity(candidates.len());
    for chunk in answers {
        out.extend(chunk?);
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct SystemOneResponse {
    answers: BTreeMap<String, NoulAnswer>,
}

#[derive(Debug, Deserialize)]
struct NoulAnswer {
    noul: f64,
}

#[derive(Serialize)]
struct NoulQuestion {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: Value,
    criteria: Value,
}

fn noul(instructions: Value, yes: &str, no: &str) -> NoulQuestion {
    NoulQuestion {
        kind: "noul",
        instructions,
        criteria: json!({ "true": yes, "false": no }),
    }
}

pub fn chunk_request(default_branch: &str, candidates: &[JevInput]) -> Value {
    let mut questions = serde_json::Map::new();
    for i in 0..candidates.len() {
        let prefix = format!("c{i}");
        questions.insert(
            format!("{prefix}_work_already_landed"),
            serde_json::to_value(noul(
                json!({
                    "question": format!(
                        "Does the unique work on `candidates[{i}]` already appear to have landed on `{default_branch}`?"
                    ),
                    "inspect": [
                        format!("candidates[{i}].unique_subjects"),
                        format!("candidates[{i}].pr"),
                        format!("candidates[{i}].diffstat"),
                        format!("candidates[{i}].unique_commit_count"),
                        format!("candidates[{i}].local_only_commit_count"),
                        format!("candidates[{i}].ref_age_secs"),
                    ],
                    "focus": "Look for squash leftovers, a merged PR, or subjects that match already-landed work. Do not treat unmerged unique work as landed. An empty ref (unique_commit_count 0) created recently to start work is not landed leftover. A `pr` describes the branch name, not necessarily these commits: when local_only_commit_count is above 0 the commits were never pushed, so a merged PR of the same name does not prove they landed."
                }),
                "The unique commits are leftovers of work that is already on the default branch",
                "The unique commits still look like work that has not landed",
            ))
            .expect("noul json"),
        );
        questions.insert(
            format!("{prefix}_name_is_throwaway"),
            serde_json::to_value(noul(
                json!({
                    "question": format!(
                        "Is `candidates[{i}].branch` a throwaway or temporary branch name?"
                    ),
                    "examples_of_throwaway": ["tmp", "wip", "experiment", "scratch", "agent session", "spike"],
                    "focus": "Judge the name, not the commit quality."
                }),
                "The name marks temporary, experimental, or agent scratch work",
                "The name looks like a real feature, fix, or long-lived branch",
            ))
            .expect("noul json"),
        );
        questions.insert(
            format!("{prefix}_looks_abandoned"),
            serde_json::to_value(noul(
                json!({
                    "question": format!(
                        "Does `candidates[{i}]` look abandoned rather than paused active work?"
                    ),
                    "inspect": [
                        format!("candidates[{i}].ref_age_secs"),
                        format!("candidates[{i}].last_commit_at"),
                        format!("candidates[{i}].last_subject"),
                        format!("candidates[{i}].upstream_gone"),
                        format!("candidates[{i}].pr"),
                        format!("candidates[{i}].behind"),
                    ],
                    "focus": "Use `ref_age_secs` as idle time for this local name. A day or more idle leans abandoned. A few minutes means it was just created. last_commit_at on an empty ref is the default-branch commit, not branch creation. A `pr` whose state is CLOSED without merged_at was given up on and leans abandoned; an OPEN `pr` is still in flight."
                }),
                "It looks abandoned and unlikely to be resumed",
                "It looks like active or paused work that should be kept",
            ))
            .expect("noul json"),
        );
        questions.insert(
            format!("{prefix}_unique_commits_are_noise"),
            serde_json::to_value(noul(
                json!({
                    "question": format!(
                        "Are the unique commits on `candidates[{i}]` noise rather than work worth keeping?"
                    ),
                    "inspect": [
                        format!("candidates[{i}].unique_subjects"),
                        format!("candidates[{i}].diffstat"),
                        format!("candidates[{i}].unique_commit_count"),
                        format!("candidates[{i}].local_only_commit_count"),
                        format!("candidates[{i}].ref_age_secs"),
                    ],
                    "focus": "Discardable unique work includes wip, typo, lockfile, merge-from-default only, codegen-only, leftover review nits that already sit on default, or changes unrelated to any unfinished task. Unfinished feature work, tests, or a real bugfix are not discardable. local_only_commit_count above 0 means no remote holds these commits and deleting the branch loses them for good, so call them noise only when the subjects clearly are."
                }),
                "The unique commits are leftover, unrelated, or otherwise safe to drop",
                "The unique commits are unfinished work that would be lost",
            ))
            .expect("noul json"),
        );
        questions.insert(
            format!("{prefix}_empty_ref_is_leftover"),
            serde_json::to_value(noul(
                json!({
                    "question": format!(
                        "Is `candidates[{i}]` an empty leftover local name rather than a branch created to start new work?"
                    ),
                    "inspect": [
                        format!("candidates[{i}].branch"),
                        format!("candidates[{i}].unique_commit_count"),
                        format!("candidates[{i}].ref_age_secs"),
                        format!("candidates[{i}].last_subject"),
                    ],
                    "focus": "unique_commit_count 0 plus idle time of a day or more, or a finished-looking name (PR number, old ticket), is leftover. unique_commit_count 0 plus a small ref_age_secs is a fresh workspace. Unique commits mean it is not an empty leftover."
                }),
                "Empty local name left after the work already sits on the default branch",
                "Still unique work, or an empty branch created recently to start work",
            ))
            .expect("noul json"),
        );
    }

    json!({
        "state": {
            "default_branch": default_branch,
            "candidates": candidates,
        },
        "model": MODEL,
        "questions": questions,
    })
}

fn parse_chunk_answers(answers: &BTreeMap<String, NoulAnswer>, count: usize) -> Result<Vec<Nouls>> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let prefix = format!("c{i}");
        out.push(Nouls {
            work_already_landed: noul_value(answers, &format!("{prefix}_work_already_landed"))?,
            name_is_throwaway: noul_value(answers, &format!("{prefix}_name_is_throwaway"))?,
            looks_abandoned: noul_value(answers, &format!("{prefix}_looks_abandoned"))?,
            unique_commits_are_noise: noul_value(
                answers,
                &format!("{prefix}_unique_commits_are_noise"),
            )?,
            empty_ref_is_leftover: noul_value(answers, &format!("{prefix}_empty_ref_is_leftover"))?,
        });
    }
    Ok(out)
}

fn noul_value(answers: &BTreeMap<String, NoulAnswer>, key: &str) -> Result<f64> {
    answers
        .get(key)
        .map(|a| a.noul)
        .with_context(|| format!("missing TypeSafe answer {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::Candidate;

    fn input(name: &str) -> JevInput {
        candidate(name).jev_input()
    }

    fn candidate(name: &str) -> Candidate {
        Candidate {
            branch: Some(name.into()),
            behind: Some(3),
            diffstat: Some("1 file changed, 1 insertion(+)".into()),
            upstream_gone: true,
            ..crate::candidate::fixtures::candidate()
        }
    }

    #[test]
    fn request_asks_five_nouls_per_candidate() {
        let req = chunk_request("main", &[input("sixeight/tmp")]);
        assert_eq!(req["model"], "jev-latest");
        let questions = req["questions"].as_object().unwrap();
        assert_eq!(questions.len(), 5);
        let raw = req.to_string();
        for retired in ["squash_merged", "\"ahead\""] {
            assert!(!raw.contains(retired), "{retired} is still sent to Jev");
        }
        for key in [
            "c0_work_already_landed",
            "c0_name_is_throwaway",
            "c0_looks_abandoned",
            "c0_unique_commits_are_noise",
            "c0_empty_ref_is_leftover",
        ] {
            assert_eq!(questions[key]["type"], "noul");
            assert!(questions[key]["instructions"].is_object());
            assert!(questions[key]["criteria"]["true"].is_string());
            assert!(questions[key]["criteria"]["false"].is_string());
        }
        assert_eq!(req["state"]["default_branch"], "main");
        for question in ["c0_work_already_landed", "c0_unique_commits_are_noise"] {
            let inspect = req["questions"][question]["instructions"]["inspect"].to_string();
            assert!(
                inspect.contains("candidates[0].local_only_commit_count"),
                "{question}: {inspect}"
            );
        }
        assert_eq!(req["state"]["candidates"][0]["branch"], "sixeight/tmp");
    }

    /// Every field that is sent is one a question points at.
    #[test]
    fn every_field_sent_is_inspected_by_some_question() {
        let mut c = candidate("sixeight/tmp");
        c.unique_subjects = vec!["feat: one".into()];
        c.ref_age_secs = Some(60);
        c.pr = Some(crate::candidate::PrInfo {
            state: crate::candidate::PrState::Closed,
            title: "t".into(),
            merged_at: None,
            head_sha: None,
            number: None,
        });
        let req = chunk_request("main", &[c.jev_input()]);
        let inspected = req["questions"].to_string();
        for field in req["state"]["candidates"][0].as_object().unwrap().keys() {
            assert!(
                inspected.contains(&format!("candidates[0].{field}")),
                "`{field}` is sent to Jev but no question inspects it"
            );
        }
    }

    #[test]
    fn nothing_about_the_local_machine_is_sent() {
        let mut c = candidate("sixeight/tmp");
        c.worktree_path = Some("/Users/someone/src/secret-project-wt".into());
        c.sha = Some("0123456789abcdef0123456789abcdef01234567".into());
        let raw = chunk_request("main", &[c.jev_input()]).to_string();
        for private in [
            "worktree_path",
            "/Users/someone",
            "secret-project-wt",
            "\"sha\"",
            "0123456789abcdef",
        ] {
            assert!(!raw.contains(private), "{private} is sent to Jev: {raw}");
        }
        assert!(raw.contains("sixeight/tmp"));
    }

    #[test]
    fn parse_answers_maps_by_index() {
        let mut answers = BTreeMap::new();
        answers.insert("c0_work_already_landed".into(), NoulAnswer { noul: 0.9 });
        answers.insert("c0_name_is_throwaway".into(), NoulAnswer { noul: 0.1 });
        answers.insert("c0_looks_abandoned".into(), NoulAnswer { noul: 0.2 });
        answers.insert(
            "c0_unique_commits_are_noise".into(),
            NoulAnswer { noul: 0.3 },
        );
        answers.insert("c0_empty_ref_is_leftover".into(), NoulAnswer { noul: 0.4 });
        let parsed = parse_chunk_answers(&answers, 1).unwrap();
        assert_eq!(parsed[0].work_already_landed, 0.9);
        assert_eq!(parsed[0].name_is_throwaway, 0.1);
    }

    #[test]
    fn parse_answers_requires_every_key() {
        let answers = BTreeMap::new();
        let err = parse_chunk_answers(&answers, 1).unwrap_err();
        assert!(err.to_string().contains("c0_work_already_landed"));
    }

    #[test]
    fn client_reads_noul_answers_from_http() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let body = r#"{"model":"jev-latest","answers":{"c0_work_already_landed":{"type":"noul","noul":0.91},"c0_name_is_throwaway":{"type":"noul","noul":0.1},"c0_looks_abandoned":{"type":"noul","noul":0.2},"c0_unique_commits_are_noise":{"type":"noul","noul":0.3},"c0_empty_ref_is_leftover":{"type":"noul","noul":0.4}},"usage":{"input_tokens":1,"output_tokens":1}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });

        let client = Client::with_base_url("test-key", format!("http://{addr}")).unwrap();
        let nouls = client.judge_chunk("main", &[input("x")]).unwrap();
        assert_eq!(nouls[0].work_already_landed, 0.91);
        assert_eq!(nouls[0].unique_commits_are_noise, 0.3);
        handle.join().unwrap();
    }

    #[test]
    fn chunks_are_judged_in_parallel_but_answers_keep_candidate_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let candidates: Vec<JevInput> = (0..CHUNK_SIZE * 2 + 5)
            .map(|i| input(&format!("b{i}")))
            .collect();
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let nouls = judge_chunks(&candidates, |chunk| {
            // later chunks answer first, so a naive "as completed" merge would reorder
            let first: usize = chunk[0].branch.as_deref().unwrap()[1..].parse().unwrap();
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            let started = std::time::Instant::now();
            while peak.load(Ordering::SeqCst) < 2 && started.elapsed() < Duration::from_millis(500)
            {
                std::thread::yield_now();
            }
            in_flight.fetch_sub(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(if first == 0 { 30 } else { 0 }));
            Ok(chunk
                .iter()
                .map(|c| {
                    let i: f64 = c.branch.as_deref().unwrap()[1..].parse().unwrap();
                    Nouls {
                        work_already_landed: i,
                        name_is_throwaway: 0.0,
                        looks_abandoned: 0.0,
                        unique_commits_are_noise: 0.0,
                        empty_ref_is_leftover: 0.0,
                    }
                })
                .collect())
        })
        .unwrap();
        let order: Vec<usize> = nouls
            .iter()
            .map(|n| n.work_already_landed as usize)
            .collect();
        assert_eq!(order, (0..candidates.len()).collect::<Vec<_>>());
        assert!(
            peak.load(Ordering::SeqCst) >= 2,
            "chunks ran one after another"
        );
    }

    #[test]
    fn one_failed_chunk_fails_the_whole_judgement() {
        let candidates: Vec<JevInput> = (0..CHUNK_SIZE + 1)
            .map(|i| input(&format!("b{i}")))
            .collect();
        let result = judge_chunks(&candidates, |chunk| {
            if chunk.len() == 1 {
                anyhow::bail!("boom")
            }
            Ok(Vec::new())
        });
        assert!(result.unwrap_err().to_string().contains("boom"));
    }

    #[test]
    fn parse_answers_empty_chunk() {
        let answers = BTreeMap::new();
        let parsed = parse_chunk_answers(&answers, 0).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_answers_ignores_unknown_keys_and_maps_second_candidate() {
        let mut answers = BTreeMap::new();
        for (i, landed) in [(0, 0.0), (1, 1.0)] {
            answers.insert(
                format!("c{i}_work_already_landed"),
                NoulAnswer { noul: landed },
            );
            answers.insert(format!("c{i}_name_is_throwaway"), NoulAnswer { noul: 0.0 });
            answers.insert(format!("c{i}_looks_abandoned"), NoulAnswer { noul: 0.0 });
            answers.insert(
                format!("c{i}_unique_commits_are_noise"),
                NoulAnswer { noul: 0.0 },
            );
            answers.insert(
                format!("c{i}_empty_ref_is_leftover"),
                NoulAnswer { noul: 0.0 },
            );
        }
        answers.insert("extra".into(), NoulAnswer { noul: 0.42 });
        let parsed = parse_chunk_answers(&answers, 2).unwrap();
        assert_eq!(parsed[0].work_already_landed, 0.0);
        assert_eq!(parsed[1].work_already_landed, 1.0);
    }

    #[test]
    fn parse_answers_rejects_partial_second_candidate() {
        let mut answers = BTreeMap::new();
        answers.insert("c0_work_already_landed".into(), NoulAnswer { noul: 0.1 });
        answers.insert("c0_name_is_throwaway".into(), NoulAnswer { noul: 0.1 });
        answers.insert("c0_looks_abandoned".into(), NoulAnswer { noul: 0.1 });
        answers.insert(
            "c0_unique_commits_are_noise".into(),
            NoulAnswer { noul: 0.1 },
        );
        answers.insert("c0_empty_ref_is_leftover".into(), NoulAnswer { noul: 0.1 });
        let err = parse_chunk_answers(&answers, 2).unwrap_err();
        assert!(err.to_string().contains("c1_work_already_landed"));
    }

    #[test]
    fn client_http_error_does_not_judge() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let body = "nope";
            let resp = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });

        let client = Client::with_base_url("test-key", format!("http://{addr}")).unwrap();
        let err = client.judge_chunk("main", &[input("x")]).unwrap_err();
        assert!(err.to_string().contains("500"), "ERROR: {err}");
        handle.join().unwrap();
    }
}
