//! MCP server (Streamable HTTP, JSON responses) on `/mcp`.
//!
//! One endpoint serves every tool. Home Assistant's "Model Context Protocol"
//! integration connects to one URL and lists the tools, so a new tool is one
//! more entry in `tools()`, not a new endpoint. Each tool definition and each
//! token of a result costs prefill time on a small CPU model, so tools stay
//! few, short and terse.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::search::{self, SearchBackend};

pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

#[derive(Clone, Default)]
pub struct Tools {
    pub web_search: Option<Arc<dyn SearchBackend>>,
}

fn web_search_definition() -> Value {
    json!({
        "name": "web_search",
        "description": "Searches the internet. Use it for news, current events, sports results and facts \
            that can change. Your own knowledge is older than these results. For recent events, put the \
            current year in the query. Returns a few short results with their source and date.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to search for."},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 5, "description": "Number of results. Default 3."},
            },
            "required": ["query"],
        },
    })
}

fn tool_error(text: &str) -> Value {
    json!({"isError": true, "content": [{"type": "text", "text": text}]})
}

impl Tools {
    pub fn list(&self) -> Vec<Value> {
        let mut tools = Vec::new();
        if self.web_search.is_some() {
            tools.push(web_search_definition());
        }
        tools
    }

    async fn call(&self, name: &str, args: &Value) -> Value {
        match (name, &self.web_search) {
            ("web_search", Some(backend)) => {
                let query = args["query"].as_str().unwrap_or("").trim();
                if query.is_empty() {
                    return tool_error("query is empty");
                }
                let count = args["max_results"].as_u64().map_or(3, |n| n.clamp(1, 5)) as usize;
                // Add the year for recency questions. The log shows the query as sent.
                let query = search::with_year(query, search::current_year());
                match backend.search(query.clone(), count).await {
                    Ok(results) => {
                        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                        let text = search::format_results(&results, count, &today);
                        tracing::info!("search {query:?} -> {} chars", text.len());
                        json!({"content": [{"type": "text", "text": text}]})
                    }
                    Err(err) => {
                        tracing::warn!("search failed for {query:?}: {err:#}");
                        tool_error(&format!("Search failed: {err:#}"))
                    }
                }
            }
            _ => tool_error(&format!("Unknown tool {name}")),
        }
    }

    /// Handles one JSON-RPC message. `None` for notifications.
    pub async fn handle(&self, msg: &Value) -> Option<Value> {
        let method = msg["method"].as_str()?;
        let id = msg.get("id").filter(|id| !id.is_null())?.clone();
        let params = &msg["params"];
        let result = match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or("");
                let version = PROTOCOL_VERSIONS
                    .iter()
                    .find(|v| **v == asked)
                    .unwrap_or(&PROTOCOL_VERSIONS[0]);
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "assistant-hub", "version": env!("CARGO_PKG_VERSION")},
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools": self.list()}),
            "tools/call" => {
                self.call(params["name"].as_str().unwrap_or(""), &params["arguments"])
                    .await
            }
            _ => {
                return Some(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("Method not found: {method}")},
                }));
            }
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{Hit, Results};
    use futures_util::future::BoxFuture;

    struct Fake;
    impl SearchBackend for Fake {
        fn search(&self, query: String, _max: usize) -> BoxFuture<'_, anyhow::Result<Results>> {
            Box::pin(async move {
                Ok(Results {
                    results: vec![Hit {
                        title: query,
                        url: "https://example.org/a".into(),
                        content: "c".into(),
                        published: String::new(),
                    }],
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn initialize_list_and_call() {
        let tools = Tools {
            web_search: Some(Arc::new(Fake)),
        };
        let init = tools
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}))
            .await
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(
            tools
                .handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .await
                .is_none()
        );
        let list = tools
            .handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .await
            .unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], "web_search");
        let call = tools
            .handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "web_search", "arguments": {"query": "latest news"}}}))
            .await
            .unwrap();
        let text = call["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains(&format!("latest news {}", search::current_year())),
            "{text}"
        );
        let unknown = tools
            .handle(&json!({"jsonrpc": "2.0", "id": 4, "method": "nope"}))
            .await
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn no_tools_without_search() {
        let tools = Tools::default();
        let list = tools
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await
            .unwrap();
        assert_eq!(list["result"]["tools"], json!([]));
    }
}
