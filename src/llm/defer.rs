//! Deferred tools for LFM3 (`LLM_DEFER_TOOLS`, off by default).
//!
//! Home Assistant sends every tool with every request. With timers, media
//! controls and broadcasts, the tools are about 5,100 of 7,300 prompt tokens.
//! LFM3 can work with deferred tools: their definitions stay out of the
//! prompt, and a search tool returns them when the model needs one. The chat
//! template then writes the found definitions into the tool result, so the
//! cached start of the prompt stays the same. With the default patterns, the
//! prompt has about 2,100 tokens less, and a full prefill (after a change of
//! the prompt) took about 20 % less time. A request that needs a deferred tool
//! costs one model step more. docs/llm.md has the measured effect on the
//! answers: the model picks a wrong tool more often, so this stays off by
//! default.
//!
//! Home Assistant does not know the search tool. The proxy answers it: it
//! sends the search step and its result to llama-server and passes on only
//! the final reply. Home Assistant then sends the deferred tool call without
//! the search step, so the proxy keeps each search step under the ID of the
//! tool call that followed it and puts it back into later requests.
//!
//! The rewrite of each request, as the guide of Liquid AI describes it:
//!
//! 1. Every tool stays in `tools`, so llama-server knows the names. A tool that
//!    matches a pattern gets `defer_loading`, which llama.cpp honors, and its
//!    name goes into `chat_template_kwargs.deferred_tools`, which the patched
//!    template honors on both engines (`template.rs`).
//! 2. The tool `search_tools` is added.
//! 3. The names of the deferred tools leave the instruction lines of Home
//!    Assistant ("Follow these instructions for tools from ...").
//! 4. One paragraph at the end of the system message lists what the deferred
//!    tools do and tells the model to search for them.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde::Serialize;
use serde_json::{Value, json};

/// The patterns of `LLM_DEFER_TOOLS=on`: the timer intents, the media player
/// controls and the broadcast of Home Assistant. Home Assistant answers most
/// such sentences with its local intents before the LLM sees them.
pub const DEFAULT_PATTERNS: &[&str] = &[
    "intent__Hass*Timer*",
    "media_player__Hass*",
    "assist_satellite__HassBroadcast",
];

/// The name of the search tool that the proxy answers. Liquid AI recommends it.
pub const SEARCH_TOOL: &str = "search_tools";

/// The most search steps in one reply. After them, the reply goes on as it is.
pub const MAX_SEARCHES: usize = 2;

/// The most tools in one search result.
const MAX_RESULTS: usize = 5;

/// The search steps that the proxy keeps for later requests.
const KEPT_STEPS: usize = 64;

/// Words of a search query that match no tool on their own.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "this", "that", "tool", "tools", "please", "can", "you", "set", "make", "turn", "get",
    "all", "from", "into", "current", "some",
];

/// Spoken words and the word of the tool descriptions that they mean.
const SYNONYMS: &[(&str, &str)] = &[
    ("louder", "volume"),
    ("quieter", "volume"),
    ("softer", "volume"),
    ("loud", "volume"),
    ("quiet", "volume"),
    ("skip", "next"),
    ("continue", "resumes"),
    ("unpause", "resumes"),
    ("resume", "resumes"),
    ("stop", "pauses"),
    ("back", "previous"),
    ("last", "previous"),
    ("silence", "mutes"),
    ("alarm", "timer"),
    ("countdown", "timer"),
    ("announce", "broadcast"),
    ("announcement", "broadcast"),
    ("everyone", "broadcast"),
];

/// One tool call of a model reply.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A rewritten chat request.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The request as it goes to llama-server.
    pub body: Value,
    /// The definitions (`function` objects) of the deferred tools.
    deferred: Vec<Value>,
}

impl Prepared {
    pub fn bytes(&self) -> Option<Bytes> {
        serde_json::to_vec(&self.body).ok().map(Bytes::from)
    }

    /// The request with one more search step: the reply of the model with
    /// its search calls, and one tool result for each call.
    pub fn with_step(&mut self, step: &[Value]) {
        if let Some(messages) = self.body.get_mut("messages").and_then(Value::as_array_mut) {
            messages.extend_from_slice(step);
        }
    }

