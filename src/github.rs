use bake::{Context, Error, Result, Value};
use serde_json::{Value as JsonValue, json};
use std::io::Write;
use std::process::Stdio;

#[derive(Clone, Debug)]
pub(crate) struct Repository {
    pub owner: String,
    pub name: String,
}

impl Repository {
    pub(crate) fn resolve(context: &Context, value: &str) -> Result<Self> {
        if value.is_empty() {
            return Self::from_origin(context);
        }
        let (owner, name) = value
            .split_once('/')
            .ok_or_else(|| Error::new("repository must use owner/name format"))?;
        Self::new(owner, name)
    }

    pub(crate) fn from_origin(context: &Context) -> Result<Self> {
        let output = context
            .command("git")
            .args(["remote", "get-url", "origin"])
            .output()?;
        if !output.status.success() {
            return Err(Error::new(format!(
                "could not read the origin Git remote: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let remote = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let path = if let Some(path) = remote.strip_prefix("https://github.com/") {
            path
        } else if let Some(path) = remote.strip_prefix("http://github.com/") {
            path
        } else if let Some(path) = remote.strip_prefix("ssh://git@github.com/") {
            path
        } else if let Some(path) = remote.strip_prefix("git@github.com:") {
            path
        } else {
            return Err(Error::new(format!(
                "origin remote {remote:?} is not a github.com repository"
            )));
        };
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (owner, name) = path
            .split_once('/')
            .ok_or_else(|| Error::new("GitHub origin must include owner and repository name"))?;
        if name.contains('/') {
            return Err(Error::new(
                "GitHub origin has more than one repository path component",
            ));
        }
        Self::new(owner, name)
    }

    fn new(owner: &str, name: &str) -> Result<Self> {
        if !valid_repository_part(owner) || !valid_repository_part(name) {
            return Err(Error::new(
                "GitHub owner and repository must contain only letters, numbers, dots, underscores, or hyphens",
            ));
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    pub(crate) fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

fn valid_repository_part(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub(crate) fn validate_branch(branch: &str) -> Result<()> {
    if valid_repository_part(branch) {
        Ok(())
    } else {
        Err(Error::new(
            "branch must contain only letters, numbers, dots, underscores, or hyphens",
        ))
    }
}

pub(crate) fn effective_checks(checks: &[String]) -> Vec<String> {
    if checks.is_empty() {
        vec!["Publish to crates.io / check".to_owned()]
    } else {
        checks.to_vec()
    }
}

pub(crate) fn validate_setup(
    branch: &str,
    approvals: u32,
    checks: &[String],
    reviewers: &[String],
    wait_timer: Option<u32>,
    environment: &str,
) -> Result<()> {
    validate_branch(branch)?;
    if approvals > 6 {
        return Err(Error::new(
            "GitHub rulesets allow between zero and six required pull request approvals",
        ));
    }
    if checks.iter().any(|check| check.trim().is_empty()) {
        return Err(Error::new("status check names cannot be empty"));
    }
    if reviewers.len() > 6 {
        return Err(Error::new(
            "GitHub environments allow at most six reviewers",
        ));
    }
    for reviewer in reviewers {
        parse_reviewer(reviewer)?;
    }
    if wait_timer.is_some_and(|wait_timer| wait_timer > 43_200) {
        return Err(Error::new(
            "environment wait timer must be between zero and 43,200 minutes",
        ));
    }
    if environment.is_empty() || !valid_repository_part(environment) {
        return Err(Error::new(
            "environment must contain only letters, numbers, dots, underscores, or hyphens",
        ));
    }
    Ok(())
}

fn parse_reviewer(value: &str) -> Result<(&str, u64)> {
    let (kind, identifier) = value
        .split_once(':')
        .ok_or_else(|| Error::new(format!("reviewer {value:?} must use User:ID or Team:ID")))?;
    if !matches!(kind, "User" | "Team") {
        return Err(Error::new(format!(
            "reviewer type in {value:?} must be User or Team"
        )));
    }
    let identifier = identifier.parse::<u64>().map_err(|_| {
        Error::new(format!(
            "reviewer {value:?} must contain a numeric GitHub ID"
        ))
    })?;
    Ok((kind, identifier))
}

pub(crate) fn setup_plan(
    repository: &Repository,
    branch: &str,
    approvals: u32,
    checks: &[String],
    reviewers: &[String],
    wait_timer: Option<u32>,
    environment: &str,
) -> Value {
    json!({
        "repository": repository.full_name(),
        "branch_ruleset": branch_ruleset(branch, approvals, checks),
        "tag_ruleset": tag_ruleset(),
        "environment": environment_payload(environment, reviewers, wait_timer, None),
        "preservation": "Existing environment settings are preserved unless overridden above.",
        "apply": "releases:cargo:setup:github:apply",
    })
}

fn branch_ruleset(branch: &str, approvals: u32, checks: &[String]) -> JsonValue {
    let mut rules = vec![json!({
        "type": "pull_request",
        "parameters": {
            "allowed_merge_methods": ["squash", "rebase"],
            "dismiss_stale_reviews_on_push": true,
            "require_code_owner_review": false,
            "require_last_push_approval": false,
            "required_approving_review_count": approvals,
            "required_review_thread_resolution": true,
        }
    })];
    rules.push(json!({
        "type": "required_status_checks",
        "parameters": {
            "do_not_enforce_on_create": false,
            "required_status_checks": checks.iter().map(|check| json!({"context": check})).collect::<Vec<_>>(),
            "strict_required_status_checks_policy": true,
        }
    }));
    rules.push(json!({"type": "non_fast_forward"}));

    json!({
        "name": "Socketry Cargo checks",
        "target": "branch",
        "enforcement": "active",
        "conditions": {"ref_name": {"include": [format!("refs/heads/{branch}")], "exclude": []}},
        "rules": rules,
        "bypass_actors": [],
    })
}

fn tag_ruleset() -> JsonValue {
    json!({
        "name": "Socketry Cargo release tags",
        "target": "tag",
        "enforcement": "active",
        "conditions": {"ref_name": {"include": ["refs/tags/v*"], "exclude": []}},
        "rules": [{"type": "deletion"}],
        "bypass_actors": [],
    })
}

fn environment_payload(
    environment: &str,
    requested_reviewers: &[String],
    wait_timer: Option<u32>,
    existing: Option<&JsonValue>,
) -> JsonValue {
    let mut payload = json!({"name": environment});
    let existing_protection_rules = existing
        .and_then(|environment| environment.get("protection_rules"))
        .and_then(JsonValue::as_array);
    let existing_wait_timer = existing_protection_rules
        .into_iter()
        .flatten()
        .find(|rule| rule.get("type").and_then(JsonValue::as_str) == Some("wait_timer"))
        .and_then(|rule| rule.get("wait_timer"))
        .and_then(JsonValue::as_u64);
    if let Some(wait_timer) = wait_timer.or(existing_wait_timer.map(|timer| timer as u32)) {
        payload["wait_timer"] = json!(wait_timer);
    }

    if !requested_reviewers.is_empty() {
        let reviewers: Vec<_> = requested_reviewers
            .iter()
            .filter_map(|reviewer| parse_reviewer(reviewer).ok())
            .map(|(kind, identifier)| json!({"type": kind, "id": identifier}))
            .collect();
        payload["prevent_self_review"] = json!(true);
        payload["reviewers"] = json!(reviewers);
    } else if let Some(existing_rules) = existing_protection_rules
        && let Some(review_rule) = existing_rules
            .iter()
            .find(|rule| rule.get("type").and_then(JsonValue::as_str) == Some("required_reviewers"))
    {
        if let Some(prevent_self_review) = review_rule.get("prevent_self_review") {
            payload["prevent_self_review"] = prevent_self_review.clone();
        }
        if let Some(reviewers) = review_rule.get("reviewers").and_then(JsonValue::as_array) {
            payload["reviewers"] = json!(
                reviewers
                    .iter()
                    .filter_map(|reviewer| {
                        let kind = reviewer.get("type")?.as_str()?;
                        let identifier = reviewer.get("reviewer")?.get("id")?.as_u64()?;
                        Some(json!({"type": kind, "id": identifier}))
                    })
                    .collect::<Vec<_>>()
            );
        }
    }

    if let Some(branch_policy) =
        existing.and_then(|environment| environment.get("deployment_branch_policy"))
    {
        payload["deployment_branch_policy"] = branch_policy.clone();
    }
    payload
}

fn existing_environment(
    context: &Context,
    repository: &Repository,
    environment: &str,
) -> Result<Option<JsonValue>> {
    let list_path = format!("repos/{}/environments?per_page=100", repository.full_name());
    let response = github_api(context, "GET", &list_path, None)?;
    let environments = response
        .get("environments")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| Error::new("GitHub returned an invalid environments response"))?;
    if !environments
        .iter()
        .any(|existing| existing.get("name").and_then(JsonValue::as_str) == Some(environment))
    {
        return Ok(None);
    }

    let path = format!(
        "repos/{}/environments/{environment}",
        repository.full_name()
    );
    github_api(context, "GET", &path, None).map(Some)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_setup(
    context: &Context,
    repository: &Repository,
    branch: &str,
    approvals: u32,
    checks: &[String],
    reviewers: &[String],
    wait_timer: Option<u32>,
    environment: &str,
) -> Result<Value> {
    let names = crate::cargo::workspace_packages(context)?
        .into_iter()
        .map(|package| package.name)
        .collect::<Vec<_>>();
    if names.is_empty() {
        return Err(Error::new(
            "the workspace has no publishable packages to protect",
        ));
    }
    let existing_environment = existing_environment(context, repository, environment)?;
    let branch_ruleset = branch_ruleset(branch, approvals, checks);
    let tag_ruleset = tag_ruleset();

    let branch_result = upsert_ruleset(context, repository, &branch_ruleset)?;
    let tag_result = upsert_ruleset(context, repository, &tag_ruleset)?;
    let environment_body = environment_payload(
        environment,
        reviewers,
        wait_timer,
        existing_environment.as_ref(),
    );
    let environment_path = format!(
        "repos/{}/environments/{}",
        repository.full_name(),
        environment
    );
    let environment_result =
        github_api(context, "PUT", &environment_path, Some(&environment_body))?;

    Ok(json!({
        "repository": repository.full_name(),
        "branch_ruleset": branch_result,
        "tag_ruleset": tag_result,
        "environment": environment_result,
    }))
}

fn upsert_ruleset(
    context: &Context,
    repository: &Repository,
    desired: &JsonValue,
) -> Result<JsonValue> {
    let name = desired
        .get("name")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| Error::new("managed ruleset has no name"))?;
    let path = format!("repos/{}/rulesets?per_page=100", repository.full_name());
    let existing = github_api(context, "GET", &path, None)?;
    let rulesets = existing
        .as_array()
        .ok_or_else(|| Error::new("GitHub returned an invalid rulesets response"))?;
    let matches: Vec<_> = rulesets
        .iter()
        .filter(|ruleset| ruleset.get("name").and_then(JsonValue::as_str) == Some(name))
        .collect();
    if matches.len() > 1 {
        return Err(Error::new(format!(
            "multiple GitHub rulesets are named {name:?}"
        )));
    }

    let mut payload = desired.clone();
    if let Some(object) = payload.as_object_mut() {
        object.remove("repository");
    }
    if let Some(existing) = matches.first() {
        let identifier = existing
            .get("id")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| Error::new(format!("GitHub ruleset {name:?} has no numeric ID")))?;
        let path = format!("repos/{}/rulesets/{identifier}", repository.full_name());
        github_api(context, "PUT", &path, Some(&payload))
    } else {
        let path = format!("repos/{}/rulesets", repository.full_name());
        github_api(context, "POST", &path, Some(&payload))
    }
}

pub(crate) fn github_api(
    context: &Context,
    method: &str,
    path: &str,
    body: Option<&JsonValue>,
) -> Result<JsonValue> {
    let mut command = context.command("gh");
    command.args(["api", "--method", method, path]);
    if body.is_some() {
        command.args(["--input", "-"]).stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| Error::new(format!("could not start GitHub CLI `gh`: {error}")))?;
    if let Some(body) = body {
        let contents = serde_json::to_vec(body)?;
        child
            .stdin
            .take()
            .ok_or_else(|| Error::new("could not open GitHub CLI input"))?
            .write_all(&contents)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "GitHub API request {method} {path} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if output.stdout.is_empty() {
        return Ok(JsonValue::Null);
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| Error::new(format!("could not parse GitHub API response: {error}")))
}
