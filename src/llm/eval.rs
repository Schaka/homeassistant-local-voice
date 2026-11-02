//! Evaluation of a conversation agent: `assistant-hub eval llm`.
//!
//! The questions in `src/llm/assist.json` are what Home Assistant's Assist
//! asks: an action, a state, the weather, a web search, small talk. Each
//! question has a scripted tool result, so the run needs only the model and
//! the same input for every engine. The history goes back to the model the way
//! Home Assistant sends it: the text and the tool calls of earlier answers, but
//! not their thinking.
//!
//! A run talks to any OpenAI compatible `/v1` URL. That can be the hub
//! (`/llm/v1`) or an engine directly. `eval compare A.json B.json` decides
//! whether engine B may replace engine A: B must answer as well as A and be
//! faster. Questions that must find nothing (small talk, a question that must
//! not change a list) are part of the set, because a gate without negatives
//! passes anything.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const FIXTURE: &str = include_str!("assist.json");

/// Rounds of tool calls per question before the run gives up.
const MAX_ROUNDS: usize = 4;

#[derive(Debug, Clone, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolExpect {
    pub name: String,
    /// Argument name and a text that its value must contain (any case).
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    pub id: String,
    pub text: String,
    /// The tool that the model must call. `null`: it must call none, except
    /// the tools in `allow_tools`.
    #[serde(default)]
    pub tool: Option<ToolExpect>,
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(default)]
    pub forbid_tools: Vec<String>,
    /// Tool name and the scripted result.
    #[serde(default)]
    pub results: BTreeMap<String, String>,
    #[serde(default)]
    pub answer_any: Vec<String>,
    #[serde(default)]
    pub answer_all: Vec<String>,
    #[serde(default)]
    pub answer_none: Vec<String>,
    #[serde(default)]
    pub answer_min_chars: Option<usize>,
    /// Models fail this one today. It is reported but does not gate.
    #[serde(default)]
    pub known_failure: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Fixture {
    pub version: u32,
    pub system: String,
    pub filler_devices: usize,
    pub tools: Vec<ToolDef>,
    pub questions: Vec<Question>,
}

impl Fixture {
    pub fn builtin() -> Result<Self> {
        serde_json::from_str(FIXTURE).context("src/llm/assist.json")
    }

    /// The system prompt: the house, plus filler sensors so the prompt is as
    /// long as a real Assist prompt.
    pub fn system_prompt(&self) -> String {
        let mut prompt = self.system.clone();
        for i in 0..self.filler_devices {
            prompt.push_str(&format!(
                "\n- names: Room {i} Temperature\n  domain: sensor\n  state: '{}.5'\n  attributes:\n    unit_of_measurement: °C\n  areas: Room {i}",
                18 + i % 7
            ));
        }
        prompt
    }

    fn tools_json(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": t.name, "description": t.description, "parameters": t.parameters}})
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub base_url: String,
    pub key: Option<String>,
    pub repeat: usize,
    pub temperature: f64,
    pub top_p: f64,
    /// Run only these question ids. Empty: all.
    pub only: Vec<String>,
    pub timeout: Duration,
}

/// The result of one question in one repeat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub id: String,
    pub repeat: usize,
    pub known_failure: bool,
    pub correct: bool,
    pub reasons: Vec<String>,
    /// The final reply has no text. Home Assistant shows an error then.
    pub empty_reply: bool,
    /// A wrong, missing or forbidden tool call, or arguments that are no JSON.
    pub tool_error: bool,
    /// A failed request (HTTP error, timeout).
    pub error: Option<String>,
    pub tools: Vec<String>,
    pub answer: String,
    pub wall_s: f64,
    pub rounds: usize,
    /// Tokens that the engine processed (not read from its cache).
    pub prompt_n: u64,
    /// The most tokens that one request of the question processed.
    #[serde(default)]
    pub max_round_prompt_n: u64,
    pub cached: u64,
    pub prompt_ms: f64,
    pub predicted_n: u64,
    pub predicted_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub base_url: String,
    pub model: String,
    pub temperature: f64,
    pub top_p: f64,
    pub repeat: usize,
    pub fixture_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub gating_total: usize,
    pub gating_correct: usize,
    pub known_failure_total: usize,
    pub known_failure_correct: usize,
    pub empty_replies: usize,
    pub tool_errors: usize,
    pub errors: usize,
    /// Wall time of the first request of the run. The engine has no prompt
    /// cache then, so this is the time to prefill the whole prompt.
    pub first_sample_s: f64,
    /// Mean time per question, without the first sample.
    pub mean_s: f64,
    pub median_s: f64,
    /// Tokens per second, over all requests.
    pub prompt_tps: f64,
    pub generation_tps: f64,
    /// Mean tokens that the engine processed per question, without the first.
    pub processed_per_question: f64,
    /// The most tokens that any request after the first processed. A large
    /// value means a request that read the whole prompt again.
    #[serde(default)]
    pub max_processed_round: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub meta: Meta,
    pub summary: Summary,
    pub samples: Vec<Sample>,
}

