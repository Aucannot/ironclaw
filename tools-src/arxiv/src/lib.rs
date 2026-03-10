//! arXiv Search WASM Tool for IronClaw.
//!
//! Queries arXiv Atom API and returns structured paper results.

wit_bindgen::generate!({
    world: "sandboxed-tool",
    path: "../../wit/tool.wit",
});

use serde::Deserialize;

const ARXIV_API_ENDPOINT: &str = "https://export.arxiv.org/api/query";
const DEFAULT_MAX_RESULTS: u32 = 5;
const MAX_RESULTS: u32 = 25;
const DEFAULT_START: u32 = 0;
const MAX_RETRIES: u32 = 3;

struct ArxivTool;

impl exports::near::agent::tool::Guest for ArxivTool {
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
        "Search arXiv papers by keyword, title, author, abstract, or category. \
         Returns paper metadata including title, authors, summary, publication \
         date, primary category, and links."
            .to_string()
    }
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    query: String,
    max_results: Option<u32>,
    start: Option<u32>,
    sort_by: Option<String>,
    sort_order: Option<String>,
}

fn execute_inner(params: &str) -> Result<String, String> {
    let params: SearchParams =
        serde_json::from_str(params).map_err(|e| format!("Invalid parameters: {e}"))?;

    if params.query.trim().is_empty() {
        return Err("'query' must not be empty".to_string());
    }
    if params.query.len() > 2000 {
        return Err("'query' exceeds maximum length of 2000 characters".to_string());
    }

    let max_results = params.max_results.unwrap_or(DEFAULT_MAX_RESULTS).clamp(1, MAX_RESULTS);
    let start = params.start.unwrap_or(DEFAULT_START);
    let sort_by = params.sort_by.unwrap_or_else(|| "relevance".to_string());
    let sort_order = params
        .sort_order
        .unwrap_or_else(|| "descending".to_string());

    if !matches!(
        sort_by.as_str(),
        "relevance" | "lastUpdatedDate" | "submittedDate"
    ) {
        return Err(
            "Invalid 'sort_by': expected 'relevance', 'lastUpdatedDate', or 'submittedDate'"
                .to_string(),
        );
    }

    if !matches!(sort_order.as_str(), "ascending" | "descending") {
        return Err("Invalid 'sort_order': expected 'ascending' or 'descending'".to_string());
    }

    let url = format!(
        "{base}?search_query={query}&start={start}&max_results={max_results}&sortBy={sort_by}&sortOrder={sort_order}",
        base = ARXIV_API_ENDPOINT,
        query = url_encode(&params.query),
        start = start,
        max_results = max_results,
        sort_by = url_encode(&sort_by),
        sort_order = url_encode(&sort_order)
    );

    let headers = serde_json::json!({
        "Accept": "application/atom+xml",
        "User-Agent": "IronClaw-arXiv-Tool/0.1"
    });

    let response = {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = near::agent::host::http_request("GET", &url, &headers.to_string(), None, None)
                .map_err(|e| format!("HTTP request failed: {e}"))?;

            if resp.status >= 200 && resp.status < 300 {
                break resp;
            }

            if attempt < MAX_RETRIES && (resp.status == 429 || resp.status >= 500) {
                near::agent::host::log(
                    near::agent::host::LogLevel::Warn,
                    &format!(
                        "arXiv API error {} (attempt {}/{}). Retrying...",
                        resp.status, attempt, MAX_RETRIES
                    ),
                );
                continue;
            }

            let body = String::from_utf8_lossy(&resp.body);
            return Err(format!("arXiv API error (HTTP {}): {}", resp.status, body));
        }
    };

    let body =
        String::from_utf8(response.body).map_err(|e| format!("Invalid UTF-8 response: {e}"))?;

    let total_results = parse_total_results(&body).unwrap_or(0);
    let entries = parse_entries(&body);

    let output = serde_json::json!({
        "query": params.query,
        "start": start,
        "max_results": max_results,
        "total_results": total_results,
        "result_count": entries.len(),
        "results": entries
    });

    serde_json::to_string(&output).map_err(|e| format!("Failed to serialize output: {e}"))
}

fn parse_total_results(xml: &str) -> Option<u32> {
    extract_tag_text(xml, "opensearch:totalResults")
        .and_then(|s| s.trim().parse::<u32>().ok())
}

