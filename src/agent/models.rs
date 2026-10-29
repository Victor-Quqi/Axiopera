//! Native model pickers, queried without starting an inference turn.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{BufReader, Lines};
use tokio::process::ChildStdout;

use super::Proc;
use crate::ipc::Agent;

#[derive(Debug, Default, Serialize)]
pub struct Catalog {
    pub models: Vec<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Model {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

impl Catalog {
    pub fn render(&self, agent: Agent) -> String {
        let mut lines = Vec::new();
        let default_label = match agent {
            Agent::Claude => "account default",
            _ => "backend default",
        };
        if let Some(model) = &self.configured_model {
            lines.push(format!("Configured model: {}", single_line(model)));
        }
        if self.models.is_empty() {
            lines.push("No models returned.".into());
        }
        for (label, model) in [
            (default_label, &self.default_model),
            ("current", &self.current_model),
        ] {
            if let Some(model) = model
                && !self.models.iter().any(|m| &m.id == model)
            {
                lines.push(format!("{}  ({label})", single_line(model)));
            }
        }
        for model in &self.models {
            let mut row = single_line(&model.id);
            if let Some(resolved) = &model.resolved_model
                && !resolved.is_empty()
                && resolved != &model.id
            {
                row.push_str(&format!(" → {}", single_line(resolved)));
            }
            if let Some(name) = &model.name
                && name_key(name) != name_key(&model.id)
                && !(agent == Agent::Claude && model.id == "default")
            {
                row.push_str(&format!("  {}", single_line(name)));
            }
            if self.default_model.as_deref() == Some(&model.id) {
                row.push_str(&format!("  ({default_label})"));
            }
            if self.current_model.as_deref() == Some(&model.id) {
                row.push_str("  (current)");
            }
            if model.disabled == Some(true) {
                row.push_str("  (disabled)");
            }
            if let Some(efforts) = &model.efforts
                && !efforts.is_empty()
            {
                row.push_str(&format!(
                    "  effort: {}",
                    efforts
                        .iter()
                        .map(|e| single_line(e))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            lines.push(row);
        }
        lines.join("\n")
    }
}

fn name_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn single_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

pub fn command(agent: Agent) -> String {
    format!("axiopera models {}", json!(agent).as_str().unwrap())
}

/// Match backend parameter errors, excluding auth, capacity and transport failures.
pub fn selection_error(error: &str) -> bool {
    let text = error.to_ascii_lowercase();
    if [
        "401",
        "403",
        "429",
        "unauthorized",
        "authentication",
        "api key",
        "api_key",
        "permission",
        "access",
        "quota",
        "rate limit",
        "credit",
        "billing",
        "timeout",
        "timed out",
        "connection",
        "overload",
        "capacity",
        "auto_review",
        "auto-review",
        "approval review",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        return false;
    }
    if [
        "model_not_found",
        "unknown model",
        "invalid model",
        "unsupported model",
        "unrecognized model",
        "no such model",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        return true;
    }
    let parameter =
        text.contains("model") || text.contains("effort") || text.contains("reasoning_effort");
    parameter
        && [
            "not found",
            "does not exist",
            "not supported",
            "unsupported",
            "invalid value",
            "invalid variant",
            "unknown variant",
            "not a valid",
            "not a recognized",
            "must be one of",
        ]
        .iter()
        .any(|s| text.contains(s))
}

pub async fn query(
    agent: Agent,
    exe: &Path,
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<Catalog> {
    let args: Vec<String> = match agent {
        Agent::Codex => vec!["app-server"],
        Agent::Grok => vec!["agent", "--no-leader", "stdio"],
        Agent::Claude => vec![
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--settings",
            "{\"disableAllHooks\":true}",
        ],
    }
    .into_iter()
    .map(str::to_owned)
    .collect();
    let (proc, stdout) = Proc::spawn_with(exe, &args, cwd, env, None)?;
    query_process(agent, proc, stdout, timeout).await
}

async fn query_process(
    agent: Agent,
    mut proc: Proc,
    stdout: ChildStdout,
    timeout: Duration,
) -> Result<Catalog> {
    let mut lines = super::lines(stdout);
    let result = tokio::time::timeout(timeout, discover(agent, &proc, &mut lines)).await;
    // Kill the whole job on success and error, including any startup helper processes.
    proc.job.kill();
    let _ = proc.child.wait().await;
    result.context("model query timed out")?
}

async fn discover(
    agent: Agent,
    proc: &Proc,
    lines: &mut Lines<BufReader<ChildStdout>>,
) -> Result<Catalog> {
    let mut catalog = Catalog::default();
    match agent {
        Agent::Claude => {
            proc.stdin.send(&json!({"type":"control_request","request_id":"models","request":{"subtype":"initialize","hooks":{}}})).await?;
            while let Some(line) = lines.next_line().await? {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if msg["type"] == "control_request" {
                    proc.stdin.send(&json!({"type":"control_response","response":{"subtype":"error","request_id":msg["request_id"],"error":"model query only"}})).await?;
                }
                if msg["type"] != "control_response" || msg["response"]["request_id"] != "models" {
                    continue;
                }
                let response = &msg["response"];
                if response["subtype"] != "success" {
                    bail!("model query initialize failed")
                }
                let payload = &response["response"];
                catalog.models = parse_models(agent, &payload["models"])?;
                if catalog.models.iter().any(|m| m.id == "default") {
                    catalog.default_model = Some("default".into());
                }
                if let Some(unavailable) = payload.get("unavailable_models") {
                    for mut model in parse_models(agent, unavailable)? {
                        if catalog.models.iter().any(|m| m.id == model.id) {
                            continue;
                        }
                        model.disabled = Some(true);
                        catalog.models.push(model);
                    }
                }
                return Ok(catalog);
            }
            bail!("agent exited before returning models")
        }
        Agent::Codex => {
            rpc(
                proc,
                lines,
                1,
                "initialize",
                json!({"clientInfo":{"name":"axiopera","version":env!("CARGO_PKG_VERSION")}}),
            )
            .await?;
            proc.stdin
                .send(&json!({"jsonrpc":"2.0","method":"initialized","params":{}}))
                .await?;
            let mut cursor = Value::Null;
            let mut seen = HashSet::new();
            let mut id = 2;
            loop {
                let page = rpc(proc, lines, id, "model/list", json!({"cursor":cursor})).await?;
                let models = parse_models(agent, &page["data"])?;
                for (row, model) in page["data"].as_array().unwrap().iter().zip(&models) {
                    if row["isDefault"] == true {
                        catalog.default_model = Some(model.id.clone());
                    }
                }
                catalog.models.extend(models);
                cursor = page["nextCursor"].clone();
                let Some(next) = cursor.as_str().filter(|s| !s.is_empty()) else {
                    break;
                };
                if !seen.insert(next.to_owned()) {
                    bail!("model/list repeated its pagination cursor")
                }
                id += 1;
            }
        }
        Agent::Grok => {
            rpc(proc, lines, 1, "initialize", json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"axiopera","version":env!("CARGO_PKG_VERSION")}})).await?;
            let response = rpc(proc, lines, 2, "_x.ai/models/list", json!({})).await?;
            if !response["error"].is_null() {
                bail!("models/list: {}", response["error"])
            }
            let payload = response.get("result").unwrap_or(&response);
            catalog.models = parse_models(agent, &payload["availableModels"])?;
            catalog.current_model = string(payload, "currentModelId");
        }
    }
    Ok(catalog)
}

async fn rpc(
    proc: &Proc,
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    proc.stdin
        .send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
        .await?;
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if msg.get("method").is_some() {
            if let Some(id) = msg.get("id") {
                proc.stdin.send(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"model query only"}})).await?;
            }
            continue;
        }
        if msg["id"] != id {
            continue;
        }
        if let Some(error) = msg.get("error") {
            if error["code"] == -32601 {
                bail!("{method} is not supported by this agent version")
            }
            bail!(
                "{method}: {}",
                error["message"].as_str().unwrap_or("query failed")
            );
        }
        return msg
            .get("result")
            .cloned()
            .context("missing model query result");
    }
    bail!("agent exited during {method}")
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn parse_models(agent: Agent, value: &Value) -> Result<Vec<Model>> {
    let rows = value
        .as_array()
        .context("agent did not return a model list")?;
    rows.iter()
        .map(|row| {
            let (id_key, name_key, efforts) = match agent {
                Agent::Claude => ("value", "displayName", row.get("supportedEffortLevels")),
                Agent::Codex => ("model", "displayName", row.get("supportedReasoningEfforts")),
                Agent::Grok => ("modelId", "name", row["_meta"].get("reasoningEfforts")),
            };
            let id = string(row, id_key)
                .filter(|s| !s.is_empty())
                .context("model entry has no selectable ID")?;
            Ok(Model {
                id,
                name: string(row, name_key),
                description: string(row, "description"),
                efforts: efforts.and_then(Value::as_array).map(|values| {
                    values
                        .iter()
                        .filter_map(|v| {
                            v.as_str()
                                .or_else(|| v["reasoningEffort"].as_str())
                                .or_else(|| v["value"].as_str())
                                .map(str::to_owned)
                        })
                        .collect()
                }),
                resolved_model: string(row, "resolvedModel"),
                disabled: row.get("disabled").and_then(Value::as_bool),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture(agent: Agent, mode: &str, timeout: Duration) -> Result<Catalog> {
        let mut env: Vec<_> = std::env::vars().collect();
        env.push(("AXIOPERA_MODEL_FIXTURE".into(), mode.into()));
        let args = [
            "--exact",
            "agent::models::tests::protocol_fixture",
            "--ignored",
            "--nocapture",
        ]
        .map(str::to_owned);
        let (proc, stdout) = Proc::spawn_with(
            &std::env::current_exe()?,
            &args,
            &std::env::current_dir()?,
            &env,
            None,
        )?;
        query_process(agent, proc, stdout, timeout).await
    }

    #[test]
    #[ignore = "subprocess fixture for model protocol tests"]
    fn protocol_fixture() {
        use std::io::{BufRead, Write};
        let Ok(mode) = std::env::var("AXIOPERA_MODEL_FIXTURE") else {
            return;
        };
        let send = |value: Value| {
            println!("{value}");
            std::io::stdout().flush().unwrap();
        };
        for line in std::io::stdin().lock().lines() {
            let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if mode == "hang" {
                std::thread::sleep(Duration::from_secs(30));
            }
            if msg["type"] == "control_request" {
                send(
                    json!({"type":"control_response","response":{"request_id":"other","subtype":"success","response":{"models":[]}}}),
                );
                send(
                    json!({"type":"control_response","response":{"request_id":msg["request_id"],"subtype":"success","response":{"models":[{"value":"default","displayName":"Default"}],"unavailable_models":[{"value":"restricted","description":"Unavailable for this account"}]}}}),
                );
                continue;
            }
            let result = match msg["method"].as_str() {
                Some("initialize") => json!({}),
                Some("model/list") => {
                    if msg["params"]["cursor"].is_null() {
                        send(
                            json!({"jsonrpc":"2.0","id":"callback","method":"test/request","params":{}}),
                        );
                        json!({"data":[{"model":"first","displayName":"First"}],"nextCursor":"page2"})
                    } else {
                        json!({"data":[{"model":"second","displayName":"Second","isDefault":true}],"nextCursor":if mode == "repeat" { json!("page2") } else { Value::Null }})
                    }
                }
                Some("_x.ai/models/list") => {
                    if mode == "unsupported" {
                        send(
                            json!({"jsonrpc":"2.0","id":msg["id"],"error":{"code":-32601,"message":"method not found"}}),
                        );
                        continue;
                    }
                    if mode == "error" {
                        json!({"error":"catalog unavailable"})
                    } else {
                        json!({"result":{"currentModelId":"company-claude","availableModels":[{"modelId":"company-claude","name":"Claude"}]}})
                    }
                }
                _ => continue,
            };
            send(json!({"jsonrpc":"2.0","id":msg["id"],"result":result}));
        }
    }

    #[tokio::test]
    async fn protocols_preserve_pages_defaults_and_disabled_rows() {
        let timeout = Duration::from_secs(10);
        let codex = fixture(Agent::Codex, "ok", timeout).await.unwrap();
        assert_eq!(
            codex
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(codex.default_model.as_deref(), Some("second"));
        assert!(
            codex
                .render(Agent::Codex)
                .contains("second  (backend default)")
        );
        let grok = fixture(Agent::Grok, "ok", timeout).await.unwrap();
        assert_eq!(grok.current_model.as_deref(), Some("company-claude"));
        assert!(grok.default_model.is_none());
        assert_eq!(
            grok.render(Agent::Grok),
            "company-claude  Claude  (current)"
        );
        let json = serde_json::to_value(&grok).unwrap();
        assert_eq!(json["current_model"], "company-claude");
        assert!(json.get("default_model").is_none());
        let claude = fixture(Agent::Claude, "ok", timeout).await.unwrap();
        assert_eq!(claude.default_model.as_deref(), Some("default"));
        assert_eq!(
            claude.render(Agent::Claude),
            "default  (account default)\nrestricted  (disabled)"
        );
        assert_eq!(claude.models[1].disabled, Some(true));
        assert!(
            fixture(Agent::Codex, "repeat", timeout)
                .await
                .unwrap_err()
                .to_string()
                .contains("cursor")
        );
        assert!(
            fixture(Agent::Grok, "error", timeout)
                .await
                .unwrap_err()
                .to_string()
                .contains("catalog unavailable")
        );
        assert!(
            fixture(Agent::Grok, "unsupported", timeout)
                .await
                .unwrap_err()
                .to_string()
                .contains("not supported")
        );
    }

    #[tokio::test]
    async fn timeout_kills_and_reaps_the_query_process() {
        let error = fixture(Agent::Codex, "hang", Duration::from_millis(150))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn native_ids_and_efforts_survive_normalization() {
        let rows = parse_models(Agent::Grok, &json!([{"modelId":"team-coding","name":"Claude","_meta":{"reasoningEfforts":[{"value":"high","name":"High"}]}}])).unwrap();
        assert_eq!(rows[0].id, "team-coding");
        assert_eq!(rows[0].efforts.as_ref().unwrap(), &["high"]);
        let rows = parse_models(Agent::Codex, &json!([{"id":"picker-id","model":"wire-id","supportedReasoningEfforts":[{"reasoningEffort":"xhigh"}]}])).unwrap();
        assert_eq!(rows[0].id, "wire-id");
        assert_eq!(rows[0].efforts.as_ref().unwrap(), &["xhigh"]);
        let rows = parse_models(Agent::Claude, &json!([{"value":"sonnet","resolvedModel":"claude-sonnet","description":"Daily work","disabled":true}])).unwrap();
        assert_eq!(rows[0].id, "sonnet");
        assert!(rows[0].efforts.is_none());
        assert_eq!(rows[0].resolved_model.as_deref(), Some("claude-sonnet"));
        assert_eq!(rows[0].disabled, Some(true));
        assert!(parse_models(Agent::Grok, &json!({})).is_err());
        assert!(parse_models(Agent::Grok, &json!([{"name":"missing id"}])).is_err());
        assert!(parse_models(Agent::Grok, &json!([])).unwrap().is_empty());
    }

    #[test]
    fn only_parameter_failures_get_a_catalog() {
        for error in [
            "model_not_found",
            "Unknown model 'bad'",
            "model 'bad' does not exist",
            "Invalid value 'extreme' for reasoning effort",
            "reasoning_effort is not supported with this model",
        ] {
            assert!(selection_error(error), "{error}");
        }
        for error in [
            "model does not exist or you do not have access to it",
            "401 invalid model",
            "429 model capacity",
            "model connection timed out",
            "Automatic approval review failed: model not found",
            "model exceeded quota",
            "invalid API key",
            "tool failed: file not found",
        ] {
            assert!(!selection_error(error), "{error}");
        }
    }

    #[test]
    fn text_is_compact_and_json_keeps_description() {
        let catalog = Catalog {
            models: parse_models(Agent::Claude, &json!([
                {"value":"default","displayName":"Default (recommended)","resolvedModel":"claude-opus","description":"Account default","supportedEffortLevels":["low","high"]},
                {"value":"opus","displayName":"Opus","resolvedModel":"claude-opus"},
                {"value":"claude-sonnet","displayName":"Claude Sonnet","resolvedModel":"claude-sonnet"},
                {"value":"custom","displayName":"Team model","resolvedModel":""}
            ])).unwrap(),
            default_model: Some("default".into()),
            configured_model: Some("custom".into()),
            ..Catalog::default()
        };
        assert_eq!(
            catalog.render(Agent::Claude),
            "Configured model: custom\ndefault → claude-opus  (account default)  effort: low, high\nopus → claude-opus\nclaude-sonnet\ncustom  Team model"
        );
        let json = serde_json::to_value(&catalog).unwrap();
        assert_eq!(json["models"][0]["description"], "Account default");
        assert_eq!(json["models"][0]["name"], "Default (recommended)");
        assert_eq!(json["models"][0]["resolved_model"], "claude-opus");
    }

    #[test]
    fn selections_outside_the_catalog_are_visible() {
        let catalog = Catalog {
            current_model: Some("custom".into()),
            ..Catalog::default()
        };
        assert_eq!(
            catalog.render(Agent::Grok),
            "No models returned.\ncustom  (current)"
        );
        let catalog = Catalog {
            default_model: Some("custom".into()),
            ..Catalog::default()
        };
        assert_eq!(
            catalog.render(Agent::Codex),
            "No models returned.\ncustom  (backend default)"
        );
    }
}