impl Summary {
    pub fn from_samples(samples: &[Sample]) -> Self {
        let gating: Vec<&Sample> = samples.iter().filter(|s| !s.known_failure).collect();
        let known: Vec<&Sample> = samples.iter().filter(|s| s.known_failure).collect();
        let rest: Vec<&Sample> = samples.iter().skip(1).collect();
        let mut times: Vec<f64> = rest.iter().map(|s| s.wall_s).collect();
        times.sort_by(|a, b| a.total_cmp(b));
        let mean = |v: &[f64]| {
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        };
        let median = match times.len() {
            0 => 0.0,
            n if n % 2 == 1 => times[n / 2],
            n => (times[n / 2 - 1] + times[n / 2]) / 2.0,
        };
        let prompt_ms: f64 = samples.iter().map(|s| s.prompt_ms).sum();
        let predicted_ms: f64 = samples.iter().map(|s| s.predicted_ms).sum();
        let prompt_n: u64 = samples.iter().map(|s| s.prompt_n).sum();
        let predicted_n: u64 = samples.iter().map(|s| s.predicted_n).sum();
        Self {
            gating_total: gating.len(),
            gating_correct: gating.iter().filter(|s| s.correct).count(),
            known_failure_total: known.len(),
            known_failure_correct: known.iter().filter(|s| s.correct).count(),
            empty_replies: samples.iter().filter(|s| s.empty_reply).count(),
            tool_errors: samples.iter().filter(|s| s.tool_error).count(),
            errors: samples.iter().filter(|s| s.error.is_some()).count(),
            first_sample_s: samples.first().map_or(0.0, |s| s.wall_s),
            mean_s: mean(&times),
            median_s: median,
            prompt_tps: if prompt_ms > 0.0 {
                prompt_n as f64 * 1000.0 / prompt_ms
            } else {
                0.0
            },
            generation_tps: if predicted_ms > 0.0 {
                predicted_n as f64 * 1000.0 / predicted_ms
            } else {
                0.0
            },
            processed_per_question: mean(&rest.iter().map(|s| s.prompt_n as f64).collect::<Vec<_>>()),
            max_processed_round: rest.iter().map(|s| s.max_round_prompt_n).max().unwrap_or(0),
        }
    }
}

struct Client {
    http: reqwest::Client,
    base: String,
    key: Option<String>,
}

impl Client {
    fn new(opts: &RunOptions) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(opts.timeout).build()?,
            base: opts.base_url.trim_end_matches('/').to_owned(),
            key: opts.key.clone(),
        })
    }

    fn auth(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.key {
            Some(k) => r.bearer_auth(k),
            None => r,
        }
    }

    async fn model(&self) -> String {
        let got = async {
            let v: Value = self
                .auth(self.http.get(format!("{}/models", self.base)))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok::<_, anyhow::Error>(v["data"][0]["id"].as_str().map(str::to_owned))
        }
        .await;
        got.ok().flatten().unwrap_or_else(|| "local".to_owned())
    }

    async fn chat(&self, body: &Value) -> Result<Value> {
        let resp = self
            .auth(self.http.post(format!("{}/chat/completions", self.base)).json(body))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("HTTP {status}: {}", text.chars().take(200).collect::<String>());
        }
        serde_json::from_str(&text).context("the answer is no JSON")
    }
}

fn contains(text: &str, needle: &str) -> bool {
    text.to_lowercase().contains(&needle.to_lowercase())
}

