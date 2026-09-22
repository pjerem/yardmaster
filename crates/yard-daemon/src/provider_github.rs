//! GitHub Issues [`TicketProvider`] over the REST v3 API (`reqwest::blocking`).
//!
//! Blocked-by convention: GitHub REST exposes no native "blocked by" relation,
//! so yardmaster defines THE convention for GitHub tickets as body lines. A
//! line consisting of `Blocked by #N` (same repo) or `Blocked by owner/repo#N`
//! — "blocked by" matched case-insensitively — declares a dependency. Anything
//! else on the line disqualifies it.
//!
//! Ticket keys are `owner/repo#N`.

use reqwest::blocking::{Client, Response};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use yard_core::adapters::{
    AdapterError, AdapterResult, Secret, Ticket, TicketKey, TicketProvider, TrackerTransition,
};

/// Max response-body characters echoed into error messages.
const BODY_SNIPPET_LEN: usize = 200;

pub struct GithubProvider {
    /// Config name for this provider instance; becomes `TicketKey::provider`.
    provider: String,
    /// API root, e.g. `https://api.github.com` (tests point it at wiremock).
    base_url: String,
    token: Secret,
    /// GitHub login whose assigned issues `my_tickets` returns.
    user: String,
    /// `owner/name` repositories scanned by `my_tickets`.
    repos: Vec<String>,
    client: Client,
}

impl GithubProvider {
    pub fn new(
        provider: String,
        base_url: String,
        token: Secret,
        user: String,
        repos: Vec<String>,
    ) -> Self {
        Self {
            provider,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            user,
            repos,
            client: Client::new(),
        }
    }

    fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<&serde_json::Value>,
    ) -> AdapterResult<Response> {
        tracing::debug!(%method, url, "github provider request");
        let mut req = self
            .client
            .request(method.clone(), url)
            .header("Authorization", format!("Bearer {}", self.token.expose()))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "yardmaster");
        if let Some(json) = body {
            req = req.json(json);
        }
        let resp = req
            .send()
            .map_err(|e| AdapterError::Transient(format!("{method} {url}: {e}")))?;
        classify(&method, url, resp)
    }

    /// GET `url`, deserialize the body, and return the `rel="next"` Link (if any).
    fn get_page<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
    ) -> AdapterResult<(T, Option<String>)> {
        let resp = self.send(Method::GET, url, None)?;
        let next = resp
            .headers()
            .get("link")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_next_link);
        let value = resp
            .json::<T>()
            .map_err(|e| AdapterError::Permanent(format!("GET {url}: invalid JSON: {e}")))?;
        Ok((value, next))
    }

    fn to_ticket(&self, repo: &str, issue: IssueJson) -> Ticket {
        let body = issue.body.unwrap_or_default();
        Ticket {
            key: TicketKey {
                provider: self.provider.clone(),
                key: format!("{repo}#{}", issue.number),
            },
            title: issue.title,
            blocked_by: self.parse_blocked_by(&body, repo),
            body,
            status: issue.state,
            assignee: issue.assignee.map(|u| u.login),
            url: issue.html_url,
        }
    }

    /// Extract dependencies per the module-level blocked-by convention.
    fn parse_blocked_by(&self, body: &str, repo: &str) -> Vec<TicketKey> {
        const PREFIX: &str = "blocked by ";
        body.lines()
            .filter_map(|line| {
                let line = line.trim();
                let head = line.get(..PREFIX.len())?;
                if !head.eq_ignore_ascii_case(PREFIX) {
                    return None;
                }
                let rest = line[PREFIX.len()..].trim();
                let key = if let Some(num) = rest.strip_prefix('#') {
                    let n: u64 = num.parse().ok()?;
                    format!("{repo}#{n}")
                } else {
                    let (r, n) = split_ref(rest)?;
                    format!("{r}#{n}")
                };
                Some(TicketKey {
                    provider: self.provider.clone(),
                    key,
                })
            })
            .collect()
    }

    fn issue_url(&self, key: &TicketKey) -> AdapterResult<String> {
        let (repo, number) = split_ref(&key.key).ok_or_else(|| {
            AdapterError::Permanent(format!(
                "invalid GitHub ticket key {:?}: expected owner/repo#N",
                key.key
            ))
        })?;
        Ok(format!("{}/repos/{repo}/issues/{number}", self.base_url))
    }
}