    /// The messages of one search step: the assistant message with the calls
    /// and the results that the proxy found.
    pub fn answer(&self, calls: &[ToolCall]) -> Vec<Value> {
        let tool_calls: Vec<Value> = calls
            .iter()
            .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
            .collect();
        let mut step = vec![json!({"role": "assistant", "content": "", "tool_calls": tool_calls})];
        for call in calls {
            let query = serde_json::from_str::<Value>(&call.arguments)
                .ok()
                .and_then(|v| v["query"].as_str().map(str::to_owned))
                .unwrap_or_default();
            let found = search(&query, &self.deferred);
            tracing::info!("search_tools \"{query}\" -> {} tools", found.len());
            step.push(json!({"role": "tool", "tool_call_id": call.id, "content": result_text(&found)}));
        }
        step
    }
}

/// `true` if the reply calls only the search tool.
pub fn only_searches(calls: &[ToolCall]) -> bool {
    !calls.is_empty() && calls.iter().all(|c| c.name == SEARCH_TOOL)
}

/// The rewrite of requests, with the search steps for later requests.
#[derive(Clone)]
pub struct Deferral {
    patterns: Arc<Vec<String>>,
    steps: Arc<Mutex<Steps>>,
}

#[derive(Default)]
struct Steps {
    by_call: HashMap<String, Vec<Value>>,
    order: VecDeque<String>,
}

impl Deferral {
    pub fn new(patterns: Vec<String>) -> Self {
        Self {
            patterns: Arc::new(patterns),
            steps: Arc::default(),
        }
    }

    /// Rewrites one chat request. `None` if it has no tool that matches a
    /// pattern, so it goes on unchanged.
    pub fn prepare(&self, body: &[u8]) -> Option<Prepared> {
        let mut v: Value = serde_json::from_slice(body).ok()?;
        let tools = v.get_mut("tools")?.as_array_mut()?;
        let mut names = Vec::new();
        let mut deferred = Vec::new();
        for tool in tools.iter_mut() {
            let Some(name) = tool["function"]["name"].as_str().map(str::to_owned) else {
                continue;
            };
            if name == SEARCH_TOOL || !self.patterns.iter().any(|p| glob(p, &name)) {
                continue;
            }
            tool["defer_loading"] = Value::Bool(true);
            deferred.push(tool["function"].clone());
            names.push(name);
        }
        if names.is_empty() {
            return None;
        }
        tools.push(search_tool());
        let kwargs = v
            .as_object_mut()?
            .entry("chat_template_kwargs")
            .or_insert_with(|| json!({}))
            .as_object_mut()?;
        kwargs.insert("deferred_tools".into(), json!(names));
        let messages = v.get_mut("messages")?.as_array_mut()?;
        if let Some(system) = messages.iter_mut().find(|m| m["role"] == "system")
            && let Some(text) = system["content"].as_str()
        {
            let text = without_names(text, &names);
            system["content"] = Value::String(format!("{text}\n\n{}", hint(&deferred)));
        }
        self.restore_steps(messages);
        Some(Prepared { body: v, deferred })
    }

    /// Keeps the search steps of one reply under the IDs of its tool calls.
    pub fn remember(&self, calls: &[ToolCall], step: &[Value]) {
        if calls.is_empty() || step.is_empty() {
            return;
        }
        let mut steps = self.steps.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for call in calls {
            if steps.by_call.insert(call.id.clone(), step.to_vec()).is_none() {
                steps.order.push_back(call.id.clone());
            }
        }
        while steps.order.len() > KEPT_STEPS {
            if let Some(old) = steps.order.pop_front() {
                steps.by_call.remove(&old);
            }
        }
    }

    /// Puts each kept search step back before the assistant message whose
    /// tool call followed it.
    fn restore_steps(&self, messages: &mut Vec<Value>) {
        let steps = self.steps.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if steps.by_call.is_empty() {
            return;
        }
        let mut i = 0;
        while i < messages.len() {
            let step = (messages[i]["role"] == "assistant")
                .then(|| messages[i]["tool_calls"].as_array())
                .flatten()
                .and_then(|calls| {
                    calls
                        .iter()
                        .find_map(|c| c["id"].as_str().and_then(|id| steps.by_call.get(id)))
                })
                .cloned();
            match step {
                Some(step) => {
                    let n = step.len();
                    messages.splice(i..i, step);
                    i += n + 1;
                }
                None => i += 1,
            }
        }
    }
}

fn search_tool() -> Value {
    json!({"type": "function", "function": {
        "name": SEARCH_TOOL,
        "description": "Search the catalog of more tools by what they do. Returns the tools that match.",
        "parameters": {"type": "object", "properties": {
            "query": {"type": "string", "description": "What the tool must do, in a few words"}
        }, "required": ["query"]}
    }})
}