/// Checks the tool calls of one question. Returns the reasons for a failure.
fn check_tools(q: &Question, calls: &[(String, Value)]) -> Vec<String> {
    let mut reasons = Vec::new();
    for (name, _) in calls {
        if q.forbid_tools.contains(name) {
            reasons.push(format!("called the forbidden tool {name}"));
        }
    }
    match &q.tool {
        Some(want) => match calls.iter().find(|(n, _)| *n == want.name) {
            None => reasons.push(format!("did not call {}", want.name)),
            Some((_, args)) => {
                for (key, text) in &want.args {
                    let value = match &args[key] {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    if !contains(&value, text) {
                        reasons.push(format!("argument {key}={value:?} lacks {text:?}"));
                    }
                }
            }
        },
        None => {
            for (name, _) in calls {
                if !q.allow_tools.contains(name) && !q.forbid_tools.contains(name) {
                    reasons.push(format!("called {name}, but no tool is needed"));
                }
            }
        }
    }
    reasons
}

fn check_answer(q: &Question, answer: &str) -> Vec<String> {
    let mut reasons = Vec::new();
    let min = q.answer_min_chars.unwrap_or(1);
    if answer.trim().chars().count() < min {
        reasons.push(format!("the answer has fewer than {min} characters"));
    }
    if !q.answer_any.is_empty() && !q.answer_any.iter().any(|n| contains(answer, n)) {
        reasons.push(format!("the answer has none of {:?}", q.answer_any));
    }
    for n in &q.answer_all {
        if !contains(answer, n) {
            reasons.push(format!("the answer lacks {n:?}"));
        }
    }
    for n in &q.answer_none {
        if contains(answer, n) {
            reasons.push(format!("the answer has {n:?}"));
        }
    }
    reasons
}

#[derive(Default)]
struct Timings {
    prompt_n: u64,
    max_round_prompt_n: u64,
    cached: u64,
    prompt_ms: f64,
    predicted_n: u64,
    predicted_ms: f64,
}

impl Timings {
    /// Both engines report `timings`. The cached tokens come from `usage`.
    fn add(&mut self, resp: &Value) {
        let t = &resp["timings"];
        self.prompt_n += t["prompt_n"].as_u64().unwrap_or(0);
        self.max_round_prompt_n = self.max_round_prompt_n.max(t["prompt_n"].as_u64().unwrap_or(0));
        self.prompt_ms += t["prompt_ms"].as_f64().unwrap_or(0.0);
        self.predicted_n += t["predicted_n"].as_u64().unwrap_or(0);
        self.predicted_ms += t["predicted_ms"].as_f64().unwrap_or(0.0);
        self.cached += resp["usage"]["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| t["cache_n"].as_u64())
            .unwrap_or(0);
    }
}

async fn run_question(
    client: &Client,
    fx: &Fixture,
    q: &Question,
    repeat: usize,
    opts: &RunOptions,
    model: &str,
) -> Sample {
    let start = Instant::now();
    let mut messages = vec![
        json!({"role": "system", "content": fx.system_prompt()}),
        json!({"role": "user", "content": q.text}),
    ];
    let tools = fx.tools_json();
    let mut calls: Vec<(String, Value)> = Vec::new();
    let mut reasons = Vec::new();
    let mut timings = Timings::default();
    let mut tool_error = false;
    let mut error = None;
    let mut answer = String::new();
    let mut rounds = 0;
    loop {
        if rounds == MAX_ROUNDS {
            reasons.push(format!("still calling tools after {MAX_ROUNDS} rounds"));
            tool_error = true;
            break;
        }
        rounds += 1;
        let body = json!({
            "model": model, "messages": messages, "tools": tools,
            "temperature": opts.temperature, "top_p": opts.top_p, "stream": false,
        });
        let resp = match client.chat(&body).await {
            Ok(r) => r,
            Err(err) => {
                error = Some(format!("{err:#}"));
                reasons.push(format!("request failed: {err:#}"));
                break;
            }
        };
        timings.add(&resp);
        let msg = &resp["choices"][0]["message"];
        let wanted: Vec<Value> = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        if wanted.is_empty() {
            answer = msg["content"].as_str().unwrap_or("").to_owned();
            break;
        }
        // Home Assistant sends the text and the tool calls back, never the thinking.
        messages
            .push(json!({"role": "assistant", "content": msg["content"].as_str().unwrap_or(""), "tool_calls": wanted}));
        for (i, call) in wanted.iter().enumerate() {
            let name = call["function"]["name"].as_str().unwrap_or("").to_owned();
            let raw = call["function"]["arguments"].as_str().unwrap_or("{}");
            let args = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| {
                tool_error = true;
                reasons.push(format!("the arguments of {name} are no JSON: {raw}"));
                Value::Null
            });
            let id = call["id"]
                .as_str()
                .map_or_else(|| format!("call_{rounds}_{i}"), str::to_owned);
            let result = q.results.get(&name).cloned().unwrap_or_else(|| "ok".to_owned());
            messages.push(json!({"role": "tool", "tool_call_id": id, "content": result}));
            calls.push((name, args));
        }
    }
    let empty_reply = error.is_none() && answer.trim().is_empty();
    if error.is_none() {
        let tool_reasons = check_tools(q, &calls);
        tool_error |= !tool_reasons.is_empty();
        reasons.extend(tool_reasons);
        if empty_reply {
            reasons.push("the reply is empty".to_owned());
        }
        reasons.extend(check_answer(q, &answer));
    }
    Sample {
        id: q.id.clone(),
        repeat,
        known_failure: q.known_failure,
        correct: reasons.is_empty(),
        reasons,
        empty_reply,
        tool_error,
        error,
        tools: calls.into_iter().map(|(n, _)| n).collect(),
        answer,
        wall_s: start.elapsed().as_secs_f64(),
        rounds,
        prompt_n: timings.prompt_n,
        max_round_prompt_n: timings.max_round_prompt_n,
        cached: timings.cached,
        prompt_ms: timings.prompt_ms,
        predicted_n: timings.predicted_n,
        predicted_ms: timings.predicted_ms,
    }
}

/// Runs the questions `repeat` times each, in order, on one connection URL.
pub async fn run(fx: &Fixture, opts: &RunOptions) -> Result<Report> {
    let client = Client::new(opts)?;
    let model = client.model().await;
    let questions: Vec<&Question> = fx
        .questions
        .iter()
        .filter(|q| opts.only.is_empty() || opts.only.contains(&q.id))
        .collect();
    if questions.is_empty() {
        bail!("no question matches {:?}", opts.only);
    }
    let mut samples = Vec::new();
    for repeat in 0..opts.repeat.max(1) {
        for q in &questions {
            let s = run_question(&client, fx, q, repeat, opts, &model).await;
            println!(
                "{:<2} {:<22} {:>6.1} s  {}",
                if s.correct { "ok" } else { "NO" },
                s.id,
                s.wall_s,
                s.reasons.join("; ")
            );
            samples.push(s);
        }
    }
    Ok(Report {
        meta: Meta {
            base_url: opts.base_url.clone(),
            model,
            temperature: opts.temperature,
            top_p: opts.top_p,
            repeat: opts.repeat.max(1),
            fixture_version: fx.version,
        },
        summary: Summary::from_samples(&samples),
        samples,
    })
}

pub fn print_summary(r: &Report) {
    let s = &r.summary;
    println!("model {}", r.meta.model);
    println!(
        "correct {}/{} (known failures {}/{}), empty replies {}, tool errors {}, request errors {}",
        s.gating_correct,
        s.gating_total,
        s.known_failure_correct,
        s.known_failure_total,
        s.empty_replies,
        s.tool_errors,
        s.errors
    );
    println!(
        "first request {:.1} s, then {:.1} s mean and {:.1} s median per question, {:.0} tokens processed per question (at most {} in one request)",
        s.first_sample_s, s.mean_s, s.median_s, s.processed_per_question, s.max_processed_round
    );
    println!(
        "prompt {:.0} tokens/s, generation {:.1} tokens/s",
        s.prompt_tps, s.generation_tps
    );
}

/// What engine B must achieve against engine A.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    /// B may answer this share of the questions less often than A.
    pub quality_slack: f64,
    /// B must be this much faster per question ...
    pub time_gain: f64,
    /// ... or this much faster at the first request (the full prefill).
    pub first_gain: f64,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            quality_slack: 0.05,
            time_gain: 0.20,
            first_gain: 0.30,
        }
    }
}

