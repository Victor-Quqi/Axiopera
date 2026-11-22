//! Native model pickers, queried without starting an inference turn.

use std::collections::{HashMap, HashSet};
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
    /// The catalog's own default: Claude's account default, Codex's backend default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// What the agent itself picks when given no model or effort, user settings included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_effort: Option<String>,
    /// Axiopera's configured model, passed to spawns without --model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_effort: Option<String>,
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
        // A configured model may take a different default effort: name the model alone.
        let spawns = match (
            &self.configured_model,
            &self.native_model,
            &self.native_effort,
        ) {
            (Some(model), _, _) => {
                let effort = self
                    .configured_effort
                    .as_deref()
                    .map(|v| format!(", effort {}", single_line(v)))
                    .unwrap_or_default();
                Some(format!("{}{effort} (Axiopera config)", single_line(model)))
            }
            (None, Some(model), Some(effort)) => Some(format!(
                "{}, effort {}",
                single_line(model),
                single_line(effort)
            )),
            (None, Some(model), None) => Some(single_line(model)),
            (None, None, _) => None,
        };
        if let Some(spawns) = spawns {
            lines.push(format!("Without --model, spawns use {spawns}."));
        }
        if self.models.is_empty() {
            lines.push("No models returned.".into());
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
                && (name_key(name) != name_key(&model.id)
                    || (agent == Agent::Agy
                        && model.efforts.as_deref() == Some(&[])
                        && name.ends_with(')')))
                && !(agent == Agent::Claude && model.id == "default")
            {
                row.push_str(&format!("  {}", single_line(name)));
            }
            if self.default_model.as_deref() == Some(&model.id) {
                row.push_str(&format!("  ({default_label})"));
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
    configured_model: Option<&str>,
    timeout: Duration,
) -> Result<Catalog> {
    let args: Vec<String> = match agent {
        Agent::Pi => return super::pi::query(exe, cwd, env, timeout).await,
        Agent::Opencode => {
            return super::opencode::query(exe, cwd, env, configured_model, timeout).await;
        }
        Agent::Codex => vec!["app-server"],
        Agent::Agy => vec!["acp"],
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
    let preference = (agent == Agent::Agy)
        .then(|| super::agy::saved_model(env))
        .flatten();
    query_process(
        agent,
        proc,
        stdout,
        cwd,
        timeout,
        (preference.as_deref(), configured_model),
    )
    .await
}

async fn query_process(
    agent: Agent,
    mut proc: Proc,
    stdout: ChildStdout,
    cwd: &Path,
    timeout: Duration,
    defaults: (Option<&str>, Option<&str>),
) -> Result<Catalog> {
    let mut lines = super::lines(stdout);
    let result =
        tokio::time::timeout(timeout, discover(agent, &proc, &mut lines, cwd, defaults)).await;
    // Kill the whole job on success and error, including any startup helper processes.
    proc.job.kill();
    let _ = proc.child.wait().await;
    result.context("model query timed out")?
}

async fn discover(
    agent: Agent,
    proc: &Proc,
    lines: &mut Lines<BufReader<ChildStdout>>,
    cwd: &Path,
    (preference, configured_model): (Option<&str>, Option<&str>),
) -> Result<Catalog> {
    let mut catalog = Catalog::default();
    match agent {
        Agent::Pi | Agent::Opencode => bail!("this backend has a dedicated model query"),
        Agent::Claude => {
            proc.stdin.send(&json!({"type":"control_request","request_id":"models","request":{"subtype":"initialize","hooks":{}}})).await?;
            let response = claude_response(proc, lines, "models")
                .await?
                .context("agent exited before returning models")?;
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
            // The model and effort a session gets from the user's settings. The reply holds all
            // of the settings: only these two are read. Without it the catalog still stands.
            proc.stdin.send(&json!({"type":"control_request","request_id":"settings","request":{"subtype":"get_settings"}})).await?;
            let settings = claude_response(proc, lines, "settings");
            if let Ok(Ok(Some(response))) = tokio::time::timeout(SETTINGS_WAIT, settings).await
                && response["subtype"] == "success"
            {
                let applied = &response["response"]["applied"];
                catalog.native_model = string(applied, "model");
                catalog.native_effort = string(applied, "effort");
            }
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
            let mut efforts = HashMap::new();
            let mut id = 2;
            loop {
                let page = rpc(proc, lines, id, "model/list", json!({"cursor":cursor})).await?;
                let models = parse_models(agent, &page["data"])?;
                for (row, model) in page["data"].as_array().unwrap().iter().zip(&models) {
                    if row["isDefault"] == true {
                        catalog.default_model = Some(model.id.clone());
                    }
                    if let Some(effort) = string(row, "defaultReasoningEffort") {
                        efforts.insert(model.id.clone(), effort);
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
            // A new thread takes the configured model and effort, project layers included, else
            // the catalog's defaults. Only these two keys of the configuration are read.
            let params = json!({"cwd": cwd, "includeLayers": false});
            if let Ok(read) = rpc(proc, lines, id + 1, "config/read", params).await {
                catalog.native_model = string(&read["config"], "model");
                catalog.native_effort = string(&read["config"], "model_reasoning_effort");
            }
            let model = catalog
                .native_model
                .take()
                .or(catalog.default_model.clone());
            if catalog.native_effort.is_none() {
                catalog.native_effort = model.as_ref().and_then(|m| efforts.remove(m));
            }
            catalog.native_model = model;
        }
        Agent::Agy => {
            rpc(proc, lines, 1, "initialize", super::agy::initialize()).await?;
            let mut session = rpc(
                proc,
                lines,
                2,
                "session/new",
                json!({"cwd":cwd,"mcpServers":[]}),
            )
            .await?;
            let id = session["sessionId"].clone();
            let preferred = preference
                .and_then(|label| super::agy::preferred_variant(&session, label, None))
                .map(str::to_owned);
            let mut inherits_effort = false;
            if let Some(variant) = preferred
                && rpc(
                    proc,
                    lines,
                    4,
                    "session/set_model",
                    json!({"sessionId":id,"modelId":variant}),
                )
                .await
                .is_ok()
            {
                inherits_effort = preference.is_some_and(|label| {
                    super::agy::preferred_variant(&session, label, configured_model).is_some()
                });
                session = rpc(
                    proc,
                    lines,
                    5,
                    "session/resume",
                    json!({"sessionId":id,"cwd":cwd,"mcpServers":[]}),
                )
                .await?;
            }
            let choices =
                super::agy::option(&session, "model").context("missing AGY model picker")?;
            catalog.models = parse_models(agent, &choices["options"])?;
            catalog.native_model = string(choices, "currentValue");
            catalog.native_effort = super::agy::option(&session, "thought_level")
                .and_then(|o| string(o, "currentValue"));
            // ACP returns the effort picker for the selected family only.
            // Switch models on the disposable catalog session, without starting a turn.
            for (index, model) in catalog.models.iter_mut().enumerate() {
                let switched;
                let configured = Some(model.id.as_str()) == configured_model;
                let picker = if Some(&model.id) == catalog.native_model.as_ref()
                    && (!configured || inherits_effort)
                {
                    &session
                } else {
                    let Ok(reply) = rpc(
                        proc,
                        lines,
                        6 + index as u64,
                        "session/set_config_option",
                        json!({"sessionId":id,"configId":"model","value":model.id}),
                    )
                    .await
                    else {
                        continue;
                    };
                    switched = reply;
                    &switched
                };
                if super::agy::option(picker, "model").and_then(|o| o["currentValue"].as_str())
                    == Some(model.id.as_str())
                {
                    let effort = super::agy::option(picker, "thought_level");
                    if configured {
                        catalog.configured_effort = effort.and_then(|o| string(o, "currentValue"));
                    }
                    model.efforts = Some(
                        effort
                            .and_then(|o| o["options"].as_array())
                            .into_iter()
                            .flatten()
                            .filter_map(|r| string(r, "value"))
                            .collect(),
                    );
                }
            }
            rpc(
                proc,
                lines,
                6 + catalog.models.len() as u64,
                "session/close",
                json!({"sessionId":id}),
            )
            .await?;
        }
        Agent::Grok => {
            rpc(proc, lines, 1, "initialize", json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"axiopera","version":env!("CARGO_PKG_VERSION")}})).await?;
            let response = rpc(proc, lines, 2, "_x.ai/models/list", json!({})).await?;
            if !response["error"].is_null() {
                bail!("models/list: {}", response["error"])
            }
            let payload = response.get("result").unwrap_or(&response);
            catalog.models = parse_models(agent, &payload["availableModels"])?;
            // New sessions start on the current model, at the effort listed with it.
            catalog.native_model = string(payload, "currentModelId");
            catalog.native_effort = payload["availableModels"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|row| catalog.native_model.as_deref() == row["modelId"].as_str())
                .and_then(|row| string(&row["_meta"], "reasoningEffort"));
        }
    }
    Ok(catalog)
}

/// How long the catalog waits for Claude's settings, which older versions may not answer.
const SETTINGS_WAIT: Duration = Duration::from_secs(2);

/// The reply to the control request `id`; Claude's own requests are turned down meanwhile.
async fn claude_response(
    proc: &Proc,
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: &str,
) -> Result<Option<Value>> {
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if msg["type"] == "control_request" {
            proc.stdin.send(&json!({"type":"control_response","response":{"subtype":"error","request_id":msg["request_id"],"error":"model query only"}})).await?;
        }
        if msg["type"] == "control_response" && msg["response"]["request_id"] == id {
            return Ok(Some(msg["response"].clone()));
        }
    }
    Ok(None)
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
                Agent::Agy => ("value", "name", None),
                Agent::Opencode | Agent::Pi => bail!("this backend has a dedicated model parser"),
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
        query_process(
            agent,
            proc,
            stdout,
            &std::env::current_dir()?,
            timeout,
            (
                (mode == "configured-agy-first").then_some("First (High)"),
                mode.strip_prefix("configured-agy-"),
            ),
        )
        .await
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
            if msg["request"]["subtype"] == "get_settings" {
                if mode == "silent" {
                    continue;
                }
                send(
                    json!({"type":"control_response","response":{"request_id":msg["request_id"],"subtype":"success","response":{"env":{"SECRET":"x"},"applied":{"model":"claude-opus","effort":"xhigh"}}}}),
                );
                continue;
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
                Some("session/new") => json!({"sessionId":"catalog",
                    "models":{"availableModels":[{"name":"First (High)","modelId":"first-high"}]},
                    "configOptions":[
                    {"id":"model","currentValue":"first","options":[
                        {"value":"first","name":"First"},{"value":"second","name":"Second"},
                        {"value":"fixed-medium","name":"Fixed (Medium)"},{"value":"unavailable","name":"Unavailable"}]},
                    {"id":"thought_level","currentValue":"low","options":[{"value":"low"}]}]}),
                Some("session/set_model") => {
                    assert_eq!(msg["params"]["modelId"], "first-high");
                    send(json!({"id":msg["id"],"error":{"code":-32603,"message":"unavailable"}}));
                    continue;
                }
                Some("session/set_config_option") => {
                    let model = &msg["params"]["value"];
                    if model == "unavailable" {
                        send(
                            json!({"jsonrpc":"2.0","id":msg["id"],"error":{"code":-32602,"message":"unavailable"}}),
                        );
                        continue;
                    }
                    let mut options = vec![json!({"id":"model","currentValue":model})];
                    if model == "second" {
                        options.push(json!({"id":"thought_level","currentValue":"deep","options":[{"value":"deep"},{"value":"extra"}]}));
                    }
                    json!({"configOptions":options})
                }
                Some("session/close") => json!({}),
                Some("model/list") => {
                    if msg["params"]["cursor"].is_null() {
                        send(
                            json!({"jsonrpc":"2.0","id":"callback","method":"test/request","params":{}}),
                        );
                        json!({"data":[{"model":"first","displayName":"First"}],"nextCursor":"page2"})
                    } else {
                        json!({"data":[{"model":"second","displayName":"Second","isDefault":true,"defaultReasoningEffort":"medium"}],"nextCursor":if mode == "repeat" { json!("page2") } else { Value::Null }})
                    }
                }
                Some("config/read") => match mode.as_str() {
                    "configured" => {
                        json!({"config":{"model":"first","model_reasoning_effort":"high","model_providers":{"x":{"token":"secret"}}}})
                    }
                    "unsupported" => {
                        send(
                            json!({"jsonrpc":"2.0","id":msg["id"],"error":{"code":-32601,"message":"method not found"}}),
                        );
                        continue;
                    }
                    _ => json!({"config":{"model":null}}),
                },
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
                        json!({"result":{"currentModelId":"company-claude","availableModels":[{"modelId":"other","name":"Other"},{"modelId":"company-claude","name":"Claude","_meta":{"reasoningEffort":"high"}}]}})
                    }
                }
                _ => continue,
            };
            send(json!({"jsonrpc":"2.0","id":msg["id"],"result":result}));
        }
    }

    #[tokio::test]
    async fn rejected_agy_preference_keeps_the_native_catalog() {
        let catalog = fixture(Agent::Agy, "configured-agy-first", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(catalog.native_model.as_deref(), Some("first"));
        assert_eq!(catalog.native_effort.as_deref(), Some("low"));
        // Explicitly selecting this family resets its effort in the fixture.
        assert_eq!(catalog.configured_effort, None);
        assert_eq!(
            catalog.models[1].efforts.as_ref().unwrap(),
            &["deep", "extra"]
        );
    }

    #[tokio::test]
    async fn unavailable_configured_agy_model_keeps_other_choices_visible() {
        let catalog = fixture(
            Agent::Agy,
            "configured-agy-unavailable",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(catalog.native_model.as_deref(), Some("first"));
        assert_eq!(catalog.configured_effort, None);
        assert_eq!(
            catalog.models[1].efforts.as_ref().unwrap(),
            &["deep", "extra"]
        );
        assert!(catalog.models[3].efforts.is_none());
    }

    #[tokio::test]
    async fn agy_catalog_reports_each_models_own_effort_picker() {
        let agy = fixture(Agent::Agy, "ok", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(
            agy.render(Agent::Agy),
            "Without --model, spawns use first, effort low.\nfirst  effort: low\nsecond  effort: deep, extra\nfixed-medium  Fixed (Medium)\nunavailable"
        );
        let rows = serde_json::to_value(&agy).unwrap()["models"].clone();
        assert_eq!(rows[1]["efforts"], json!(["deep", "extra"]));
        assert_eq!(rows[2]["efforts"], json!([]));
        assert!(rows[3].get("efforts").is_none());
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
        assert_eq!(
            codex.render(Agent::Codex),
            "Without --model, spawns use second, effort medium.\nfirst\nsecond  (backend default)"
        );
        // The user's configuration wins over the catalog's default.
        let configured = fixture(Agent::Codex, "configured", timeout).await.unwrap();
        assert_eq!(configured.native_model.as_deref(), Some("first"));
        assert_eq!(configured.native_effort.as_deref(), Some("high"));
        // Without config/read the catalog's default still answers.
        let old = fixture(Agent::Codex, "unsupported", timeout).await.unwrap();
        assert_eq!(old.native_model.as_deref(), Some("second"));
        let grok = fixture(Agent::Grok, "ok", timeout).await.unwrap();
        assert_eq!(grok.native_model.as_deref(), Some("company-claude"));
        assert_eq!(grok.native_effort.as_deref(), Some("high"));
        assert!(grok.default_model.is_none());
        let json = serde_json::to_value(&grok).unwrap();
        assert_eq!(json["native_model"], "company-claude");
        assert!(json.get("default_model").is_none());
        let claude = fixture(Agent::Claude, "ok", timeout).await.unwrap();
        assert_eq!(claude.default_model.as_deref(), Some("default"));
        assert_eq!(
            claude.render(Agent::Claude),
            "Without --model, spawns use claude-opus, effort xhigh.\ndefault  (account default)\nrestricted  (disabled)"
        );
        assert_eq!(claude.models[1].disabled, Some(true));
        // Versions that do not answer get_settings still list their models, after a wait.
        let silent = fixture(Agent::Claude, "silent", timeout).await.unwrap();
        assert!(silent.native_model.is_none());
        assert_eq!(silent.models.len(), 2);
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
            native_model: Some("claude-opus".into()),
            native_effort: Some("xhigh".into()),
            configured_model: Some("custom".into()),
            configured_effort: None,
        };
        // A configured model may take another default effort: none is named.
        assert_eq!(
            catalog.render(Agent::Claude),
            "Without --model, spawns use custom (Axiopera config).\ndefault → claude-opus  (account default)  effort: low, high\nopus → claude-opus\nclaude-sonnet\ncustom  Team model"
        );
        let json = serde_json::to_value(&catalog).unwrap();
        assert_eq!(json["models"][0]["description"], "Account default");
        assert_eq!(json["models"][0]["name"], "Default (recommended)");
        assert_eq!(json["models"][0]["resolved_model"], "claude-opus");
    }

    #[test]
    fn spawn_defaults_outside_the_catalog_are_visible() {
        let catalog = Catalog {
            native_model: Some("custom".into()),
            ..Catalog::default()
        };
        assert_eq!(
            catalog.render(Agent::Codex),
            "Without --model, spawns use custom.\nNo models returned."
        );
    }
}
