//! Tavily Web Search WASM Tool for IronClaw.
//!
//! Searches the web using Tavily Search API and returns structured results.

wit_bindgen::generate!({
    world: "sandboxed-tool",
    path: "../../wit/tool.wit",
});

use serde::Deserialize;

const TAVILY_SEARCH_ENDPOINT: &str = "https://api.tavily.com/search";
const DEFAULT_MAX_RESULTS: u32 = 5;
const MAX_RESULTS: u32 = 20;
const MAX_RETRIES: u32 = 3;

struct TavilyTool;

impl exports::near::agent::tool::Guest for TavilyTool {
    fn execute(req: exports::near::agent::tool::Request) -> exports::near::agent::tool::Response {
        match execute_inner(&req.params) {
            Ok(result) => exports::near::agent::tool::Response {
                output: Some(result),
                error: None,
            },
            Err(e) => exports::near::agent::tool::Response {
                output: None,
                error: Some(e),
            },
        }
    }

    fn schema() -> String {
        SCHEMA.to_string()
    }

    fn description() -> String {
        "Search the web using Tavily Search API. Returns titles, URLs, snippets, \
         and optional answer synthesis for the query. Authentication is handled \
         via the 'tavily_api_key' secret injected by the host."
            .to_string()
    }
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    query: String,
    max_results: Option<u32>,
    search_depth: Option<String>,
    topic: Option<String>,
    include_answer: Option<bool>,
    include_raw_content: Option<bool>,
    include_domains: Option<Vec<String>>,
    exclude_domains: Option<Vec<String>>,
    days: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct TavilyResponse {
    answer: Option<String>,
    results: Option<Vec<TavilyResult>>,
    images: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TavilyResult {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
    score: Option<f64>,
    published_date: Option<String>,
}

fn execute_inner(params: &str) -> Result<String, String> {
    let params: SearchParams =
        serde_json::from_str(params).map_err(|e| format!("Invalid parameters: {e}"))?;

    let query = params.query.trim();

    if query.is_empty() {
        return Err("'query' must not be empty".into());
    }
    if query.len() > 2000 {
        return Err("'query' exceeds maximum length of 2000 characters".into());
    }

    let max_results = params
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS);
    let search_depth = params.search_depth.as_deref().unwrap_or("basic");
    if !matches!(search_depth, "basic" | "advanced") {
        return Err("Invalid 'search_depth': expected 'basic' or 'advanced'".into());
    }

    let topic = params.topic.as_deref().unwrap_or("general");
    if !matches!(topic, "general" | "news") {
        return Err("Invalid 'topic': expected 'general' or 'news'".into());
    }

    if let Some(days) = params.days {
        if days == 0 {
            return Err("Invalid 'days': expected integer >= 1".into());
        }

        if topic != "news" {
            return Err("Invalid 'days': only supported when topic='news'".into());
        }
    }

    validate_domain_filters(params.include_domains.as_deref(), "include_domains")?;
    validate_domain_filters(params.exclude_domains.as_deref(), "exclude_domains")?;

    if !near::agent::host::secret_exists("tavily_api_key") {
        return Err("Tavily API key not found in secret store. Set it with: \
             ironclaw secret set tavily_api_key <key>. \
             Get a key at: https://app.tavily.com/"
            .into());
    }

    let payload = build_payload(&params, max_results, search_depth, topic);
    let headers = serde_json::json!({
        "Accept": "application/json",
        "Content-Type": "application/json",
        "User-Agent": "IronClaw-Tavily-Tool/0.1"
    });

    let body_bytes = serde_json::to_vec(&payload)
        .map_err(|e| format!("Failed to serialize request payload: {e}"))?;

    let response = {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = near::agent::host::http_request(
                "POST",
                TAVILY_SEARCH_ENDPOINT,
                &headers.to_string(),
                Some(&body_bytes),
                None,
            )
            .map_err(|e| format!("HTTP request failed: {e}"))?;

            if resp.status >= 200 && resp.status < 300 {
                break resp;
            }

            if attempt < MAX_RETRIES && (resp.status == 429 || resp.status >= 500) {
                near::agent::host::log(
                    near::agent::host::LogLevel::Warn,
                    &format!(
                        "Tavily API error {} (attempt {}/{}). Retrying...",
                        resp.status, attempt, MAX_RETRIES
                    ),
                );
                continue;
            }

            let body = String::from_utf8_lossy(&resp.body);
            return Err(format!("Tavily API error (HTTP {}): {}", resp.status, body));
        }
    };

    let body =
        String::from_utf8(response.body).map_err(|e| format!("Invalid UTF-8 response: {e}"))?;
    let tavily: TavilyResponse =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse Tavily response: {e}"))?;