impl TicketProvider for GithubProvider {
    fn my_tickets(&self) -> AdapterResult<Vec<Ticket>> {
        let mut out = Vec::new();
        for repo in &self.repos {
            let mut url = format!(
                "{}/repos/{repo}/issues?assignee={}&state=open&per_page=100",
                self.base_url, self.user
            );
            loop {
                let (issues, next): (Vec<IssueJson>, _) = self.get_page(&url)?;
                out.extend(
                    issues
                        .into_iter()
                        // The issues endpoint also lists PRs; skip them.
                        .filter(|i| i.pull_request.is_none())
                        .map(|i| self.to_ticket(repo, i)),
                );
                match next {
                    Some(n) => url = n,
                    None => break,
                }
            }
        }
        Ok(out)
    }

    fn get(&self, key: &TicketKey) -> AdapterResult<Ticket> {
        let url = self.issue_url(key)?;
        let (issue, _) = self.get_page::<IssueJson>(&url)?;
        // issue_url validated the key, so split_ref cannot fail here.
        let repo = key.key.split('#').next().unwrap_or_default();
        Ok(self.to_ticket(repo, issue))
    }

    fn available_transitions(&self, key: &TicketKey) -> AdapterResult<Vec<TrackerTransition>> {
        let ticket = self.get(key)?;
        Ok(match ticket.status.as_str() {
            "open" => vec![TrackerTransition {
                id: "close".to_string(),
                name: "Close".to_string(),
            }],
            "closed" => vec![TrackerTransition {
                id: "reopen".to_string(),
                name: "Reopen".to_string(),
            }],
            _ => Vec::new(),
        })
    }

    fn apply_transition(
        &self,
        key: &TicketKey,
        transition: &TrackerTransition,
    ) -> AdapterResult<()> {
        let new_state = match transition.id.as_str() {
            "close" => "closed",
            "reopen" => "open",
            other => {
                return Err(AdapterError::Permanent(format!(
                    "unknown GitHub transition id {other:?} (expected \"close\" or \"reopen\")"
                )));
            }
        };
        let url = self.issue_url(key)?;
        self.send(
            Method::PATCH,
            &url,
            Some(&serde_json::json!({ "state": new_state })),
        )?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct IssueJson {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    assignee: Option<UserJson>,
    html_url: String,
    /// Present on PR items returned by the issues endpoint; used to skip them.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct UserJson {
    login: String,
}

/// `owner/repo#N` → (`owner/repo`, N). None on any malformation.
fn split_ref(s: &str) -> Option<(&str, u64)> {
    let (repo, num) = s.split_once('#')?;
    let (owner, name) = repo.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some((repo, num.parse().ok()?))
}

/// Map HTTP status per the adapter contract: 5xx/429/rate-limited-403 are
/// Transient, other non-2xx are Permanent. The message carries a body snippet
/// but never the token (only status/URL/body are interpolated).
fn classify(method: &Method, url: &str, resp: Response) -> AdapterResult<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let rate_limited = status == StatusCode::FORBIDDEN
        && resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() == "0");
    let body = resp.text().unwrap_or_default();
    let msg = format!("{method} {url}: HTTP {status}: {}", snippet(&body));
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS || rate_limited {
        Err(AdapterError::Transient(msg))
    } else {
        Err(AdapterError::Permanent(msg))
    }
}

fn snippet(body: &str) -> String {
    match body.char_indices().nth(BODY_SNIPPET_LEN) {
        Some((idx, _)) => format!("{}…", &body[..idx]),
        None => body.to_string(),
    }
}