fn parse_entries(xml: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for chunk in xml.split("<entry>").skip(1) {
        let Some(entry_xml) = chunk.split("</entry>").next() else {
            continue;
        };

        let id = extract_tag_text(entry_xml, "id").map(|s| decode_xml_entities(&s));
        let title = extract_tag_text(entry_xml, "title")
            .map(|s| collapse_ws(&decode_xml_entities(&s)))
            .unwrap_or_default();
        let summary = extract_tag_text(entry_xml, "summary")
            .map(|s| collapse_ws(&decode_xml_entities(&s)))
            .unwrap_or_default();
        let published = extract_tag_text(entry_xml, "published").map(|s| decode_xml_entities(&s));
        let updated = extract_tag_text(entry_xml, "updated").map(|s| decode_xml_entities(&s));
        let primary_category = extract_primary_category(entry_xml).map(|s| decode_xml_entities(&s));
        let categories = extract_categories(entry_xml);
        let authors = extract_authors(entry_xml);
        let pdf_url = extract_link_href(entry_xml, "related")
            .or_else(|| extract_link_href(entry_xml, "alternate"));

        let mut paper = serde_json::json!({
            "title": title,
            "summary": summary,
            "authors": authors,
            "categories": categories
        });

        if let Some(v) = id {
            paper["id"] = serde_json::json!(v);
        }
        if let Some(v) = published {
            paper["published"] = serde_json::json!(v);
        }
        if let Some(v) = updated {
            paper["updated"] = serde_json::json!(v);
        }
        if let Some(v) = primary_category {
            paper["primary_category"] = serde_json::json!(v);
        }
        if let Some(v) = pdf_url {
            paper["url"] = serde_json::json!(v);
        }

        out.push(paper);
    }

    out
}

fn extract_tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

fn extract_primary_category(xml: &str) -> Option<String> {
    let marker = "<arxiv:primary_category";
    let start = xml.find(marker)?;
    let tail = &xml[start..];
    extract_attr(tail, "term")
}

fn extract_categories(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    let marker = "<category";

    while let Some(pos) = rest.find(marker) {
        rest = &rest[pos + marker.len()..];
        if let Some(term) = extract_attr(rest, "term") {
            out.push(decode_xml_entities(&term));
        }
    }

    out.sort();
    out.dedup();
    out
}

fn extract_authors(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in xml.split("<author>").skip(1) {
        let Some(author_xml) = chunk.split("</author>").next() else {
            continue;
        };
        if let Some(name) = extract_tag_text(author_xml, "name") {
            out.push(collapse_ws(&decode_xml_entities(&name)));
        }
    }
    out
}

fn extract_link_href(xml: &str, rel: &str) -> Option<String> {
    let marker = "<link ";
    let mut rest = xml;
    while let Some(pos) = rest.find(marker) {
        rest = &rest[pos..];
        let end = rest.find('>')?;
        let tag = &rest[..end];
        let tag_rel = extract_attr(tag, "rel");
        let href = extract_attr(tag, "href");
        if tag_rel.as_deref() == Some(rel) {
            return href.map(|s| decode_xml_entities(&s));
        }
        rest = &rest[end + 1..];
    }
    None
}

fn extract_attr(tag_fragment: &str, attr: &str) -> Option<String> {
    let key = format!(r#"{attr}=""#, attr = attr);
    let start = tag_fragment.find(&key)? + key.len();
    let end = tag_fragment[start..].find('"')? + start;
    Some(tag_fragment[start..end].to_string())
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_xml_entities(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => {
                out.push('%');
                out.push(char::from(b"0123456789ABCDEF"[(b >> 4) as usize]));
                out.push(char::from(b"0123456789ABCDEF"[(b & 0xf) as usize]));
            }
        }
    }
    out
}

const SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "arXiv search query (supports plain text or arXiv query syntax like ti:, au:, abs:, cat:)"
    },
    "max_results": {
      "type": "integer",
      "minimum": 1,
      "maximum": 25,
      "default": 5,
      "description": "Number of papers to return"
    },
    "start": {
      "type": "integer",
      "minimum": 0,
      "default": 0,
      "description": "Zero-based offset for pagination"
    },
    "sort_by": {
      "type": "string",
      "enum": ["relevance", "lastUpdatedDate", "submittedDate"],
      "default": "relevance",
      "description": "Sort strategy used by arXiv"
    },
    "sort_order": {
      "type": "string",
      "enum": ["ascending", "descending"],
      "default": "descending",
      "description": "Sort direction"
    }
  },
  "required": ["query"],
  "additionalProperties": false
}"#;

export!(ArxivTool);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_atom_feed_entry() {
        let xml = r#"
<feed>
  <opensearch:totalResults>1</opensearch:totalResults>
  <entry>
    <id>http://arxiv.org/abs/2501.00001v1</id>
    <updated>2025-01-02T10:00:00Z</updated>
    <published>2025-01-01T10:00:00Z</published>
    <title>  Test Paper  </title>
    <summary>  This is a summary. </summary>
    <author><name>Alice</name></author>
    <author><name>Bob</name></author>
    <arxiv:primary_category term="cs.AI" />
    <category term="cs.AI" />
    <category term="stat.ML" />
    <link rel="alternate" href="http://arxiv.org/abs/2501.00001v1" />
  </entry>
</feed>
"#;

        assert_eq!(parse_total_results(xml), Some(1));
        let entries = parse_entries(xml);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["title"], "Test Paper");
        assert_eq!(entries[0]["authors"][0], "Alice");
        assert_eq!(entries[0]["primary_category"], "cs.AI");
        assert_eq!(entries[0]["categories"][1], "stat.ML");
        assert_eq!(entries[0]["url"], "http://arxiv.org/abs/2501.00001v1");
    }
}
