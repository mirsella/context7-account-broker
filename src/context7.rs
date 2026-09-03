#[cfg(test)]
use reqwest::header::AUTHORIZATION;
use reqwest::header::HeaderMap;
use reqwest::{Client, Request as HttpRequest, StatusCode};
use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_ENDPOINT: &str = "https://context7.com/api";
const API_TIMEOUT: Duration = Duration::from_secs(60);
const EMPTY_DOCS: &str = "Documentation not found or not finalized for this library. This might have happened because you used an invalid Context7-compatible library ID. To get a valid Context7-compatible library ID, use the 'resolve-library-id' with the package name you wish to retrieve documentation for.";

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "tool", rename_all = "kebab-case")]
pub(crate) enum Request {
    Search { query: String, library_name: String },
    Docs { query: String, library_id: String },
}

impl Request {
    pub(crate) fn subject(&self) -> &str {
        match self {
            Self::Search { library_name, .. } => library_name,
            Self::Docs { library_id, .. } => library_id,
        }
    }

    fn route(&self) -> (&'static str, [(&'static str, &str); 2]) {
        match self {
            Self::Search {
                query,
                library_name,
            } => (
                "v2/libs/search",
                [("query", query), ("libraryName", library_name)],
            ),
            Self::Docs { query, library_id } => {
                ("v2/context", [("query", query), ("libraryId", library_id)])
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Quota {
    pub(crate) limit: u64,
    pub(crate) remaining: u64,
    pub(crate) reset_at: SystemTime,
    pub(crate) blocked: bool,
}

pub(crate) struct Upstream {
    pub(crate) status: StatusCode,
    pub(crate) result: CallToolResult,
    pub(crate) quota: Option<Quota>,
    pub(crate) libraries: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum Error {
    Network(String),
    Invalid(String),
}

impl Error {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Network(message) | Self::Invalid(message) => message,
        }
    }
}

#[cfg(test)]
type SendFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Upstream, Error>> + Send + 'static>>;
#[cfg(test)]
type Sender = std::sync::Arc<dyn Fn(String, Request) -> SendFuture + Send + Sync>;

pub(crate) struct Context7 {
    client: Client,
    #[cfg(test)]
    sender: Option<Sender>,
}

impl Context7 {
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: Client::builder().timeout(API_TIMEOUT).build()?,
            #[cfg(test)]
            sender: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_sender<F, Fut>(sender: F) -> Self
    where
        F: Fn(String, Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Upstream, Error>> + Send + 'static,
    {
        Self {
            client: Client::new(),
            sender: Some(std::sync::Arc::new(move |key, request| {
                Box::pin(sender(key, request))
            })),
        }
    }

    pub(crate) async fn call(&self, api_key: &str, request: &Request) -> Result<Upstream, Error> {
        #[cfg(test)]
        if let Some(sender) = &self.sender {
            return sender(api_key.to_owned(), request.clone()).await;
        }

        let (status, headers, body) = self.send(api_key, request).await?;
        decode(request, status, &headers, &body)
    }

    pub(crate) async fn probe_quota(&self, api_key: &str) -> Result<Quota, Error> {
        let request = Request::Search {
            query: "quota status check".to_owned(),
            library_name: "Context7".to_owned(),
        };
        let (status, headers, _) = self.send(api_key, &request).await?;
        if status != StatusCode::OK && status != StatusCode::TOO_MANY_REQUESTS {
            return Err(Error::Invalid(format!(
                "quota check returned HTTP {status} without valid quota headers"
            )));
        }
        quota_from_headers(&headers, status)
            .map_err(Error::Invalid)?
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "quota check returned HTTP {status} without valid quota headers"
                ))
            })
    }

    fn build_request(&self, api_key: &str, request: &Request) -> Result<HttpRequest, Error> {
        let (path, query) = request.route();
        self.client
            .get(format!("{API_ENDPOINT}/{path}"))
            .query(&query)
            .bearer_auth(api_key)
            .build()
            .map_err(|error| Error::Invalid(error.to_string()))
    }

    async fn send(
        &self,
        api_key: &str,
        request: &Request,
    ) -> Result<(StatusCode, HeaderMap, String), Error> {
        let response = self
            .client
            .execute(self.build_request(api_key, request)?)
            .await
            .map_err(|error| Error::Network(error.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .map_err(|error| Error::Network(error.to_string()))?;
        Ok((status, headers, body))
    }
}

pub(crate) fn quota_summary(quota: &Quota) -> String {
    let used = quota.limit - quota.remaining;
    let percentage = (used.saturating_mul(100) + quota.limit / 2) / quota.limit;
    let reset = quota
        .reset_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!(
        "{used}/{limit} used ({percentage}%){}; resets {}",
        if quota.blocked { ", blocked" } else { "" },
        format_timestamp(reset),
        limit = quota.limit
    )
}

fn decode(
    request: &Request,
    status: StatusCode,
    headers: &HeaderMap,
    body: &str,
) -> Result<Upstream, Error> {
    let quota = match quota_from_headers(headers, status) {
        Ok(quota) => quota,
        Err(error) => {
            eprintln!("warning: {error}; ignoring quota metadata");
            None
        }
    };
    if !status.is_success() {
        return Ok(Upstream {
            status,
            result: CallToolResult::error(vec![ContentBlock::text(error_message(body, status))]),
            quota,
            libraries: Vec::new(),
        });
    }

    match request {
        Request::Docs { .. } => Ok(Upstream {
            status,
            result: CallToolResult::success(vec![ContentBlock::text(if body.is_empty() {
                EMPTY_DOCS
            } else {
                body
            })]),
            quota,
            libraries: Vec::new(),
        }),
        Request::Search { .. } => {
            let search: SearchResponse = serde_json::from_str(body).map_err(|_| {
                Error::Invalid("Context7 returned an invalid library search response".to_owned())
            })?;
            let libraries = search
                .results
                .iter()
                .map(|result| result.id.to_lowercase())
                .collect();
            let text = if search.results.is_empty() {
                search
                    .error
                    .unwrap_or_else(|| "No libraries found matching the provided name.".to_owned())
            } else {
                format!("Available Libraries:\n\n{}", format_search_results(&search))
            };
            Ok(Upstream {
                status,
                result: CallToolResult::success(vec![ContentBlock::text(text)]),
                quota,
                libraries,
            })
        }
    }
}

fn quota_from_headers(headers: &HeaderMap, status: StatusCode) -> Result<Option<Quota>, String> {
    let values = [
        headers.get("RateLimit-Limit"),
        headers.get("RateLimit-Remaining"),
        headers.get("RateLimit-Reset"),
    ];
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    let parse = |value: Option<&reqwest::header::HeaderValue>| {
        value
            .ok_or(())?
            .to_str()
            .map_err(|_| ())?
            .trim()
            .parse::<u64>()
            .map_err(|_| ())
    };
    let [limit, remaining, reset] = values.map(parse);
    let (limit, remaining, reset) = match (limit, remaining, reset) {
        (Ok(limit), Ok(remaining), Ok(reset)) if limit > 0 && remaining <= limit && reset > 0 => {
            (limit, remaining, reset)
        }
        _ => return Err("Context7 returned invalid rate-limit headers".to_owned()),
    };
    let reset_at = UNIX_EPOCH
        .checked_add(Duration::from_secs(reset))
        .ok_or_else(|| "Context7 returned invalid rate-limit headers".to_owned())?;
    Ok(Some(Quota {
        limit,
        remaining,
        reset_at,
        blocked: status == StatusCode::TOO_MANY_REQUESTS,
    }))
}

fn error_message(body: &str, status: StatusCode) -> String {
    if let Ok(value) = serde_json::from_str::<ErrorResponse>(body)
        && let Some(message) = value.message
    {
        return message;
    }
    match status {
        StatusCode::TOO_MANY_REQUESTS => "Rate limited or quota exceeded.".to_owned(),
        StatusCode::NOT_FOUND => "The requested library does not exist.".to_owned(),
        StatusCode::UNAUTHORIZED => "Invalid Context7 API key.".to_owned(),
        _ => format!("Request failed with status {}.", status.as_u16()),
    }
}

fn format_search_results(search: &SearchResponse) -> String {
    let results = search
        .results
        .iter()
        .map(format_search_result)
        .collect::<Vec<_>>()
        .join("\n----------\n");
    if search.search_filter_applied {
        format!(
            "**Note:** Your results only include libraries matching your teamspace's library filters. To adjust quality thresholds or blocked libraries, update your filters at https://context7.com/dashboard?tab=policies\n\n{results}"
        )
    } else {
        results
    }
}

fn format_search_result(result: &SearchResult) -> String {
    let mut lines = vec![
        format!("- Title: {}", result.title),
        format!("- Context7-compatible library ID: {}", result.id),
        format!("- Description: {}", result.description),
    ];
    if let Some(value) = result.total_snippets.filter(|value| *value != -1) {
        lines.push(format!("- Code Snippets: {value}"));
    }
    let reputation = match result.trust_score {
        None => "Unknown",
        Some(value) if value < 0.0 => "Unknown",
        Some(value) if value >= 7.0 => "High",
        Some(value) if value >= 4.0 => "Medium",
        Some(_) => "Low",
    };
    lines.push(format!("- Source Reputation: {reputation}"));
    if let Some(value) = result.benchmark_score.filter(|value| *value > 0.0) {
        lines.push(format!("- Benchmark Score: {value}"));
    }
    if !result.versions.is_empty() {
        lines.push(format!("- Versions: {}", result.versions.join(", ")));
    }
    if !result.source.is_empty() {
        lines.push(format!("- Source: {}", result.source));
    }
    lines.join("\n")
}

fn format_timestamp(seconds: u64) -> String {
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
        day_seconds / 3600,
        (day_seconds % 3600) / 60,
        day_seconds % 60
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[derive(Deserialize)]
struct ErrorResponse {
    message: Option<String>,
}

#[derive(Deserialize)]
struct SearchResponse {
    error: Option<String>,
    results: Vec<SearchResult>,
    #[serde(rename = "searchFilterApplied", default)]
    search_filter_applied: bool,
}

#[derive(Deserialize)]
struct SearchResult {
    id: String,
    title: String,
    description: String,
    #[serde(rename = "totalSnippets")]
    total_snippets: Option<i64>,
    #[serde(rename = "trustScore")]
    trust_score: Option<f64>,
    #[serde(rename = "benchmarkScore")]
    benchmark_score: Option<f64>,
    #[serde(default)]
    versions: Vec<String>,
    #[serde(default)]
    source: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn quota_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("RateLimit-Limit", "100".parse().unwrap());
        headers.insert("RateLimit-Remaining", "91".parse().unwrap());
        headers.insert("RateLimit-Reset", "4102444800".parse().unwrap());
        headers
    }

    #[test]
    fn builds_canonical_authenticated_requests() {
        let context7 = Context7::new().unwrap();
        let request = context7
            .build_request(
                "ctx7sk-test",
                &Request::Search {
                    query: "hooks for react".to_owned(),
                    library_name: "Next.js".to_owned(),
                },
            )
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://context7.com/api/v2/libs/search?query=hooks+for+react&libraryName=Next.js"
        );
        assert_eq!(request.headers()[AUTHORIZATION], "Bearer ctx7sk-test");
    }

    #[test]
    fn decodes_official_search_format_and_structured_affinity() {
        let body = json!({
            "searchFilterApplied": true,
            "results": [{
                "id": "/vercel/next.js",
                "title": "Next.js",
                "description": "The React framework",
                "totalSnippets": 12,
                "trustScore": 8,
                "benchmarkScore": 1.5,
                "versions": ["15", "14"],
                "source": "github"
            }]
        })
        .to_string();
        let response = decode(
            &Request::Search {
                query: "hooks".to_owned(),
                library_name: "Next.js".to_owned(),
            },
            StatusCode::OK,
            &quota_headers(),
            &body,
        )
        .unwrap();
        let text = &response.result.content[0].as_text().unwrap().text;
        assert!(text.starts_with("Available Libraries:\n\n**Note:**"));
        assert!(text.contains("- Source Reputation: High"));
        assert!(text.contains("- Versions: 15, 14"));
        assert_eq!(response.libraries, ["/vercel/next.js"]);
        assert_eq!(response.quota.unwrap().remaining, 91);
    }

    #[test]
    fn malformed_optional_quota_does_not_discard_content() {
        let mut headers = quota_headers();
        headers.insert("RateLimit-Remaining", "101".parse().unwrap());
        let response = decode(
            &Request::Docs {
                query: "hooks".to_owned(),
                library_id: "/vercel/next.js".to_owned(),
            },
            StatusCode::OK,
            &headers,
            "docs",
        )
        .unwrap();
        assert!(response.quota.is_none());
        assert_eq!(response.result.content[0].as_text().unwrap().text, "docs");
    }
}