/// Extract the `rel="next"` target from a `Link` header, if present.
fn parse_next_link(header: &str) -> Option<String> {
    for part in header.split(',') {
        let mut segs = part.split(';');
        let Some(url) = segs.next() else { continue };
        if segs.any(|p| p.trim().eq_ignore_ascii_case(r#"rel="next""#)) {
            return Some(url.trim().strip_prefix('<')?.strip_suffix('>')?.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "s3cr3t-t0ken";

    /// wiremock is async; reqwest::blocking must run outside a runtime. A
    /// multi-thread runtime keeps the mock server polled on worker threads
    /// while the test thread issues blocking requests.
    struct TestServer {
        server: MockServer,
        rt: tokio::runtime::Runtime,
    }

    impl TestServer {
        fn start() -> Self {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let server = rt.block_on(MockServer::start());
            Self { server, rt }
        }

        fn mount(&self, mock: Mock) {
            self.rt.block_on(mock.mount(&self.server));
        }

        fn uri(&self) -> String {
            self.server.uri()
        }
    }

    fn provider(uri: &str, repos: &[&str]) -> GithubProvider {
        GithubProvider::new(
            "gh".to_string(),
            uri.to_string(),
            Secret::new(TOKEN.to_string()),
            "octocat".to_string(),
            repos.iter().map(|s| s.to_string()).collect(),
        )
    }

    fn key(k: &str) -> TicketKey {
        TicketKey {
            provider: "gh".to_string(),
            key: k.to_string(),
        }
    }

    fn issue_json(number: u64, state: &str, body: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "title": format!("Issue {number}"),
            "state": state,
            "body": body,
            "assignee": {"login": "octocat"},
            "html_url": format!("https://example.test/issues/{number}"),
        })
    }

    fn auth() -> wiremock::matchers::HeaderExactMatcher {
        header("authorization", format!("Bearer {TOKEN}"))
    }

    #[test]
    fn my_tickets_scans_repos_paginates_and_skips_prs() {
        let ts = TestServer::start();
        let uri = ts.uri();

        let mut pr_item = issue_json(2, "open", None);
        pr_item["pull_request"] = serde_json::json!({"url": "https://example.test/pr/2"});

        // Repo a/one, page 1: one issue + one PR item, Link to page 2.
        ts.mount(
            Mock::given(method("GET"))
                .and(path("/repos/a/one/issues"))
                .and(query_param("assignee", "octocat"))
                .and(query_param("state", "open"))
                .and(query_param("per_page", "100"))
                .and(auth())
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!([issue_json(1, "open", None), pr_item]))
                        .insert_header(
                            "link",
                            format!(
                                "<{uri}/repos/a/one/issues?page=2>; rel=\"next\", \
                                 <{uri}/repos/a/one/issues?page=2>; rel=\"last\""
                            )
                            .as_str(),
                        ),
                ),
        );
        // Repo a/one, page 2 (the Link URL carries no assignee param, so the
        // page-1 mock cannot match it).
        ts.mount(
            Mock::given(method("GET"))
                .and(path("/repos/a/one/issues"))
                .and(query_param("page", "2"))
                .and(auth())
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!([issue_json(3, "open", None)])),
                ),
        );
        // Repo b/two: single page.
        ts.mount(
            Mock::given(method("GET"))
                .and(path("/repos/b/two/issues"))
                .and(query_param("assignee", "octocat"))
                .and(auth())
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!([issue_json(7, "open", None)])),
                ),
        );

        let tickets = provider(&uri, &["a/one", "b/two"]).my_tickets().unwrap();
        let keys: Vec<&str> = tickets.iter().map(|t| t.key.key.as_str()).collect();
        assert_eq!(keys, ["a/one#1", "a/one#3", "b/two#7"]);
        assert!(tickets.iter().all(|t| t.key.provider == "gh"));
        assert!(tickets.iter().all(|t| t.status == "open"));
        assert!(
            tickets
                .iter()
                .all(|t| t.assignee.as_deref() == Some("octocat"))
        );
        // Null body → empty string, no blocked_by.
        assert_eq!(tickets[0].body, "");
        assert!(tickets[0].blocked_by.is_empty());
    }

    #[test]
    fn get_maps_issue_and_extracts_blocked_by() {
        let ts = TestServer::start();
        let body = "Intro line\n\
                    blocked BY #2\n\
                    Blocked by b/two#9\n\
                    BLOCKED BY x/y#3\n\
                    not Blocked by #4\n\
                    Blocked by nothing\n\
                    Blocked by #bad";
        let mut issue = issue_json(5, "open", Some(body));
        issue["assignee"] = serde_json::Value::Null;
        ts.mount(
            Mock::given(method("GET"))
                .and(path("/repos/a/one/issues/5"))
                .and(auth())
                .respond_with(ResponseTemplate::new(200).set_body_json(issue)),
        );

        let ticket = provider(&ts.uri(), &[]).get(&key("a/one#5")).unwrap();
        assert_eq!(ticket.key, key("a/one#5"));
        assert_eq!(ticket.title, "Issue 5");
        assert_eq!(ticket.body, body);
        assert_eq!(ticket.assignee, None);
        assert_eq!(ticket.url, "https://example.test/issues/5");
        assert_eq!(
            ticket.blocked_by,
            vec![key("a/one#2"), key("b/two#9"), key("x/y#3")]
        );
    }

    #[test]
    fn get_rejects_malformed_keys() {
        let p = provider("http://127.0.0.1:1", &[]);
        for bad in ["nonsense", "a/one#x", "one#5", "a/one", "/one#5", "a/#5"] {
            let err = p.get(&key(bad)).unwrap_err();
            assert!(
                matches!(err, AdapterError::Permanent(_)),
                "{bad}: expected Permanent, got {err}"
            );
        }
    }

    #[test]
    fn transitions_follow_issue_state() {
        let ts = TestServer::start();
        ts.mount(
            Mock::given(method("GET"))
                .and(path("/repos/a/one/issues/1"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(issue_json(1, "open", None)),
                ),
        );
        ts.mount(
            Mock::given(method("GET"))
                .and(path("/repos/a/one/issues/2"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(issue_json(2, "closed", None)),
                ),
        );

        let p = provider(&ts.uri(), &[]);
        let open = p.available_transitions(&key("a/one#1")).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "close");

        let closed = p.available_transitions(&key("a/one#2")).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, "reopen");
    }

    #[test]
    fn apply_transition_patches_issue_state() {
        let ts = TestServer::start();
        ts.mount(
            Mock::given(method("PATCH"))
                .and(path("/repos/a/one/issues/5"))
                .and(body_json(serde_json::json!({"state": "closed"})))
                .and(auth())
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(issue_json(5, "closed", None)),
                ),
        );
        ts.mount(
            Mock::given(method("PATCH"))
                .and(path("/repos/a/one/issues/6"))
                .and(body_json(serde_json::json!({"state": "open"})))
                .and(auth())
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(issue_json(6, "open", None)),
                ),
        );

        let p = provider(&ts.uri(), &[]);
        let t = |id: &str, name: &str| TrackerTransition {
            id: id.to_string(),
            name: name.to_string(),
        };
        p.apply_transition(&key("a/one#5"), &t("close", "Close"))
            .unwrap();
        p.apply_transition(&key("a/one#6"), &t("reopen", "Reopen"))
            .unwrap();

        let err = p
            .apply_transition(&key("a/one#5"), &t("wontfix", "Wontfix"))
            .unwrap_err();
        assert!(matches!(err, AdapterError::Permanent(_)));
    }

    #[test]
    fn http_errors_map_per_contract_and_never_leak_token() {
        let ts = TestServer::start();
        let mount_status = |n: u64, template: ResponseTemplate| {
            ts.mount(
                Mock::given(method("GET"))
                    .and(path(format!("/repos/e/one/issues/{n}")))
                    .and(auth())
                    .respond_with(template),
            );
        };
        mount_status(1, ResponseTemplate::new(500).set_body_string("boom"));
        mount_status(2, ResponseTemplate::new(404).set_body_string("gone"));
        mount_status(
            3,
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .set_body_string("rate limited"),
        );
        mount_status(4, ResponseTemplate::new(403).set_body_string("forbidden"));

        let p = provider(&ts.uri(), &[]);
        let err = |n: u64| p.get(&key(&format!("e/one#{n}"))).unwrap_err();

        let e500 = err(1);
        assert!(matches!(e500, AdapterError::Transient(_)), "{e500}");
        assert!(e500.to_string().contains("boom"));

        let e404 = err(2);
        assert!(matches!(e404, AdapterError::Permanent(_)), "{e404}");
        assert!(e404.to_string().contains("gone"));

        let e403rl = err(3);
        assert!(matches!(e403rl, AdapterError::Transient(_)), "{e403rl}");

        let e403 = err(4);
        assert!(matches!(e403, AdapterError::Permanent(_)), "{e403}");

        for e in [e500, e404, e403rl, e403] {
            assert!(
                !e.to_string().contains(TOKEN),
                "token leaked into error: {e}"
            );
        }
    }
}
