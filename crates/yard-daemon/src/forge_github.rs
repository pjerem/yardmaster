//! GitHub `Forge` adapter over the REST API (blocking reqwest).
//!
//! Adapters only execute already-approved actions; 🔴 gating happens in core.
//! Error mapping per the adapter contract: network / 5xx / 429 / rate-limited
//! 403 → `Transient`; other 4xx, auth, and payload parse failures →
//! `Permanent`. The token travels exclusively in the `Authorization` header
//! and never appears in error messages.

use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, LINK};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use yard_core::adapters::{
    AdapterError, AdapterResult, CheckRun, Forge, MergeMethod, PrComment, PrDraft, PrRef,
    PrSnapshot, Review, Secret,
};

const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "yardmaster";
const PER_PAGE: u32 = 100;
/// GitHub's placeholder login for deleted accounts (`user: null` in payloads).
const GHOST_LOGIN: &str = "ghost";

pub struct GithubForge {
    client: Client,
    base_url: String,
    token: Secret,
}

impl GithubForge {
    /// `base_url` is `https://api.github.com` in production and the wiremock
    /// URI in tests. A trailing slash is tolerated.
    pub fn new(base_url: String, token: Secret) -> AdapterResult<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| AdapterError::Permanent(format!("building http client: {e}")))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Attaches the standard GitHub headers, sends, and classifies failures.
    /// `ctx` names the operation in error messages (never the token).
    fn send(&self, req: RequestBuilder, ctx: &str) -> AdapterResult<Response> {
        let resp = req
            .header(AUTHORIZATION, format!("Bearer {}", self.token.expose()))
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .map_err(|e| AdapterError::Transient(format!("network error during {ctx}: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        // Secondary rate limits surface as 403 with an exhausted quota header.
        let rate_limited = status == StatusCode::FORBIDDEN
            && resp
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                == Some("0");
        let body = resp.text().unwrap_or_default();
        let message = serde_json::from_str::<ErrorBody>(&body)
            .map(|b| b.message)
            .unwrap_or(body);
        let detail = format!("github {status} during {ctx}: {message}");
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS || rate_limited {
            Err(AdapterError::Transient(detail))
        } else {
            Err(AdapterError::Permanent(detail))
        }
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str, ctx: &str) -> AdapterResult<T> {
        let resp = self.send(self.client.get(self.url(path)), ctx)?;
        parse(resp, ctx)
    }

    /// GETs every page of a list endpoint, following `Link: rel="next"`.
    /// `wrapper` unwraps object-enveloped lists (e.g. `{"check_runs": [...]}`).
    fn get_paged<T: DeserializeOwned>(
        &self,
        path: &str,
        wrapper: Option<&str>,
        ctx: &str,
    ) -> AdapterResult<Vec<T>> {
        let mut items = Vec::new();
        let mut next = Some(format!("{}?per_page={PER_PAGE}", self.url(path)));
        while let Some(page_url) = next {
            let resp = self.send(self.client.get(page_url), ctx)?;
            next = next_link(resp.headers());
            let value: serde_json::Value = parse(resp, ctx)?;
            let list = match wrapper {
                Some(key) => value.get(key).cloned(),
                None => Some(value),
            };
            let Some(serde_json::Value::Array(page)) = list else {
                return Err(AdapterError::Permanent(format!(
                    "unexpected {ctx} payload shape"
                )));
            };
            for item in page {
                items
                    .push(serde_json::from_value(item).map_err(|e| {
                        AdapterError::Permanent(format!("parsing {ctx} item: {e}"))
                    })?);
            }
        }
        Ok(items)
    }
}

impl Forge for GithubForge {
    fn create_pr(&self, draft: &PrDraft) -> AdapterResult<PrRef> {
        let path = format!("/repos/{}/pulls", draft.repo);
        let body = json!({
            "title": draft.title,
            "body": draft.body,
            "base": draft.base,
            "head": draft.head,
            "draft": draft.draft,
        });
        let resp = self.send(self.client.post(self.url(&path)).json(&body), "create_pr")?;
        let created: PullCreated = parse(resp, "create_pr")?;
        Ok(PrRef {
            repo: draft.repo.clone(),
            number: created.number,
        })
    }

    fn pr_snapshot(&self, pr: &PrRef) -> AdapterResult<PrSnapshot> {
        let repo = &pr.repo;
        let pull: Pull = self.get_json(
            &format!("/repos/{repo}/pulls/{}", pr.number),
            "pr_snapshot pull",
        )?;

        let checks: Vec<ApiCheckRun> = self.get_paged(
            &format!("/repos/{repo}/commits/{}/check-runs", pull.head.sha),
            Some("check_runs"),
            "pr_snapshot check-runs",
        )?;
        let reviews: Vec<ApiReview> = self.get_paged(
            &format!("/repos/{repo}/pulls/{}/reviews", pr.number),
            None,
            "pr_snapshot reviews",
        )?;
        let mut comments: Vec<ApiComment> = self.get_paged(
            &format!("/repos/{repo}/issues/{}/comments", pr.number),
            None,
            "pr_snapshot issue comments",
        )?;
        comments.extend(self.get_paged::<ApiComment>(
            &format!("/repos/{repo}/pulls/{}/comments", pr.number),
            None,
            "pr_snapshot review comments",
        )?);
        // ISO-8601 timestamps sort lexicographically; stable sort keeps
        // issue-comments before review-comments on equal timestamps.
        comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        let state = if pull.merged_at.is_some() {
            "merged".to_string()
        } else {
            pull.state
        };
        Ok(PrSnapshot {
            pr: pr.clone(),
            state,
            draft: pull.draft,
            base: pull.base.name,
            head_sha: pull.head.sha,
            mergeable: pull.mergeable,
            checks: checks
                .into_iter()
                .map(ApiCheckRun::into_check_run)
                .collect(),
            reviews: reviews.into_iter().map(ApiReview::into_review).collect(),
            comments: comments.into_iter().map(ApiComment::into_comment).collect(),
        })
    }

    fn post_comment(&self, pr: &PrRef, body: &str) -> AdapterResult<()> {
        let path = format!("/repos/{}/issues/{}/comments", pr.repo, pr.number);
        self.send(
            self.client
                .post(self.url(&path))
                .json(&json!({ "body": body })),
            "post_comment",
        )?;
        Ok(())
    }

    fn merge(&self, pr: &PrRef, method: MergeMethod) -> AdapterResult<()> {
        let merge_method = match method {
            MergeMethod::Merge => "merge",
            MergeMethod::Squash => "squash",
            MergeMethod::Rebase => "rebase",
        };
        let path = format!("/repos/{}/pulls/{}/merge", pr.repo, pr.number);
        // 405 (not mergeable) / 409 (head moved) map to Permanent with
        // GitHub's message via the generic classification in `send`.
        self.send(
            self.client
                .put(self.url(&path))
                .json(&json!({ "merge_method": merge_method })),
            "merge",
        )?;
        Ok(())
    }
}

fn parse<T: DeserializeOwned>(resp: Response, ctx: &str) -> AdapterResult<T> {
    resp.json()
        .map_err(|e| AdapterError::Permanent(format!("parsing {ctx} response: {e}")))
}

/// Extracts the `rel="next"` target from a `Link` header, if any.
fn next_link(headers: &HeaderMap) -> Option<String> {
    let link = headers.get(LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        params.contains(r#"rel="next""#).then(|| {
            target
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

// ---------------------------------------------------------------------------
// REST payload shapes (only the fields we consume)

#[derive(Deserialize)]
struct ErrorBody {
    message: String,
}

#[derive(Deserialize)]
struct PullCreated {
    number: u64,
}

#[derive(Deserialize)]
struct Pull {
    state: String,
    merged_at: Option<String>,
    #[serde(default)]
    draft: bool,
    base: RefName,
    head: HeadRef,
    mergeable: Option<bool>,
}

#[derive(Deserialize)]
struct RefName {
    #[serde(rename = "ref")]
    name: String,
}

#[derive(Deserialize)]
struct HeadRef {
    sha: String,
}

#[derive(Deserialize)]
struct ApiCheckRun {
    name: String,
    status: String,
    conclusion: Option<String>,
    html_url: Option<String>,
}

impl ApiCheckRun {
    fn into_check_run(self) -> CheckRun {
        CheckRun {
            name: self.name,
            status: self.status,
            conclusion: self.conclusion,
            url: self.html_url,
        }
    }
}

#[derive(Deserialize)]
struct ApiUser {
    login: String,
    #[serde(rename = "type")]
    kind: Option<String>,
}

impl ApiUser {
    fn is_bot(&self) -> bool {
        self.kind.as_deref() == Some("Bot") || self.login.ends_with("[bot]")
    }
}

#[derive(Deserialize)]
struct ApiReview {
    user: Option<ApiUser>,
    state: String,
}

impl ApiReview {
    fn into_review(self) -> Review {
        Review {
            author: self
                .user
                .map_or_else(|| GHOST_LOGIN.to_string(), |u| u.login),
            state: self.state,
        }
    }
}

#[derive(Deserialize)]
struct ApiComment {
    id: u64,
    user: Option<ApiUser>,
    body: Option<String>,
    created_at: String,
}

impl ApiComment {
    fn into_comment(self) -> PrComment {
        let is_bot = self.user.as_ref().is_some_and(ApiUser::is_bot);
        PrComment {
            id: self.id,
            author: self
                .user
                .map_or_else(|| GHOST_LOGIN.to_string(), |u| u.login),
            body: self.body.unwrap_or_default(),
            is_bot,
            created_at: self.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Shared multi-thread runtime: wiremock serves from its worker threads
    /// while the blocking reqwest client runs on the test thread.
    static RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    });

    fn start_server() -> MockServer {
        RT.block_on(MockServer::start())
    }

    fn mount(server: &MockServer, mock: Mock) {
        RT.block_on(mock.mount(server));
    }

    fn forge(server: &MockServer) -> GithubForge {
        GithubForge::new(server.uri(), Secret::new("test-token-xyz".into())).expect("forge")
    }

    fn pr7() -> PrRef {
        PrRef {
            repo: "acme/widgets".into(),
            number: 7,
        }
    }

    #[test]
    fn create_pr_posts_draft_and_returns_ref() {
        let server = start_server();
        mount(
            &server,
            Mock::given(method("POST"))
                .and(path("/repos/acme/widgets/pulls"))
                .and(header("authorization", "Bearer test-token-xyz"))
                .and(header("accept", "application/vnd.github+json"))
                .and(header("x-github-api-version", "2022-11-28"))
                .and(body_json(json!({
                    "title": "Add gizmo",
                    "body": "does gizmo",
                    "base": "main",
                    "head": "feature/gizmo",
                    "draft": true,
                })))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "number": 42 })))
                .expect(1),
        );
        let draft = PrDraft {
            repo: "acme/widgets".into(),
            title: "Add gizmo".into(),
            body: "does gizmo".into(),
            base: "main".into(),
            head: "feature/gizmo".into(),
            draft: true,
        };
        let pr = forge(&server).create_pr(&draft).expect("create_pr");
        assert_eq!(
            pr,
            PrRef {
                repo: "acme/widgets".into(),
                number: 42
            }
        );
    }

    #[test]
    fn create_pr_422_is_permanent_and_never_leaks_token() {
        let server = start_server();
        mount(
            &server,
            Mock::given(method("POST"))
                .and(path("/repos/acme/widgets/pulls"))
                .respond_with(
                    ResponseTemplate::new(422)
                        .set_body_json(json!({ "message": "Validation Failed" })),
                ),
        );
        let draft = PrDraft {
            repo: "acme/widgets".into(),
            title: "t".into(),
            body: "b".into(),
            base: "main".into(),
            head: "h".into(),
            draft: false,
        };
        match forge(&server).create_pr(&draft) {
            Err(AdapterError::Permanent(msg)) => {
                assert!(msg.contains("Validation Failed"), "message lost: {msg}");
                assert!(!msg.contains("test-token-xyz"), "token leaked: {msg}");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_assembles_all_endpoints_with_check_run_pagination() {
        let server = start_server();
        mount(
            &server,
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/pulls/7"))
                .and(header("authorization", "Bearer test-token-xyz"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "state": "open",
                    "merged_at": null,
                    "draft": true,
                    "base": { "ref": "main" },
                    "head": { "sha": "abc123" },
                    "mergeable": null,
                }))),
        );
        // Check-runs page 1 (first request carries per_page) links to page 2.
        let next = format!(
            "<{}/repos/acme/widgets/commits/abc123/check-runs?page=2>; rel=\"next\"",
            server.uri()
        );
        mount(
            &server,
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/commits/abc123/check-runs"))
                .and(query_param("per_page", "100"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("link", next.as_str())
                        .set_body_json(json!({
                            "check_runs": [{
                                "name": "ci",
                                "status": "completed",
                                "conclusion": "success",
                                "html_url": "https://example.test/ci",
                            }],
                        })),
                ),
        );
        mount(
            &server,
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/commits/abc123/check-runs"))
                .and(query_param("page", "2"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "check_runs": [{
                        "name": "lint",
                        "status": "in_progress",
                        "conclusion": null,
                        "html_url": null,
                    }],
                }))),
        );
        mount(
            &server,
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/pulls/7/reviews"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                    { "user": { "login": "alice", "type": "User" }, "state": "APPROVED" },
                ]))),
        );
        // Issue comment: bot by `[bot]` login suffix, later timestamp.
        mount(
            &server,
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/issues/7/comments"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                    "id": 2,
                    "user": { "login": "dependabot[bot]", "type": "User" },
                    "body": "bump deps",
                    "created_at": "2026-01-02T00:00:00Z",
                }]))),
        );
        // Review comment: bot by user.type, earlier timestamp — must sort first.
        mount(
            &server,
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/pulls/7/comments"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                    "id": 1,
                    "user": { "login": "ci-daemon", "type": "Bot" },
                    "body": "inline nit",
                    "created_at": "2026-01-01T00:00:00Z",
                }]))),
        );

        let snap = forge(&server).pr_snapshot(&pr7()).expect("snapshot");
        assert_eq!(snap.pr, pr7());
        assert_eq!(snap.state, "open");
        assert!(snap.draft);
        assert_eq!(snap.base, "main");
        assert_eq!(snap.head_sha, "abc123");
        assert_eq!(snap.mergeable, None);
        assert_eq!(
            snap.checks,
            vec![
                CheckRun {
                    name: "ci".into(),
                    status: "completed".into(),
                    conclusion: Some("success".into()),
                    url: Some("https://example.test/ci".into()),
                },
                CheckRun {
                    name: "lint".into(),
                    status: "in_progress".into(),
                    conclusion: None,
                    url: None,
                },
            ],
        );
        assert_eq!(
            snap.reviews,
            vec![Review {
                author: "alice".into(),
                state: "APPROVED".into()
            }],
        );
        assert_eq!(
            snap.comments,
            vec![
                PrComment {
                    id: 1,
                    author: "ci-daemon".into(),
                    body: "inline nit".into(),
                    is_bot: true,
                    created_at: "2026-01-01T00:00:00Z".into(),
                },
                PrComment {
                    id: 2,
                    author: "dependabot[bot]".into(),
                    body: "bump deps".into(),
                    is_bot: true,
                    created_at: "2026-01-02T00:00:00Z".into(),
                },
            ],
        );
    }

    #[test]
    fn snapshot_reports_merged_state_when_merged_at_is_set() {
        let server = start_server();
        mount(
            &server,
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/pulls/7"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "state": "closed",
                    "merged_at": "2026-01-03T12:00:00Z",
                    "draft": false,
                    "base": { "ref": "main" },
                    "head": { "sha": "abc123" },
                    "mergeable": true,
                }))),
        );
        mount(
            &server,
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/commits/abc123/check-runs"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({ "check_runs": [] })),
                ),
        );
        for list in [
            "/repos/acme/widgets/pulls/7/reviews",
            "/repos/acme/widgets/issues/7/comments",
            "/repos/acme/widgets/pulls/7/comments",
        ] {
            mount(
                &server,
                Mock::given(method("GET"))
                    .and(path(list))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!([]))),
            );
        }

        let snap = forge(&server).pr_snapshot(&pr7()).expect("snapshot");
        assert_eq!(snap.state, "merged");
        assert_eq!(snap.mergeable, Some(true));
        assert!(snap.checks.is_empty());
        assert!(snap.reviews.is_empty());
        assert!(snap.comments.is_empty());
    }

    #[test]
    fn merge_sends_selected_merge_method() {
        for (variant, expected) in [
            (MergeMethod::Merge, "merge"),
            (MergeMethod::Squash, "squash"),
            (MergeMethod::Rebase, "rebase"),
        ] {
            let server = start_server();
            mount(
                &server,
                Mock::given(method("PUT"))
                    .and(path("/repos/acme/widgets/pulls/7/merge"))
                    .and(header("authorization", "Bearer test-token-xyz"))
                    .and(body_json(json!({ "merge_method": expected })))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_json(json!({ "merged": true })),
                    )
                    .expect(1),
            );
            forge(&server)
                .merge(&pr7(), variant)
                .unwrap_or_else(|e| panic!("merge {expected}: {e}"));
        }
    }

    #[test]
    fn merge_405_is_permanent_with_github_message() {
        let server = start_server();
        mount(
            &server,
            Mock::given(method("PUT"))
                .and(path("/repos/acme/widgets/pulls/7/merge"))
                .respond_with(
                    ResponseTemplate::new(405)
                        .set_body_json(json!({ "message": "Pull Request is not mergeable" })),
                ),
        );
        match forge(&server).merge(&pr7(), MergeMethod::Squash) {
            Err(AdapterError::Permanent(msg)) => {
                assert!(msg.contains("Pull Request is not mergeable"), "got: {msg}");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[test]
    fn server_error_502_is_transient() {
        let server = start_server();
        mount(
            &server,
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/pulls/7"))
                .respond_with(ResponseTemplate::new(502)),
        );
        match forge(&server).pr_snapshot(&pr7()) {
            Err(AdapterError::Transient(_)) => {}
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn rate_limited_403_is_transient() {
        let server = start_server();
        mount(
            &server,
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/pulls/7"))
                .respond_with(
                    ResponseTemplate::new(403)
                        .insert_header("x-ratelimit-remaining", "0")
                        .set_body_json(json!({ "message": "API rate limit exceeded" })),
                ),
        );
        match forge(&server).pr_snapshot(&pr7()) {
            Err(AdapterError::Transient(msg)) => {
                assert!(msg.contains("API rate limit exceeded"), "got: {msg}");
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn post_comment_posts_issue_comment() {
        let server = start_server();
        mount(
            &server,
            Mock::given(method("POST"))
                .and(path("/repos/acme/widgets/issues/7/comments"))
                .and(header("authorization", "Bearer test-token-xyz"))
                .and(body_json(json!({ "body": "hello from yardmaster" })))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 99 })))
                .expect(1),
        );
        forge(&server)
            .post_comment(&pr7(), "hello from yardmaster")
            .expect("post_comment");
    }
}