#[derive(Debug)]
pub struct Verdict {
    pub pass: bool,
    pub lines: Vec<String>,
}

fn rate(n: usize, total: usize) -> f64 {
    if total == 0 { 0.0 } else { n as f64 / total as f64 }
}

/// Decides whether B may replace A.
pub fn compare(a: &Report, b: &Report, gate: Gate) -> Verdict {
    let (sa, sb) = (&a.summary, &b.summary);
    let mut lines = Vec::new();
    let mut pass = true;
    let mut check = |ok: bool, text: String| {
        lines.push(format!("{} {text}", if ok { "PASS" } else { "FAIL" }));
        pass &= ok;
    };
    let (qa, qb) = (
        rate(sa.gating_correct, sa.gating_total),
        rate(sb.gating_correct, sb.gating_total),
    );
    check(
        qb + gate.quality_slack >= qa,
        format!(
            "correct answers {:.0}% against {:.0}% (slack {:.0} points)",
            qb * 100.0,
            qa * 100.0,
            gate.quality_slack * 100.0
        ),
    );
    let (ea, eb) = (
        rate(sa.empty_replies, a.samples.len()),
        rate(sb.empty_replies, b.samples.len()),
    );
    check(
        eb <= ea + gate.quality_slack,
        format!("empty replies {:.0}% against {:.0}%", eb * 100.0, ea * 100.0),
    );
    let (ta, tb) = (
        rate(sa.tool_errors, a.samples.len()),
        rate(sb.tool_errors, b.samples.len()),
    );
    check(
        tb <= ta + gate.quality_slack,
        format!("tool errors {:.0}% against {:.0}%", tb * 100.0, ta * 100.0),
    );
    check(sb.errors == 0, format!("{} failed requests", sb.errors));
    let per_question = sb.mean_s <= sa.mean_s * (1.0 - gate.time_gain);
    let first = sb.first_sample_s <= sa.first_sample_s * (1.0 - gate.first_gain);
    check(
        per_question || first,
        format!(
            "speed: {:.1} s against {:.1} s per question (needs {:.0}% less), first request {:.1} s against {:.1} s (needs {:.0}% less)",
            sb.mean_s,
            sa.mean_s,
            gate.time_gain * 100.0,
            sb.first_sample_s,
            sa.first_sample_s,
            gate.first_gain * 100.0
        ),
    );
    Verdict { pass, lines }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::fake::Fake;

    fn tool_reply(name: &str, args: &str) -> String {
        json!({"choices": [{"finish_reason": "tool_calls", "index": 0, "message": {
            "role": "assistant", "content": "", "reasoning_content": "thinking about it",
            "tool_calls": [{"id": "c1", "type": "function", "function": {"name": name, "arguments": args}}]}}],
            "usage": {"prompt_tokens_details": {"cached_tokens": 100}},
            "timings": {"prompt_n": 50, "prompt_ms": 100.0, "predicted_n": 20, "predicted_ms": 400.0}})
        .to_string()
    }

    fn text_reply(content: &str) -> String {
        json!({"choices": [{"finish_reason": "stop", "index": 0, "message": {
            "role": "assistant", "content": content, "reasoning_content": "some thoughts"}}],
            "usage": {"prompt_tokens_details": {"cached_tokens": 150}},
            "timings": {"prompt_n": 10, "prompt_ms": 20.0, "predicted_n": 10, "predicted_ms": 200.0}})
        .to_string()
    }

    fn question(json: Value) -> Question {
        serde_json::from_value(json).unwrap()
    }

    fn fixture(questions: Vec<Question>) -> Fixture {
        let mut fx = Fixture::builtin().unwrap();
        fx.questions = questions;
        fx.filler_devices = 2;
        fx
    }

    fn opts(port: u16) -> RunOptions {
        RunOptions {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            key: None,
            repeat: 1,
            temperature: 0.7,
            top_p: 1.0,
            only: vec![],
            timeout: Duration::from_secs(5),
        }
    }

    async fn run_with(replies: Vec<String>, q: Question, repeat: usize) -> (Report, Fake) {
        let fake = Fake::default();
        fake.set_chat_script(replies);
        let port = fake.spawn().await;
        let mut o = opts(port);
        o.repeat = repeat;
        (run(&fixture(vec![q]), &o).await.unwrap(), fake)
    }

    fn light_on() -> Question {
        question(json!({"id": "light-on", "text": "Turn on the kitchen light.",
            "tool": {"name": "HassTurnOn", "args": {"name": "kitchen"}},
            "results": {"HassTurnOn": "RESULT-OF-TOOL"}, "answer_any": ["kitchen"]}))
    }

    #[test]
    fn the_builtin_fixture_is_consistent() {
        let fx = Fixture::builtin().unwrap();
        let names: Vec<&str> = fx.tools.iter().map(|t| t.name.as_str()).collect();
        let mut ids = std::collections::BTreeSet::new();
        for q in &fx.questions {
            assert!(ids.insert(q.id.clone()), "duplicate id {}", q.id);
            let mentioned = q
                .tool
                .iter()
                .map(|t| &t.name)
                .chain(&q.allow_tools)
                .chain(&q.forbid_tools)
                .chain(q.results.keys());
            for tool in mentioned {
                assert!(names.contains(&tool.as_str()), "{}: unknown tool {tool}", q.id);
            }
            if let Some(t) = &q.tool {
                assert!(
                    q.results.contains_key(&t.name),
                    "{}: no scripted result for {}",
                    q.id,
                    t.name
                );
            }
            assert!(
                !q.answer_any.is_empty() || !q.answer_all.is_empty() || q.answer_min_chars.is_some(),
                "{}: no check of the answer",
                q.id
            );
        }
        // The set needs questions that must find nothing.
        assert!(fx.questions.iter().filter(|q| q.tool.is_none()).count() >= 3);
        assert!(fx.questions.iter().any(|q| !q.forbid_tools.is_empty()));
        assert!(fx.questions.iter().filter(|q| q.known_failure).count() <= 2);
    }

    #[test]
    fn the_system_prompt_grows_with_the_filler_devices() {
        let mut fx = Fixture::builtin().unwrap();
        fx.filler_devices = 0;
        let short = fx.system_prompt();
        fx.filler_devices = 10;
        let long = fx.system_prompt();
        assert!(short.contains("Kitchen Light"));
        assert!(long.contains("Room 9 Temperature") && !short.contains("Room 9"));
    }

    #[tokio::test]
    async fn a_tool_call_then_an_answer_is_correct_and_sent_back_the_way_home_assistant_does() {
        let (report, fake) = run_with(
            vec![
                tool_reply("HassTurnOn", r#"{"name":"light.kitchen"}"#),
                text_reply("The kitchen light is on."),
            ],
            light_on(),
            1,
        )
        .await;
        let s = &report.samples[0];
        assert!(s.correct, "{:?}", s.reasons);
        assert_eq!((s.rounds, s.tools.clone()), (2, vec!["HassTurnOn".to_owned()]));
        assert_eq!(s.answer, "The kitchen light is on.");
        // Timings of both requests add up.
        assert_eq!((s.prompt_n, s.predicted_n), (60, 30));
        assert_eq!(s.cached, 250);
        let chats = fake.seen_with("/v1/chat/completions");
        assert_eq!(chats.len(), 2);
        let second: Value = serde_json::from_slice(&chats[1].body).unwrap();
        let m = second["messages"].as_array().unwrap();
        assert_eq!(m.len(), 4);
        assert_eq!(m[2]["role"], "assistant");
        assert!(m[2].get("reasoning_content").is_none(), "thinking is not sent back");
        assert_eq!(m[2]["tool_calls"][0]["function"]["name"], "HassTurnOn");
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_call_id"], "c1");
        assert_eq!(m[3]["content"], "RESULT-OF-TOOL");
        assert_eq!(
            second["tools"].as_array().unwrap().len(),
            Fixture::builtin().unwrap().tools.len()
        );
        assert_eq!(second["temperature"], 0.7);
        assert_eq!(second["stream"], false);
    }

    #[tokio::test]
    async fn a_wrong_tool_or_a_wrong_argument_is_a_tool_error() {
        let (r, _) = run_with(
            vec![tool_reply("HassTurnOff", "{}"), text_reply("Done kitchen.")],
            light_on(),
            1,
        )
        .await;
        assert!(!r.samples[0].correct && r.samples[0].tool_error);
        assert!(
            r.samples[0]
                .reasons
                .iter()
                .any(|x| x.contains("did not call HassTurnOn"))
        );
        let (r, _) = run_with(
            vec![
                tool_reply("HassTurnOn", r#"{"name":"bedroom"}"#),
                text_reply("Kitchen done."),
            ],
            light_on(),
            1,
        )
        .await;
        assert!(
            r.samples[0].reasons.iter().any(|x| x.contains("lacks \"kitchen\"")),
            "{:?}",
            r.samples[0].reasons
        );
        let (r, _) = run_with(
            vec![tool_reply("HassTurnOn", "{broken"), text_reply("Kitchen done.")],
            light_on(),
            1,
        )
        .await;
        assert!(r.samples[0].tool_error);
    }

    #[tokio::test]
    async fn a_question_without_a_tool_fails_when_the_model_calls_one() {
        let q = question(json!({"id": "joke", "text": "Tell me a joke.", "tool": null, "answer_min_chars": 5}));
        let (ok, _) = run_with(vec![text_reply("Why did the chicken cross the road?")], q.clone(), 1).await;
        assert!(ok.samples[0].correct);
        let (bad, _) = run_with(
            vec![tool_reply("web_search", "{}"), text_reply("Here is a joke for you.")],
            q,
            1,
        )
        .await;
        assert!(!bad.samples[0].correct && bad.samples[0].tool_error);
    }

    #[tokio::test]
    async fn allowed_and_forbidden_tools() {
        let q = question(json!({"id": "list", "text": "Is milk on the list?", "tool": null,
            "allow_tools": ["GetLiveContext"], "forbid_tools": ["HassListAddItem"], "answer_any": ["milk"]}));
        let (ok, _) = run_with(
            vec![tool_reply("GetLiveContext", "{}"), text_reply("Yes, milk is on it.")],
            q.clone(),
            1,
        )
        .await;
        assert!(ok.samples[0].correct, "{:?}", ok.samples[0].reasons);
        let (bad, _) = run_with(
            vec![
                tool_reply("HassListAddItem", r#"{"item":"milk"}"#),
                text_reply("Added milk."),
            ],
            q,
            1,
        )
        .await;
        assert!(
            bad.samples[0]
                .reasons
                .iter()
                .any(|x| x.contains("forbidden tool HassListAddItem"))
        );
    }

    #[tokio::test]
    async fn an_empty_reply_is_counted_even_when_the_model_only_thought() {
        let (r, _) = run_with(
            vec![tool_reply("HassTurnOn", r#"{"name":"kitchen"}"#), text_reply("")],
            light_on(),
            1,
        )
        .await;
        let s = &r.samples[0];
        assert!(s.empty_reply && !s.correct);
        assert_eq!(r.summary.empty_replies, 1);
    }

    #[tokio::test]
    async fn answer_checks() {
        let q = question(json!({"id": "x", "text": "?", "tool": null,
            "answer_all": ["living", "bedroom"], "answer_none": ["kitchen"]}));
        let (ok, _) = run_with(vec![text_reply("The Living room and the BEDROOM.")], q.clone(), 1).await;
        assert!(ok.samples[0].correct);
        let (bad, _) = run_with(vec![text_reply("Living room and kitchen.")], q, 1).await;
        let reasons = bad.samples[0].reasons.join(" | ");
        assert!(
            reasons.contains("lacks \"bedroom\"") && reasons.contains("has \"kitchen\""),
            "{reasons}"
        );
    }

    #[tokio::test]
    async fn a_failed_request_is_recorded_and_the_run_goes_on() {
        let fake = Fake::default();
        fake.set_chat_status(500);
        let port = fake.spawn().await;
        let mut o = opts(port);
        o.repeat = 2;
        let r = run(&fixture(vec![light_on()]), &o).await.unwrap();
        assert_eq!(r.samples.len(), 2);
        assert!(
            r.samples
                .iter()
                .all(|s| s.error.as_deref().is_some_and(|e| e.contains("HTTP 500")))
        );
        assert_eq!(r.summary.errors, 2);
        assert_eq!(r.summary.empty_replies, 0, "an error is no empty reply");
    }

    #[tokio::test]
    async fn an_endless_tool_loop_stops() {
        let (r, fake) = run_with(vec![tool_reply("HassTurnOn", r#"{"name":"kitchen"}"#)], light_on(), 1).await;
        assert!(!r.samples[0].correct);
        assert!(r.samples[0].reasons.iter().any(|x| x.contains("still calling tools")));
        assert_eq!(fake.seen_with("/v1/chat/completions").len(), MAX_ROUNDS);
    }

    #[tokio::test]
    async fn repeat_and_only_select_the_samples() {
        let fake = Fake::default();
        fake.set_chat_script(vec![text_reply("Fine, kitchen.")]);
        let port = fake.spawn().await;
        let other = question(json!({"id": "other", "text": "Hello", "tool": null, "answer_min_chars": 2}));
        let mut o = opts(port);
        o.repeat = 3;
        o.only = vec!["other".into()];
        let r = run(&fixture(vec![light_on(), other]), &o).await.unwrap();
        assert_eq!(r.samples.len(), 3);
        assert!(r.samples.iter().all(|s| s.id == "other"));
        assert_eq!(r.samples.iter().map(|s| s.repeat).collect::<Vec<_>>(), [0, 1, 2]);
        o.only = vec!["missing".into()];
        assert!(run(&fixture(vec![light_on()]), &o).await.is_err());
    }

    fn sample(id: &str, correct: bool, wall: f64, prompt_n: u64) -> Sample {
        Sample {
            id: id.into(),
            repeat: 0,
            known_failure: false,
            correct,
            reasons: vec![],
            empty_reply: false,
            tool_error: false,
            error: None,
            tools: vec![],
            answer: String::new(),
            wall_s: wall,
            rounds: 1,
            prompt_n,
            max_round_prompt_n: prompt_n,
            cached: 0,
            prompt_ms: prompt_n as f64 * 10.0,
            predicted_n: 10,
            predicted_ms: 100.0,
        }
    }

    fn report(samples: Vec<Sample>) -> Report {
        Report {
            meta: Meta {
                base_url: "x".into(),
                model: "m".into(),
                temperature: 0.7,
                top_p: 1.0,
                repeat: 1,
                fixture_version: 1,
            },
            summary: Summary::from_samples(&samples),
            samples,
        }
    }

    #[test]
    fn the_summary() {
        let mut known = sample("floor", false, 9.0, 100);
        known.known_failure = true;
        let mut empty = sample("c", false, 5.0, 100);
        empty.empty_reply = true;
        let r = report(vec![
            sample("a", true, 40.0, 5000),
            sample("b", true, 2.0, 100),
            empty,
            known,
        ]);
        let s = &r.summary;
        assert_eq!((s.gating_total, s.gating_correct), (3, 2));
        assert_eq!((s.known_failure_total, s.known_failure_correct), (1, 0));
        assert_eq!(s.empty_replies, 1);
        assert_eq!(s.first_sample_s, 40.0);
        assert!((s.mean_s - 5.333).abs() < 0.01, "{}", s.mean_s);
        assert_eq!(s.median_s, 5.0);
        assert_eq!(s.processed_per_question, 100.0);
        assert_eq!(s.max_processed_round, 100, "the first sample is not counted");
        assert!((s.prompt_tps - 100.0).abs() < 0.01, "{}", s.prompt_tps);
        assert!((s.generation_tps - 100.0).abs() < 0.01);
    }

    #[test]
    fn a_report_survives_json() {
        let r = report(vec![sample("a", true, 1.0, 10)]);
        let back: Report = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.samples.len(), 1);
        assert_eq!(back.summary.gating_correct, 1);
    }

    fn many(correct: usize, total: usize, wall: f64, first: f64) -> Report {
        let mut v: Vec<Sample> = (0..total).map(|i| sample("q", i < correct, wall, 100)).collect();
        v[0].wall_s = first;
        report(v)
    }

    #[test]
    fn the_gate_wants_the_same_quality_and_more_speed() {
        let a = many(18, 20, 6.0, 45.0);
        // Faster and as good.
        assert!(compare(&a, &many(18, 20, 3.0, 20.0), Gate::default()).pass);
        // Faster but worse.
        let v = compare(&a, &many(14, 20, 3.0, 20.0), Gate::default());
        assert!(!v.pass && v.lines.iter().any(|l| l.starts_with("FAIL correct answers")));
        // As good but not faster.
        let v = compare(&a, &many(18, 20, 5.5, 44.0), Gate::default());
        assert!(!v.pass && v.lines.iter().any(|l| l.starts_with("FAIL speed")));
        // Only the first request is much faster: enough.
        assert!(compare(&a, &many(18, 20, 6.0, 20.0), Gate::default()).pass);
        // One point of quality is inside the slack.
        assert!(compare(&a, &many(17, 20, 3.0, 20.0), Gate::default()).pass);
    }

    #[test]
    fn empty_replies_and_failed_requests_fail_the_gate() {
        let a = many(18, 20, 6.0, 45.0);
        let mut b = many(18, 20, 3.0, 20.0);
        for s in b.samples.iter_mut().take(4) {
            s.empty_reply = true;
        }
        b.summary = Summary::from_samples(&b.samples);
        assert!(
            compare(&a, &b, Gate::default())
                .lines
                .iter()
                .any(|l| l.starts_with("FAIL empty replies"))
        );
        let mut c = many(18, 20, 3.0, 20.0);
        c.samples[3].error = Some("HTTP 500".into());
        c.summary = Summary::from_samples(&c.samples);
        assert!(!compare(&a, &c, Gate::default()).pass);
    }
}
