// SPDX-License-Identifier: AGPL-3.0-only

//! MCP transport (doc 04 §3) over a single `POST /mcp` JSON-RPC endpoint, so MCP
//! clients (Claude Desktop, Cursor) drive Mnestic with the same tool names as the
//! supermemory MCP server. Streamable HTTP in its simplest form: each request gets a
//! JSON response (no SSE, which tools do not need); a notification gets 202 and no body.
//! Tools: `memory` (save/forget), `recall`, `listProjects`, `whoAmI`, `memory-graph`.
//! Resources: `supermemory://projects` and the `supermemory://profile/{containerTag}`
//! template. Prompt: `context`. The profile resource and the prompt are scoped by the
//! containerTag in the URI/argument, since a bare resource has no actor otherwise.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use mnestic_engine::Profile;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::authenticate_request;
use crate::container_tag::{parse_container_tag, Scope};
use crate::error::ApiError;
use crate::{clamp_limit, resolve_container_tag, AppState};

const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

pub async fn mcp(State(state): State<AppState>, headers: HeaderMap, Json(msg): Json<Value>) -> Response {
    let tenant = match authenticate_request(&state, &headers).await {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    if !msg.is_object() {
        // Reject a batch (array) or other non-object loudly rather than silently 202.
        return Json(json!({
            "jsonrpc": "2.0", "id": Value::Null,
            "error": { "code": -32600, "message": "expected a single JSON-RPC request object" }
        }))
        .into_response();
    }
    match handle(&state, tenant, &msg).await {
        Some(response) => Json(response).into_response(),
        // A notification (no id, e.g. notifications/initialized) gets no JSON-RPC reply.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn handle(state: &AppState, tenant: Uuid, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
    let outcome: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(initialize_result(msg.get("params"))),
        "tools/list" => Ok(tools_list()),
        "ping" => Ok(json!({})),
        "tools/call" => call_tool(state, tenant, msg.get("params")).await,
        "resources/list" => Ok(resources_list()),
        "resources/templates/list" => Ok(resource_templates_list()),
        "resources/read" => read_resource(state, tenant, msg.get("params")).await,
        "prompts/list" => Ok(prompts_list()),
        "prompts/get" => get_prompt(state, tenant, msg.get("params")).await,
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

fn initialize_result(params: Option<&Value>) -> Value {
    // Honor the client's requested version only if we actually speak it; otherwise answer
    // with our own so the client fails fast instead of proceeding on a wrong contract.
    let requested = params.and_then(|p| p.get("protocolVersion")).and_then(Value::as_str);
    let version = match requested {
        Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
        _ => DEFAULT_PROTOCOL_VERSION,
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
        "serverInfo": { "name": "mnestic", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn tools_list() -> Value {
    json!({ "tools": [
        {
            "name": "memory",
            "description": "Save a memory or forget memories by content. action is save or forget.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "action": { "type": "string", "enum": ["save", "forget"] },
                    "containerTag": { "type": "string" }
                },
                "required": ["content"]
            }
        },
        {
            "name": "recall",
            "description": "Hybrid search over the actor's memories; optionally include the profile.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "includeProfile": { "type": "boolean" },
                    "containerTag": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                "required": ["query"]
            }
        },
        {
            "name": "listProjects",
            "description": "List the container tags (projects) in use.",
            "inputSchema": { "type": "object", "properties": { "refresh": { "type": "boolean" } } }
        },
        {
            "name": "whoAmI",
            "description": "Return the authenticated user (userId, email, name).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "memory-graph",
            "description": "Summary of the actor's documents (ids and titles) and the entities extracted across their memories.",
            "inputSchema": { "type": "object", "properties": { "containerTag": { "type": "string" } } }
        }
    ]})
}

/// Dispatch a `tools/call`. A protocol problem (bad params, unknown tool) is a JSON-RPC
/// error; a tool execution problem is a result with `isError: true` (the MCP convention),
/// so the model sees the failure instead of the transport swallowing it.
async fn call_tool(state: &AppState, tenant: Uuid, params: Option<&Value>) -> Result<Value, (i64, String)> {
    let params = params.ok_or((-32602, "missing params".to_string()))?;
    let name = params.get("name").and_then(Value::as_str).ok_or((-32602, "missing tool name".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    // memory-graph also returns structuredContent (a sibling of content, per the MCP
    // spec), so it is built specially rather than as a plain text result.
    if name == "memory-graph" {
        return Ok(match memory_graph_tool(state, tenant, &args).await {
            Ok((summary, structured)) => {
                json!({ "content": [{ "type": "text", "text": summary }], "structuredContent": structured })
            }
            Err(message) => json!({ "content": [{ "type": "text", "text": message }], "isError": true }),
        });
    }
    let outcome = match name {
        "memory" => memory_tool(state, tenant, &args).await,
        "recall" => recall_tool(state, tenant, &args).await,
        "listProjects" => list_projects_tool(state, tenant).await,
        "whoAmI" => who_am_i_tool(state, tenant).await,
        other => return Err((-32602, format!("unknown tool: {other}"))),
    };
    Ok(match outcome {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
        Err(message) => json!({ "content": [{ "type": "text", "text": message }], "isError": true }),
    })
}

/// Keep an internal cause out of the tool result (it reaches the model/client); log it.
fn scrub(e: impl std::fmt::Display) -> String {
    eprintln!("mnestic-server mcp tool error: {e}");
    "internal error".to_string()
}

fn tag_from_args(args: &Value) -> Result<String, String> {
    let singular = args.get("containerTag").and_then(Value::as_str).map(str::to_string);
    let plural = args.get("containerTags").and_then(Value::as_array).map(|a| {
        a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>()
    });
    resolve_container_tag(singular, plural).map_err(|e| match e {
        ApiError::BadRequest(m) => m,
        _ => "invalid containerTag".to_string(),
    })
}

async fn memory_tool(state: &AppState, tenant: Uuid, args: &Value) -> Result<String, String> {
    let content = args.get("content").and_then(Value::as_str).unwrap_or_default();
    if content.trim().is_empty() {
        return Err("content is empty".to_string());
    }
    let tag = tag_from_args(args)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    match args.get("action").and_then(Value::as_str).unwrap_or("save") {
        "save" => {
            let r = state
                .engine
                .add(tenant, &actor_id, &container_tags, content, "conversation", None)
                .await
                .map_err(scrub)?;
            Ok(if r.idempotent_skip { "skipped".to_string() } else { format!("saved {}", r.source_id) })
        }
        "forget" => {
            let ids = state.engine.forget_by_content(tenant, &actor_id, content).await.map_err(scrub)?;
            Ok(format!("forgot {} memories", ids.len()))
        }
        other => Err(format!("unknown action: {other}")),
    }
}

async fn recall_tool(state: &AppState, tenant: Uuid, args: &Value) -> Result<String, String> {
    let query = args.get("query").and_then(Value::as_str).unwrap_or_default();
    if query.trim().is_empty() {
        return Err("query is empty".to_string());
    }
    let tag = tag_from_args(args)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    let limit = clamp_limit(args.get("limit").and_then(Value::as_i64));

    let hits = state
        .engine
        .recall_scoped(tenant, &actor_id, &container_tags, query, limit, None, false, true)
        .await
        .map_err(scrub)?;
    let results: Vec<Value> = hits
        .iter()
        .map(|h| {
            json!({
                "id": h.id.to_string(),
                "memory": h.content,
                "similarity": h.score,
                "updatedAt": h.recorded_at.map(|t| t.to_rfc3339()),
            })
        })
        .collect();
    let mut out = json!({ "results": results });
    // includeProfile defaults true (doc 04 §3).
    if args.get("includeProfile").and_then(Value::as_bool).unwrap_or(true) {
        let p = state.engine.profile(tenant, &actor_id).await.map_err(scrub)?;
        out["profile"] = json!({ "staticFacts": p.static_facts, "dynamicCtx": p.dynamic_ctx });
    }
    Ok(out.to_string())
}

fn internal(e: impl std::fmt::Display) -> (i64, String) {
    eprintln!("mnestic-server mcp error: {e}");
    (-32603, "internal error".to_string())
}

fn profile_markdown(p: &Profile) -> String {
    let mut s = String::from("# Memory profile\n");
    if !p.static_facts.is_empty() {
        s.push_str("\n## Durable facts\n");
        for f in &p.static_facts {
            s.push_str("- ");
            s.push_str(f);
            s.push('\n');
        }
    }
    if !p.dynamic_ctx.is_empty() {
        s.push_str("\n## Recent context\n");
        for c in &p.dynamic_ctx {
            s.push_str("- ");
            s.push_str(c);
            s.push('\n');
        }
    }
    s
}

fn resources_list() -> Value {
    json!({ "resources": [
        { "uri": "supermemory://projects", "name": "projects",
          "description": "Container tags in use, as JSON.", "mimeType": "application/json" }
    ]})
}

fn resource_templates_list() -> Value {
    json!({ "resourceTemplates": [
        { "uriTemplate": "supermemory://profile/{containerTag}", "name": "profile",
          "description": "Markdown memory profile for a container.", "mimeType": "text/markdown" }
    ]})
}

async fn read_resource(state: &AppState, tenant: Uuid, params: Option<&Value>) -> Result<Value, (i64, String)> {
    let uri = params
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .ok_or((-32602, "missing uri".to_string()))?;
    if uri == "supermemory://projects" {
        let tags = state.engine.store().list_container_tags(tenant).await.map_err(internal)?;
        return Ok(json!({ "contents": [{
            "uri": uri, "mimeType": "application/json", "text": json!(tags).to_string()
        }]}));
    }
    if let Some(raw) = uri.strip_prefix("supermemory://profile/") {
        let tag = resolve_container_tag(Some(raw.to_string()), None)
            .map_err(|_| (-32602, "invalid containerTag in uri".to_string()))?;
        let Scope { actor_id, .. } = parse_container_tag(&tag);
        let profile = state.engine.profile(tenant, &actor_id).await.map_err(internal)?;
        return Ok(json!({ "contents": [{
            "uri": uri, "mimeType": "text/markdown", "text": profile_markdown(&profile)
        }]}));
    }
    Err((-32602, format!("unknown resource: {uri}")))
}

fn prompts_list() -> Value {
    json!({ "prompts": [
        { "name": "context", "description": "Injects the user's memory profile as a message.",
          "arguments": [{ "name": "containerTag", "description": "Which container's profile.", "required": true }] }
    ]})
}

async fn get_prompt(state: &AppState, tenant: Uuid, params: Option<&Value>) -> Result<Value, (i64, String)> {
    let params = params.ok_or((-32602, "missing params".to_string()))?;
    let name = params.get("name").and_then(Value::as_str).ok_or((-32602, "missing prompt name".to_string()))?;
    if name != "context" {
        return Err((-32602, format!("unknown prompt: {name}")));
    }
    let raw = params
        .get("arguments")
        .and_then(|a| a.get("containerTag"))
        .and_then(Value::as_str)
        .ok_or((-32602, "context requires a containerTag argument".to_string()))?;
    let tag = resolve_container_tag(Some(raw.to_string()), None)
        .map_err(|_| (-32602, "invalid containerTag".to_string()))?;
    let Scope { actor_id, .. } = parse_container_tag(&tag);
    let profile = state.engine.profile(tenant, &actor_id).await.map_err(internal)?;
    Ok(json!({
        "description": "The user's memory profile as context.",
        "messages": [{ "role": "user", "content": { "type": "text", "text": profile_markdown(&profile) } }]
    }))
}

async fn list_projects_tool(state: &AppState, tenant: Uuid) -> Result<String, String> {
    let tags = state.engine.store().list_container_tags(tenant).await.map_err(scrub)?;
    Ok(json!({ "projects": tags }).to_string())
}

async fn who_am_i_tool(state: &AppState, tenant: Uuid) -> Result<String, String> {
    let user_id = state.engine.store().tenant_external_id(tenant).await.map_err(scrub)?.unwrap_or_default();
    Ok(json!({ "userId": user_id, "email": Value::Null, "name": Value::Null }).to_string())
}

/// Top entity surfaces shown in the memory-graph view. Bounded so a noisy graph does not flood
/// the result; the strongest-mentioned entities are what a graph summary wants.
const GRAPH_ENTITY_LIMIT: i64 = 50;

/// Returns (summary text, structuredContent). Actor-wide on purpose: a memory-graph is the
/// user's whole document set plus the entities extracted across their memories, so the container
/// tags inside the containerTag are not used to filter it (unlike recall).
async fn memory_graph_tool(state: &AppState, tenant: Uuid, args: &Value) -> Result<(String, Value), String> {
    let tag = tag_from_args(args)?;
    let Scope { actor_id, .. } = parse_container_tag(&tag);
    let docs = state.engine.store().list_documents(tenant, &actor_id).await.map_err(scrub)?;
    let documents: Vec<Value> = docs
        .iter()
        .map(|(id, title)| json!({ "id": id.to_string(), "title": title }))
        .collect();
    let entity_rows = state
        .engine
        .store()
        .actor_entities(tenant, &actor_id, GRAPH_ENTITY_LIMIT)
        .await
        .map_err(scrub)?;
    let entities: Vec<Value> = entity_rows
        .iter()
        .map(|(surface, mentions)| json!({ "surface": surface, "mentions": mentions }))
        .collect();
    let total = documents.len();
    let ents = entities.len();
    Ok((
        format!("{total} document(s), {ents} entit{}", if ents == 1 { "y" } else { "ies" }),
        json!({ "documents": documents, "totalCount": total, "entities": entities }),
    ))
}