    let results = tavily.results.unwrap_or_default();
    let formatted: Vec<serde_json::Value> = results
        .into_iter()
        .filter_map(|r| {
            let title = r.title?;
            let url = r.url?;
            let description = r.content.unwrap_or_default();

            let mut entry = serde_json::json!({
                "title": title,
                "url": url,
                "description": description
            });

            if let Some(score) = r.score {
                entry["score"] = serde_json::json!(score);
            }
            if let Some(published) = r.published_date {
                entry["published"] = serde_json::json!(published);
            }
            Some(entry)
        })
        .collect();

    let output = serde_json::json!({
        "query": query,
        "answer": tavily.answer,
        "images": tavily.images.unwrap_or_default(),
        "result_count": formatted.len(),
        "results": formatted
    });

    serde_json::to_string(&output).map_err(|e| format!("Failed to serialize output: {e}"))
}

fn validate_domain_filters(domains: Option<&[String]>, field_name: &str) -> Result<(), String> {
    let Some(domains) = domains else {
        return Ok(());
    };

    for domain in domains {
        if domain.trim().is_empty() {
            return Err(format!(
                "Invalid '{field_name}': domain entries must not be empty"
            ));
        }
    }

    Ok(())
}

fn build_payload(
    params: &SearchParams,
    max_results: u32,
    search_depth: &str,
    topic: &str,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "query": params.query,
        "max_results": max_results,
        "search_depth": search_depth,
        "topic": topic,
        "include_answer": params.include_answer.unwrap_or(true),
        "include_raw_content": params.include_raw_content.unwrap_or(false),
    });

    if let Some(ref include_domains) = params.include_domains {
        if !include_domains.is_empty() {
            payload["include_domains"] = serde_json::json!(include_domains);
        }
    }
    if let Some(ref exclude_domains) = params.exclude_domains {
        if !exclude_domains.is_empty() {
            payload["exclude_domains"] = serde_json::json!(exclude_domains);
        }
    }
    if let Some(days) = params.days {
        payload["days"] = serde_json::json!(days);
    }

    payload
}

const SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "query": {
            "type": "string",
            "description": "Search query to look up on the web"
        },
        "max_results": {
            "type": "integer",
            "minimum": 1,
            "maximum": 20,
            "default": 5,
            "description": "Number of results to return"
        },
        "search_depth": {
            "type": "string",
            "enum": ["basic", "advanced"],
            "default": "basic",
            "description": "Search depth mode"
        },
        "topic": {
            "type": "string",
            "enum": ["general", "news"],
            "default": "general",
            "description": "Search topic mode"
        },
        "include_answer": {
            "type": "boolean",
            "default": true,
            "description": "Include answer synthesis in the response"
        },
        "include_raw_content": {
            "type": "boolean",
            "default": false,
            "description": "Include raw page content from Tavily"
        },
        "include_domains": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Restrict search to specific domains"
        },
        "exclude_domains": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Exclude specific domains from results"
        },
        "days": {
            "type": "integer",
            "minimum": 1,
            "description": "Limit results to the last N days (mainly for topic='news')"
        }
    },
    "required": ["query"],
    "additionalProperties": false
}"#;

export!(TavilyTool);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_payload_defaults() {
        let p = SearchParams {
            query: "rust wasm".to_string(),
            max_results: None,
            search_depth: None,
            topic: None,
            include_answer: None,
            include_raw_content: None,
            include_domains: None,
            exclude_domains: None,
            days: None,
        };

        let payload = build_payload(&p, 5, "basic", "general");
        assert_eq!(payload["query"], "rust wasm");
        assert_eq!(payload["max_results"], 5);
        assert_eq!(payload["search_depth"], "basic");
        assert_eq!(payload["topic"], "general");
        assert_eq!(payload["include_answer"], true);
        assert_eq!(payload["include_raw_content"], false);
        assert!(payload.get("days").is_none());
    }

    #[test]
    fn build_payload_optional_filters() {
        let p = SearchParams {
            query: "ai news".to_string(),
            max_results: Some(3),
            search_depth: Some("advanced".to_string()),
            topic: Some("news".to_string()),
            include_answer: Some(false),
            include_raw_content: Some(true),
            include_domains: Some(vec!["example.com".to_string()]),
            exclude_domains: Some(vec!["spam.com".to_string()]),
            days: Some(7),
        };

        let payload = build_payload(&p, 3, "advanced", "news");
        assert_eq!(payload["include_domains"][0], "example.com");
        assert_eq!(payload["exclude_domains"][0], "spam.com");
        assert_eq!(payload["days"], 7);
    }
}
