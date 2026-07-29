//! Compact, structured GitHub CLI query planning.
//!
//! Minha delegates authentication, host selection, and private-repository
//! access to the installed `gh` CLI. This module deliberately plans read-only
//! commands only; remote mutations remain available through the permission-
//! gated `exec` tool so they always produce an explicit approval request.

use serde_json::Value;

const MAX_LIMIT: u64 = 100;

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum GitHubQueryError {
    #[error(
        "github action must be one of repo, issues, issue, prs, pr, checks, runs, workflows, release, releases"
    )]
    UnknownAction,
    #[error("github action {0} requires a positive number")]
    MissingNumber(&'static str),
    #[error("invalid GitHub repository; expected OWNER/REPO")]
    InvalidRepository,
    #[error("invalid release tag")]
    InvalidTag,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubQuery {
    pub action: String,
    pub argv: Vec<String>,
}

impl GitHubQuery {
    pub fn parse(arguments: &Value) -> Result<Self, GitHubQueryError> {
        let action = arguments.get("action").and_then(Value::as_str).unwrap_or("repo");
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, MAX_LIMIT)
            .to_string();
        let number = || {
            arguments
                .get("number")
                .and_then(Value::as_u64)
                .filter(|number| *number > 0)
                .map(|number| number.to_string())
        };
        let mut argv = match action {
            "repo" => vec![
                "repo".into(),
                "view".into(),
                "--json".into(),
                "nameWithOwner,url,visibility,defaultBranchRef,isPrivate,viewerPermission,latestRelease"
                    .into(),
            ],
            "issues" => vec![
                "issue".into(),
                "list".into(),
                "--limit".into(),
                limit,
                "--json".into(),
                "number,title,state,labels,assignees,updatedAt,url".into(),
            ],
            "issue" => vec![
                "issue".into(),
                "view".into(),
                number().ok_or(GitHubQueryError::MissingNumber("issue"))?,
                "--json".into(),
                "number,title,state,body,labels,assignees,comments,updatedAt,url".into(),
            ],
            "prs" => vec![
                "pr".into(),
                "list".into(),
                "--limit".into(),
                limit,
                "--json".into(),
                "number,title,state,isDraft,headRefName,baseRefName,reviewDecision,statusCheckRollup,updatedAt,url"
                    .into(),
            ],
            "pr" => vec![
                "pr".into(),
                "view".into(),
                number().ok_or(GitHubQueryError::MissingNumber("pr"))?,
                "--json".into(),
                "number,title,state,isDraft,body,headRefName,baseRefName,mergeable,reviewDecision,reviews,statusCheckRollup,files,updatedAt,url"
                    .into(),
            ],
            "checks" => vec![
                "pr".into(),
                "checks".into(),
                number().ok_or(GitHubQueryError::MissingNumber("checks"))?,
                "--json".into(),
                "name,state,bucket,link,workflow,startedAt,completedAt".into(),
            ],
            "runs" => vec![
                "run".into(),
                "list".into(),
                "--limit".into(),
                limit,
                "--json".into(),
                "databaseId,name,workflowName,status,conclusion,headBranch,headSha,event,createdAt,updatedAt,url"
                    .into(),
            ],
            "workflows" => vec![
                "workflow".into(),
                "list".into(),
                "--limit".into(),
                limit,
                "--json".into(),
                "id,name,state,path".into(),
            ],
            "release" => {
                let mut values = vec!["release".into(), "view".into()];
                if let Some(tag) = arguments.get("tag").and_then(Value::as_str) {
                    if !valid_tag(tag) {
                        return Err(GitHubQueryError::InvalidTag);
                    }
                    values.push(tag.into());
                }
                values.extend([
                    "--json".into(),
                    "tagName,name,isDraft,isPrerelease,publishedAt,assets,url".into(),
                ]);
                values
            }
            "releases" => vec![
                "release".into(),
                "list".into(),
                "--limit".into(),
                limit,
                "--json".into(),
                "tagName,name,isDraft,isPrerelease,publishedAt".into(),
            ],
            _ => return Err(GitHubQueryError::UnknownAction),
        };
        if let Some(repository) = arguments.get("repo").and_then(Value::as_str) {
            if !valid_repository(repository) {
                return Err(GitHubQueryError::InvalidRepository);
            }
            argv.extend(["--repo".into(), repository.into()]);
        }
        Ok(Self {
            action: action.into(),
            argv,
        })
    }
}

fn valid_repository(value: &str) -> bool {
    let mut parts = value.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(repo), None)
        if valid_component(owner) && valid_component(repo))
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b'+'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plans_bounded_structured_queries() {
        let query = GitHubQuery::parse(&json!({
            "action": "pr",
            "number": 42,
            "repo": "bybrooklyn/minha"
        }))
        .expect("valid GitHub query");
        assert_eq!(&query.argv[..3], ["pr", "view", "42"]);
        assert_eq!(
            &query.argv[query.argv.len() - 2..],
            ["--repo", "bybrooklyn/minha"]
        );

        let releases =
            GitHubQuery::parse(&json!({"action":"releases", "limit":999})).expect("valid releases query");
        assert!(releases.argv.windows(2).any(|pair| pair == ["--limit", "100"]));
    }

    #[test]
    fn rejects_injection_shaped_identifiers() {
        assert_eq!(
            GitHubQuery::parse(&json!({"action":"repo", "repo":"owner/--help"})),
            Err(GitHubQueryError::InvalidRepository)
        );
        assert_eq!(
            GitHubQuery::parse(&json!({"action":"release", "tag":"--help"})),
            Err(GitHubQueryError::InvalidTag)
        );
        assert_eq!(
            GitHubQuery::parse(&json!({"action":"issue"})),
            Err(GitHubQueryError::MissingNumber("issue"))
        );
    }
}
