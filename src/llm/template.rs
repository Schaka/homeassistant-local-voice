//! Fixes for the chat templates inside the model files.
//!
//! Home Assistant's llama.cpp integration sends the text and the tool calls of
//! earlier assistant messages back to the model, but not their reasoning. The
//! templates of Ling 3.0, LFM3 and MiniCPM5 open a new thinking block after
//! every tool result, so the model thinks again from scratch before it reports
//! the result. Ling then often writes its answer into the open block, and the
//! reply comes back empty.
//!
//! The fix is one line per template: after a tool result, the answer starts
//! with a closed thinking block. The model still thinks after a user message,
//! where it decides which tool to call. docs/llm.md has the measured effect
//! on the time per question.
//!
//! The server reads the template from the GGUF file at each start and applies
//! the patch there, so a new upstream template is used as soon as it ships. If
//! the patched line is not found, the original template is used and a warning
//! names the model.

use crate::config::Family;

/// The line of the generation prompt that opens the thinking block, and its
/// replacement with a closed block after a tool result.
fn patch_for(family: Family) -> Option<(&'static str, &'static str)> {
    Some(match family {
        Family::Ling3 => (
            "{%- if thinking_option == 'on' %}",
            "{%- if thinking_option == 'on' and messages[-1].role == 'tool' %}\n        \
             {{- '\\n<think></think>' }}\n    {%- elif thinking_option == 'on' %}",
        ),
        Family::Lfm3 => (
            "{%- if enable_thinking -%}\n        {{- \"<think>\" -}}",
            "{%- if enable_thinking and messages[-1].role == \"tool\" -%}\n        {{- \"<think></think>\" -}}\n    \
             {%- elif enable_thinking -%}\n        {{- \"<think>\" -}}",
        ),
        Family::Minicpm5 => (
            "{%- elif enable_thinking is true %}",
            "{%- elif enable_thinking is true and messages[-1].role == 'tool' %}\n            \
             {{- '<think>\\n\\n</think>\\n\\n' }}\n        {%- elif enable_thinking is true %}",
        ),
        Family::Qwen35 | Family::Granite4 => return None,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The family needs no patch.
    NotNeeded,
    /// The patched template.
    Patched(String),
    /// The line to patch occurs `n` times instead of once: the template changed upstream.
    NoMatch(usize),
}

/// Applies the patch of `family` to `template`.
pub fn patch(family: Family, template: &str) -> Outcome {
    let Some((find, replace)) = patch_for(family) else {
        return Outcome::NotNeeded;
    };
    match template.matches(find).count() {
        1 => Outcome::Patched(template.replacen(find, replace, 1)),
        n => Outcome::NoMatch(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The generation prompts of the real templates, as they are in the GGUF files.
    const LING: &str = "{%- if add_generation_prompt %}\n    {{- '<role>ASSISTANT</role>' }}\n    {%- if thinking_option == 'on' %}\n        {{- '\\n<think>' }}\n    {%- elif thinking_option == 'off' %}\n        {{- '\\n<think></think>' }}\n    {%- endif %}\n{%- endif %}";
    const LFM3: &str = "{%- if add_generation_prompt -%}\n    {{- \"<|im_start|>assistant\\n\" -}}\n    {%- if enable_thinking -%}\n        {{- \"<think>\" -}}\n    {%- else -%}\n        {{- \"<think></think>\" -}}\n    {%- endif -%}\n{%- endif -%}";
    const MINICPM5: &str = "{%- if add_generation_prompt %}\n    {{- '<|im_start|>assistant\\n' }}\n    {%- if enable_thinking is defined %}\n        {%- if enable_thinking is false %}\n            {{- '<think>\\n\\n</think>\\n\\n' }}\n        {%- elif enable_thinking is true %}\n            {{- '<think>\\n' }}\n        {%- endif %}\n    {%- endif %}\n{%- endif %}";

    fn patched(family: Family, t: &str) -> String {
        match patch(family, t) {
            Outcome::Patched(p) => p,
            other => panic!("{family:?}: {other:?}"),
        }
    }

    #[test]
    fn each_family_gets_a_tool_branch_before_the_thinking_branch() {
        for (family, t) in [
            (Family::Ling3, LING),
            (Family::Lfm3, LFM3),
            (Family::Minicpm5, MINICPM5),
        ] {
            let p = patched(family, t);
            let tool = p.find("messages[-1].role").expect("tool branch");
            let think_open = p.rfind("<think>").expect("thinking branch");
            assert!(tool < think_open, "{family:?}: the tool branch must come first:\n{p}");
            assert_eq!(p.matches("role == ").count(), 1, "{family:?}");
        }
    }

    #[test]
    fn unknown_or_changed_templates_stay_as_they_are() {
        assert_eq!(patch(Family::Granite4, LING), Outcome::NotNeeded);
        assert_eq!(patch(Family::Lfm3, LING), Outcome::NoMatch(0));
        let twice = format!("{MINICPM5}{MINICPM5}");
        assert_eq!(patch(Family::Minicpm5, &twice), Outcome::NoMatch(2));
    }

    /// Patches the template of a real model file and prints the generation part:
    /// `LLM_TEST_MODEL=/path/model.gguf LLM_TEST_FAMILY=lfm3 cargo test real_template -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a GGUF model file"]
    fn real_template() {
        let path = std::env::var("LLM_TEST_MODEL").unwrap();
        let family = Family::parse(&std::env::var("LLM_TEST_FAMILY").unwrap()).unwrap();
        let t = crate::gguf::metadata_string(std::path::Path::new(&path), "tokenizer.chat_template")
            .unwrap()
            .expect("template in the GGUF");
        let p = patched(family, &t);
        let start = p.rfind("add_generation_prompt").unwrap();
        println!("{}", &p[start..]);
    }
}
