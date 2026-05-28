//! Minimal OpenAI-compatible chat client for AI workflows.
//!
//! POSTs `{base_url}/chat/completions` with a system + user message and returns
//! the assistant text. Works against local ollama or any OpenAI-compatible cloud
//! endpoint; the only difference is `base_url`/`model`/`api_key` in config.

use std::fmt;
use std::time::Duration;

use crate::config::Llm;

#[derive(Debug)]
pub enum LlmError {
    /// No model configured, so workflows are effectively disabled.
    Disabled,
    /// Transport failure or non-2xx HTTP status.
    Http(String),
    /// Response body could not be parsed or had no choices.
    Parse(String),
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::Disabled => write!(f, "LLM disabled (no model configured)"),
            LlmError::Http(msg) => write!(f, "LLM HTTP error: {msg}"),
            LlmError::Parse(msg) => write!(f, "failed to parse LLM response: {msg}"),
        }
    }
}

impl std::error::Error for LlmError {}

/// Transform `user_text` using `system_prompt` via the configured chat endpoint.
/// Returns the assistant message text, trimmed.
pub fn transform(cfg: &Llm, system_prompt: &str, user_text: &str) -> Result<String, LlmError> {
    if cfg.model.trim().is_empty() {
        return Err(LlmError::Disabled);
    }

    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(cfg.timeout_secs.max(1)))
        .build();

    let body = serde_json::json!({
        "model": cfg.model,
        "temperature": 0.2,
        "stream": false,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_text}
        ]
    })
    .to_string();

    let mut req = agent.post(&url).set("Content-Type", "application/json");
    if !cfg.api_key.trim().is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", cfg.api_key));
    }

    let resp = match req.send_string(&body) {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => {
            return Err(LlmError::Http(format!(
                "status {code}: {}",
                resp.into_string().unwrap_or_default()
            )));
        }
        Err(ureq::Error::Transport(t)) => {
            return Err(LlmError::Http(t.to_string()));
        }
    };

    let body = resp
        .into_string()
        .map_err(|e| LlmError::Http(e.to_string()))?;

    #[derive(serde::Deserialize)]
    struct ChatResp {
        #[serde(default)]
        choices: Vec<Choice>,
    }
    #[derive(serde::Deserialize)]
    struct Choice {
        message: ChatMsg,
    }
    #[derive(serde::Deserialize)]
    struct ChatMsg {
        #[serde(default)]
        content: String,
    }

    let parsed =
        serde_json::from_str::<ChatResp>(&body).map_err(|e| LlmError::Parse(e.to_string()))?;

    let content = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::Parse("no choices in response".to_string()))?
        .message
        .content;

    Ok(content.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_model_empty() {
        let cfg = Llm {
            model: String::new(),
            ..Default::default()
        };
        let result = transform(&cfg, "sys", "txt");
        assert!(matches!(result, Err(LlmError::Disabled)));
    }

    #[test]
    fn disabled_when_model_whitespace() {
        let cfg = Llm {
            model: "   ".to_string(),
            ..Default::default()
        };
        assert!(matches!(
            transform(&cfg, "sys", "txt"),
            Err(LlmError::Disabled)
        ));
    }

    #[test]
    fn display_strings_are_sensible() {
        assert!(LlmError::Disabled.to_string().contains("disabled"));

        let http = LlmError::Http("boom".to_string()).to_string();
        assert!(http.contains("HTTP"));
        assert!(http.contains("boom"));

        let parse = LlmError::Parse("bad json".to_string()).to_string();
        assert!(parse.contains("parse"));
        assert!(parse.contains("bad json"));
    }
}
