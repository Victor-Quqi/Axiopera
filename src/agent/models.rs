//! Native model pickers, queried without starting an inference turn.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{BufReader, Lines};
use tokio::process::ChildStdout;

use super::Proc;
use crate::ipc::Agent;

#[derive(Clone, Debug, Default, Serialize)]
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

#[derive(Clone, Debug, Serialize)]
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

impl Model {
    fn provider(&self) -> Option<&str> {
        self.id
            .split_once('/')
            .map(|(name, _)| name)
            .filter(|name| !name.is_empty())
    }

    fn matches_name_or_id(&self, query: &str) -> bool {
        self.id.to_lowercase().contains(query)
            || self
                .name
                .as_ref()
                .is_some_and(|name| name.to_lowercase().contains(query))
    }
}

fn query_hint(agent: Agent) -> String {
    format!("Narrow with `{} <query>` or pass --all.", command(agent))
}

impl Catalog {
    fn with_header(&self, body: String) -> String {
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
        match spawns {
            Some(spawns) => format!("Without --model, spawns use {spawns}.\n{body}"),
            None => body,
        }
    }

    fn rows<'a>(
        &self,
        agent: Agent,
        models: impl ExactSizeIterator<Item = &'a Model>,
        limit: usize,
    ) -> String {
        let remaining = models.len().saturating_sub(limit);
        let mut lines = Vec::new();
        let default_label = match agent {
            Agent::Claude => "account default",
            _ => "backend default",
        };
        for model in models.take(limit) {
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
        if remaining > 0 {
            lines.push(format!("{remaining} more. {}", query_hint(agent)));
        }
        lines.join("\n")
    }

    fn providers(&self) -> Vec<(&str, usize)> {
        let mut counts = BTreeMap::new();
        for name in self.models.iter().filter_map(Model::provider) {
            let (_, count) = counts.entry(name.to_lowercase()).or_insert((name, 0));
            *count += 1;
        }
        counts.into_values().collect()
    }

    fn overview(&self, agent: Agent, limit: usize) -> String {
        let counts = self.providers();
        if counts.is_empty() {
            return self.rows(agent, self.models.iter(), limit);
        }
        let unprefixed = self.models.len() - counts.iter().map(|(_, count)| count).sum::<usize>();
        let mut groups: Vec<_> = counts
            .into_iter()
            .map(|(p, n)| format!("{} ({n})", single_line(p)))
            .collect();
        if unprefixed > 0 {
            groups.push(format!("unprefixed ({unprefixed})"));
        }
        format!(
            "Providers: {}.\nSelect with --provider <name>. {}",
            groups.join(", "),
            query_hint(agent)
        )
    }

    /// Only filtering changes the returned catalog. Row limits apply to text alone.
    pub fn listing(
        &self,
        agent: Agent,
        provider: Option<&str>,
        query: Option<&str>,
        all: bool,
    ) -> (String, Cow<'_, Self>) {
        const LIMIT: usize = 50;
        let filtered = provider.is_some() || query.is_some();
        let mut catalog = Cow::Borrowed(self);
        let mut empty = if filtered {
            "No matching models."
        } else {
            "No models returned."
        }
        .to_string();
        if let Some(provider) = provider {
            let provider = provider.trim_end_matches('/').to_lowercase();
            catalog
                .to_mut()
                .models
                .retain(|m| m.provider().is_some_and(|p| p.to_lowercase() == provider));
            if catalog.models.is_empty() {
                let names: Vec<_> = self
                    .providers()
                    .into_iter()
                    .map(|(p, _)| single_line(p))
                    .collect();
                if !names.is_empty() {
                    empty.push_str(&format!(" Available providers: {}.", names.join(", ")));
                }
            }
        }
        if let Some(query) = query {
            let query = query.to_lowercase();
            catalog.to_mut().models.retain(|m| {
                m.matches_name_or_id(&query)
                    || m.resolved_model
                        .as_ref()
                        .is_some_and(|id| id.to_lowercase().contains(&query))
            });
        }
        let body = if catalog.models.is_empty() {
            empty
        } else if !filtered && !all && catalog.models.len() > LIMIT {
            self.overview(agent, LIMIT)
        } else {
            self.rows(
                agent,
                catalog.models.iter(),
                if all { usize::MAX } else { LIMIT },
            )
        };
        (self.with_header(body), catalog)
    }

    pub fn render_error(&self, agent: Agent, requested: Option<&str>) -> String {
        const LIMIT: usize = 20;
        if self.models.len() <= 30 {
            return self.with_header(self.rows(agent, self.models.iter(), 30));
        }
        if let Some(raw) = requested.filter(|s| !s.is_empty()) {
            // A native ID can itself end in a qualifier such as ":free".
            let id = raw
                .rsplit_once(':')
                .filter(|(_, suffix)| !suffix.contains('/'))
                .map_or(raw, |(id, _)| id);
            if let Some(model) = self
                .models
                .iter()
                .find(|m| m.id == raw)
                .or_else(|| self.models.iter().find(|m| m.id == id))
            {
                return self.rows(agent, std::iter::once(model), LIMIT);
            }
            let query = id.to_lowercase();
            let matches: Vec<_> = match query.split_once('/') {
                Some((p, tail)) => {
                    let same_provider: Vec<_> = self
                        .models
                        .iter()
                        .filter(|m| m.provider().is_some_and(|name| name.to_lowercase() == p))
                        .collect();
                    let narrower: Vec<_> = if tail.is_empty() {
                        Vec::new()
                    } else {
                        same_provider
                            .iter()
                            .copied()
                            .filter(|m| m.matches_name_or_id(tail))
                            .collect()
                    };
                    if !narrower.is_empty() {
                        narrower
                    } else {
                        same_provider
                    }
                }
                None => self
                    .models
                    .iter()
                    .filter(|m| !query.is_empty() && m.matches_name_or_id(&query))
                    .collect(),
            };
            if !matches.is_empty() {
                return self.rows(agent, matches.into_iter(), LIMIT);
            }
        }
        self.overview(agent, LIMIT)
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
    if text.starts_with("opencode model ") && text.ends_with(" is disabled") {
        return true;
    }
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
        Agent::Pi => return super::pi::query(exe, cwd, env, configured_model, timeout).await,
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
            agy.listing(Agent::Agy, None, None, false).0,
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
            codex.listing(Agent::Codex, None, None, false).0,
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
            claude.listing(Agent::Claude, None, None, false).0,
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
            catalog.listing(Agent::Claude, None, None, false).0,
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
            catalog.listing(Agent::Codex, None, None, false).0,
            "Without --model, spawns use custom.\nNo models returned."
        );
    }

    fn numbered_catalog(prefix: &str, count: usize) -> Catalog {
        let models = (0..count)
            .map(|i| Model {
                id: format!("{prefix}{i:02}"),
                name: None,
                description: None,
                resolved_model: None,
                disabled: None,
                efforts: Some(vec!["high".into()]),
            })
            .collect();
        Catalog {
            models,
            default_model: Some("default".into()),
            native_model: Some("native".into()),
            native_effort: Some("low".into()),
            configured_model: Some("configured".into()),
            configured_effort: Some("medium".into()),
        }
    }

    #[test]
    fn provider_and_query_filters_preserve_catalog_defaults() {
        let mut catalog = numbered_catalog("Anthropic/", 2);
        catalog.models[0].name = Some("Claude Opus".into());
        catalog.models[1].resolved_model = Some("claude-sonnet".into());
        catalog
            .models
            .extend(numbered_catalog("openrouter/anthropic/opus", 1).models);
        let list = |p, q| catalog.listing(Agent::Pi, p, q, false);
        for (query, index) in [("OPUS", 0), ("sONNet", 1), ("01", 1)] {
            let (text, filtered) = list(Some("AnThRoPiC/"), Some(query));
            assert!(text.starts_with("Without --model, spawns use configured, effort medium"));
            assert_eq!(filtered.models.len(), 1);
            assert_eq!(filtered.models[0].id, catalog.models[index].id);
            let mut expected = json!(catalog);
            expected["models"] = json!([expected["models"][index]]);
            assert_eq!(json!(filtered), expected);
        }
        assert_eq!(list(None, Some("Opus")).1.models.len(), 2);
        assert_eq!(list(Some("anthropic"), None).1.models.len(), 2);
        let (text, filtered) = list(Some("missing"), None);
        assert!(filtered.models.is_empty());
        assert!(text.ends_with("No matching models. Available providers: Anthropic, openrouter."));
        let text = catalog
            .listing(Agent::Pi, Some("openrouter"), Some("absent"), true)
            .0;
        assert!(text.ends_with("No matching models."));
    }

    #[test]
    fn only_text_is_summarized_or_limited() {
        let small = numbered_catalog("team/", 50);
        let small_text = small.listing(Agent::Pi, None, None, false).0;
        assert_eq!(small_text.lines().count(), 51);
        assert_eq!(small_text, small.listing(Agent::Pi, None, None, true).0);
        let mut large = numbered_catalog("team/", 52);
        large.models.extend(numbered_catalog("other/", 1).models);
        let (summary, full) = large.listing(Agent::Pi, None, None, false);
        assert!(summary.starts_with("Without --model"));
        assert!(summary.contains("Providers: other (1), team (52)."));
        assert!(
            summary.contains("--provider <name>")
                && summary.contains("<query>")
                && summary.contains("--all")
        );
        assert!(!summary.contains("team/00"));
        assert_eq!(json!(full), json!(large));

        let (text, filtered) = large.listing(Agent::Pi, Some("team"), None, false);
        assert!(text.contains("team/49") && !text.contains("team/50"));
        assert!(text.contains("2 more.") && text.contains("--all"));
        assert_eq!(json!(filtered)["models"].as_array().unwrap().len(), 52);
        let text = large.listing(Agent::Pi, Some("team"), None, true).0;
        assert!(text.ends_with("team/51  effort: high"));
        assert!(!text.contains("other/"));
        assert_eq!(
            large.listing(Agent::Pi, None, None, true).0.lines().count(),
            54
        );

        let flat = numbered_catalog("model-", 51);
        let text = flat.listing(Agent::Codex, None, None, false).0;
        assert!(text.contains("model-49") && !text.contains("model-50"));
        assert!(text.contains("1 more.") && text.contains("<query>"));
    }

    #[test]
    fn selection_errors_show_relevant_rows_with_a_bounded_fallback() {
        let small = numbered_catalog("team/", 30);
        assert_eq!(
            small.render_error(Agent::Pi, Some("team/01")),
            small.listing(Agent::Pi, None, None, true).0
        );
        let mut catalog = numbered_catalog("team/", 30);
        catalog.models[2].name = Some("Featured".into());
        catalog
            .models
            .extend(numbered_catalog("other/team/", 1).models);
        let render = |requested| catalog.render_error(Agent::Pi, Some(requested));
        for requested in ["team/11", "team/11:bad"] {
            assert_eq!(render(requested), "team/11  effort: high");
        }
        assert_eq!(render("FEATURED"), "team/02  Featured  effort: high");
        assert_eq!(render("team/featured"), "team/02  Featured  effort: high");
        assert_eq!(render("other/00"), "other/team/00  effort: high");
        assert_eq!(render("29:bad"), "team/29  effort: high");
        let text = render("TEAM/missing");
        assert!(text.contains("team/19") && !text.contains("team/20") && !text.contains("other/"));
        assert!(text.contains("10 more.") && text.contains("axiopera models pi"));
        let text = render("absent");
        assert!(
            text.contains("Providers: other (1), team (30).")
                && text.contains("axiopera models pi")
        );
        let flat = numbered_catalog("model-", 31);
        let text = flat.render_error(Agent::Codex, None);
        assert!(
            text.contains("model-19") && !text.contains("model-20") && text.contains("11 more.")
        );
        assert!(text.contains("axiopera models codex"));
    }
}