/// The paragraph at the end of the system message. It names what the
/// deferred tools do, not their names, so the model searches before it calls.
fn hint(deferred: &[Value]) -> String {
    let mut seen = Vec::new();
    for f in deferred {
        let d = f["description"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .trim_end_matches('.');
        if !d.is_empty() && !seen.iter().any(|s: &String| s.eq_ignore_ascii_case(d)) {
            seen.push(d.to_lowercase());
        }
    }
    format!(
        "More tools are in a catalog. They do this: {}. If none of your listed tools can handle a request, \
         call {SEARCH_TOOL} to find one, then call the tool that it returns.",
        seen.join("; ")
    )
}

/// Removes the deferred names from the lists in Home Assistant's lines
/// "Follow these instructions for tools from "x" (a, b, c):".
fn without_names(text: &str, names: &[String]) -> String {
    const MARK: &str = "Follow these instructions for tools from ";
    text.split_inclusive('\n')
        .map(|line| {
            let Some(rest) = line.strip_prefix(MARK) else {
                return line.to_owned();
            };
            let (Some(open), Some(close)) = (rest.find(" ("), rest.rfind("):")) else {
                return line.to_owned();
            };
            if close <= open {
                return line.to_owned();
            }
            let kept: Vec<&str> = rest[open + 2..close]
                .split(", ")
                .filter(|n| !names.iter().any(|d| d == n.trim()))
                .collect();
            format!("{MARK}{} ({}{}", &rest[..open], kept.join(", "), &rest[close..])
        })
        .collect()
}

/// `*` matches any text, everything else matches itself.
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

/// The words of a tool name and description, lower case. "HassMediaPause"
/// gives "hass", "media", "pause".
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && prev_lower && !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
            word.push(ch.to_ascii_lowercase());
        } else {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            prev_lower = false;
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// The first five letters, so that "pause" finds "pauses" and "timer" "timers".
fn stem(word: &str) -> &str {
    let end = word.char_indices().nth(5).map_or(word.len(), |(i, _)| i);
    &word[..end]
}

/// The deferred tools that match the query, the best first.
fn search<'a>(query: &str, deferred: &'a [Value]) -> Vec<&'a Value> {
    let mut stems: Vec<String> = Vec::new();
    for w in words(query) {
        if w.len() < 3 || STOP_WORDS.contains(&w.as_str()) {
            continue;
        }
        let w = SYNONYMS.iter().find(|(k, _)| *k == w).map_or(w.as_str(), |(_, v)| v);
        let s = stem(w).to_owned();
        if !stems.contains(&s) {
            stems.push(s);
        }
    }
    let mut scored: Vec<(usize, usize, &Value)> = deferred
        .iter()
        .enumerate()
        .filter_map(|(i, f)| {
            let text = format!(
                "{} {}",
                f["name"].as_str().unwrap_or_default(),
                f["description"].as_str().unwrap_or_default()
            );
            let tool: Vec<String> = words(&text).iter().map(|w| stem(w).to_owned()).collect();
            let score = stems.iter().filter(|s| tool.contains(s)).count();
            (score > 0).then_some((score, i, f))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(MAX_RESULTS).map(|(_, _, f)| f).collect()
}

/// The tool result of a search. The definitions are written the way the
/// template writes tools: name, description, parameters, with ", " and ": ".
fn result_text(found: &[&Value]) -> String {
    if found.is_empty() {
        return "No tool matches. Search again with other words.".to_owned();
    }
    let defs: Vec<String> = found
        .iter()
        .map(|f| {
            let mut out = String::from("{");
            let mut first = true;
            for key in ["name", "description", "parameters"] {
                if let Some(v) = f.get(key) {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    out.push_str(&spaced(&Value::String(key.to_owned())));
                    out.push_str(": ");
                    out.push_str(&spaced(v));
                }
            }
            out.push('}');
            out
        })
        .collect();
    let noun = if found.len() == 1 { "tool" } else { "tools" };
    format!(
        "Found {} {noun}.\n<additional_tools>\n{}\n</additional_tools>",
        found.len(),
        defs.join("\n")
    )
}

/// JSON with ", " between items and ": " after keys, like `tojson` of the template.
fn spaced(v: &Value) -> String {
    struct Spaced;
    impl serde_json::ser::Formatter for Spaced {
        fn begin_array_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W, first: bool) -> std::io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_key<W: ?Sized + std::io::Write>(&mut self, w: &mut W, first: bool) -> std::io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
            w.write_all(b": ")
        }
    }
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, Spaced);
    if v.serialize(&mut ser).is_err() {
        return v.to_string();
    }
    String::from_utf8(buf).unwrap_or_else(|_| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str) -> Value {
        json!({"type": "function", "function": {"name": name, "description": description,
            "parameters": {"type": "object", "properties": {"area": {"type": "string"}}}}})
    }

    /// A request in the form of Home Assistant, with two deferred tools.
    fn request() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "messages": [
                {"role": "system", "content": "Intro.\nFollow these instructions for tools from \"assist\" (intent__HassTurnOn, intent__HassStartTimer, media_player__HassMediaPause, todo__get_items):\nYou ARE equipped."},
                {"role": "user", "content": "pause the music"}
            ],
            "tools": [
                tool("intent__HassTurnOn", "Turns on a device"),
                tool("intent__HassStartTimer", "Starts a new timer"),
                tool("media_player__HassMediaPause", "Pauses a media player"),
                tool("todo__get_items", "Gets the items of a list")
            ],
            "stream": true
        }))
        .unwrap()
    }

    fn deferral() -> Deferral {
        Deferral::new(DEFAULT_PATTERNS.iter().map(|p| (*p).to_owned()).collect())
    }

    #[test]
    fn patterns_match_whole_names() {
        assert!(glob("intent__Hass*Timer*", "intent__HassStartTimer"));
        assert!(glob("intent__Hass*Timer*", "intent__HassCancelAllTimers"));
        assert!(glob("intent__Hass*Timer*", "intent__HassTimerStatus"));
        assert!(!glob("intent__Hass*Timer*", "intent__HassTurnOn"));
        assert!(glob("media_player__Hass*", "media_player__HassSetVolume"));
        assert!(!glob("media_player__Hass*", "media_player__play_media"));
        assert!(glob(
            "assist_satellite__HassBroadcast",
            "assist_satellite__HassBroadcast"
        ));
        assert!(!glob(
            "assist_satellite__HassBroadcast",
            "assist_satellite__HassBroadcastX"
        ));
        assert!(glob("*", "anything"));
    }

    #[test]
    fn the_request_keeps_every_tool_and_hides_the_deferred_ones() {
        let p = deferral().prepare(&request()).expect("two tools match");
        let tools = p.body["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "intent__HassTurnOn",
                "intent__HassStartTimer",
                "media_player__HassMediaPause",
                "todo__get_items",
                SEARCH_TOOL
            ]
        );
        let deferred: Vec<bool> = tools.iter().map(|t| t["defer_loading"] == true).collect();
        assert_eq!(deferred, [false, true, true, false, false]);
        assert_eq!(
            p.body["chat_template_kwargs"]["deferred_tools"],
            json!(["intent__HassStartTimer", "media_player__HassMediaPause"])
        );
        let system = p.body["messages"][0]["content"].as_str().unwrap();
        assert!(
            system.contains("tools from \"assist\" (intent__HassTurnOn, todo__get_items):\nYou ARE equipped."),
            "{system}"
        );
        assert!(
            system.ends_with(
                "More tools are in a catalog. They do this: starts a new timer; pauses a media player. If none of \
                 your listed tools can handle a request, call search_tools to find one, then call the tool that it returns."
            ),
            "{system}"
        );
        assert_eq!(p.body["stream"], true);
    }

    #[test]
    fn a_request_without_matching_tools_stays_as_sent() {
        let body = serde_json::to_vec(&json!({"messages": [{"role": "user", "content": "hi"}],
            "tools": [tool("intent__HassTurnOn", "Turns on a device")]}))
        .unwrap();
        assert!(deferral().prepare(&body).is_none());
        assert!(deferral().prepare(br#"{"messages": []}"#).is_none());
        assert!(deferral().prepare(b"not json").is_none());
    }

    #[test]
    fn a_search_finds_tools_by_spoken_words() {
        let defs: Vec<Value> = [
            ("media_player__HassMediaPause", "Pauses a media player"),
            ("media_player__HassMediaUnpause", "Resumes a media player"),
            ("media_player__HassMediaNext", "Skips a media player to the next item"),
            (
                "media_player__HassSetVolumeRelative",
                "Increases or decreases the volume of a media player",
            ),
            ("intent__HassStartTimer", "Starts a new timer"),
            (
                "assist_satellite__HassBroadcast",
                "Broadcast a message through the home",
            ),
        ]
        .iter()
        .map(|(n, d)| tool(n, d)["function"].clone())
        .collect();
        let names = |q: &str| -> Vec<String> {
            search(q, &defs)
                .iter()
                .map(|f| f["name"].as_str().unwrap().split("__").nth(1).unwrap().to_owned())
                .collect()
        };
        assert_eq!(names("pause music")[0], "HassMediaPause");
        assert_eq!(names("skip this song")[0], "HassMediaNext");
        assert_eq!(names("louder")[0], "HassSetVolumeRelative");
        assert_eq!(names("set a timer"), ["HassStartTimer"]);
        assert_eq!(names("announce dinner to everyone")[0], "HassBroadcast");
        assert_eq!(names("resume playback")[0], "HassMediaUnpause");
        assert!(names("weather forecast").is_empty());
        assert!(names("").is_empty());
    }

    #[test]
    fn the_result_lists_the_definitions_as_the_template_writes_tools() {
        let f = json!({"description": "Starts a new timer", "name": "intent__HassStartTimer",
            "parameters": {"properties": {"minutes": {"minimum": 0, "type": "integer"}}, "type": "object"}});
        assert_eq!(
            result_text(&[&f]),
            "Found 1 tool.\n<additional_tools>\n{\"name\": \"intent__HassStartTimer\", \"description\": \"Starts a new \
             timer\", \"parameters\": {\"properties\": {\"minutes\": {\"minimum\": 0, \"type\": \"integer\"}}, \"type\": \
             \"object\"}}\n</additional_tools>"
        );
        assert_eq!(result_text(&[]), "No tool matches. Search again with other words.");
    }

    #[test]
    fn a_search_step_comes_back_before_the_call_that_followed_it() {
        let d = deferral();
        let p = d.prepare(&request()).unwrap();
        let search = ToolCall {
            id: "s1".into(),
            name: SEARCH_TOOL.into(),
            arguments: r#"{"query":"pause music"}"#.into(),
        };
        assert!(only_searches(std::slice::from_ref(&search)));
        let step = p.answer(std::slice::from_ref(&search));
        assert_eq!(step.len(), 2);
        assert_eq!(step[0]["tool_calls"][0]["function"]["name"], SEARCH_TOOL);
        assert_eq!(step[1]["tool_call_id"], "s1");
        let text = step[1]["content"].as_str().unwrap();
        assert!(
            text.starts_with("Found 1 tool.\n<additional_tools>\n{\"name\": \"media_player__HassMediaPause\""),
            "{text}"
        );

        let pause = ToolCall {
            id: "c2".into(),
            name: "media_player__HassMediaPause".into(),
            arguments: "{}".into(),
        };
        assert!(!only_searches(&[search, pause.clone()]));
        d.remember(std::slice::from_ref(&pause), &step);

        // The next request of Home Assistant has the call and its result, but no search step.
        let mut next: Value = serde_json::from_slice(&request()).unwrap();
        let messages = next["messages"].as_array_mut().unwrap();
        messages.push(json!({"role": "assistant", "content": "", "tool_calls": [
            {"id": "c2", "type": "function", "function": {"name": "media_player__HassMediaPause", "arguments": "{}"}}]}));
        messages.push(json!({"role": "tool", "tool_call_id": "c2", "content": "{\"success\": true}"}));
        let p = d.prepare(&serde_json::to_vec(&next).unwrap()).unwrap();
        let roles: Vec<&str> = p.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "assistant", "tool"]);
        assert_eq!(p.body["messages"][3]["tool_call_id"], "s1");
        assert_eq!(p.body["messages"][4]["tool_calls"][0]["id"], "c2");
    }

    #[test]
    fn only_the_newest_search_steps_are_kept() {
        let d = deferral();
        let step = [json!({"role": "tool"})];
        for i in 0..KEPT_STEPS + 10 {
            let call = ToolCall {
                id: format!("c{i}"),
                ..ToolCall::default()
            };
            d.remember(&[call], &step);
        }
        let steps = d.steps.lock().unwrap();
        assert_eq!(steps.by_call.len(), KEPT_STEPS);
        assert!(!steps.by_call.contains_key("c0"));
        assert!(steps.by_call.contains_key(&format!("c{}", KEPT_STEPS + 9)));
    }
}
