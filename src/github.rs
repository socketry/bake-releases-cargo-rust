use bake::{Context, Error, Result, Value};
use serde_json::{Value as JsonValue, json};
use std::io::{self, Write};
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
    let object = payload
        .as_object_mut()
        .unwrap_or_else(|| unreachable!("managed ruleset payloads are JSON objects"));
    object.remove("repository");
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
        if let Err(error) = write_json_input(child.stdin.as_mut(), body) {
            drop(child.stdin.take());
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        drop(child.stdin.take());
    }
    let output = wait_with_output(child)?;
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

fn write_json_input<W: Write>(input: Option<&mut W>, body: &JsonValue) -> Result<()> {
    let input = input.ok_or_else(|| Error::new("could not open GitHub CLI input"))?;
    let contents = match serde_json::to_vec(body) {
        Ok(contents) => contents,
        Err(_) => unreachable!("serde_json::Value always serializes successfully"),
    };
    input.write_all(&contents)?;
    Ok(())
}

fn wait_with_output(child: std::process::Child) -> io::Result<std::process::Output> {
    #[cfg(test)]
    if std::env::var_os("BAKE_TEST_GITHUB_WAIT_FAILURE").is_some() {
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other("injected GitHub CLI wait failure"));
    }

    child.wait_with_output()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Environment, Project};
    use serde_json::json;

    fn repository() -> Repository {
        Repository {
            owner: "socketry".to_owned(),
            name: "fixture".to_owned(),
        }
    }

    fn gh(project: &Project, environment: &mut Environment, script: &str) {
        project.executable("gh", script);
        environment.prepend_path(&project.root().join("bin"));
    }

    #[test]
    fn resolves_explicit_repository_names_and_validates_components() {
        let project = Project::new();
        let context = project.context();
        let resolved = Repository::resolve(&context, "socketry/project").unwrap();
        assert_eq!(resolved.full_name(), "socketry/project");
        assert!(
            Repository::resolve(&context, "missing-slash")
                .unwrap_err()
                .to_string()
                .contains("owner/name")
        );
        assert!(
            Repository::resolve(&context, "socketry/too/many")
                .unwrap_err()
                .to_string()
                .contains("only letters")
        );
        assert!(
            Repository::resolve(&context, "bad owner/project")
                .unwrap_err()
                .to_string()
                .contains("only letters")
        );
        assert!(Repository::resolve(&context, "/project").is_err());
        assert!(Repository::resolve(&context, "socketry/").is_err());
    }

    #[test]
    fn infers_origin_from_supported_github_remote_formats() {
        let project = Project::new();
        let mut environment = Environment::new();
        let git = project.executable(
            "git",
            "#!/bin/sh\nprintf '%s\\n' \"$BAKE_TEST_GIT_REMOTE\"\n",
        );
        environment.prepend_path(&project.root().join("bin"));

        for remote in [
            "https://github.com/socketry/project.git",
            "http://github.com/socketry/project",
            "ssh://git@github.com/socketry/project.git",
            "git@github.com:socketry/project.git",
        ] {
            environment.set("BAKE_TEST_GIT_REMOTE", remote);
            std::fs::write(
                &git,
                "#!/bin/sh\nprintf '%s\\n' \"$BAKE_TEST_GIT_REMOTE\"\n",
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            assert_eq!(
                Repository::from_origin(&project.context())
                    .unwrap()
                    .full_name(),
                "socketry/project"
            );
            assert_eq!(
                Repository::resolve(&project.context(), "")
                    .unwrap()
                    .full_name(),
                "socketry/project"
            );
        }
    }

    #[test]
    fn reports_invalid_origin_remotes() {
        let project = Project::new();
        let mut environment = Environment::new();
        let git = project.executable("git", "#!/bin/sh\necho 'remote failure' >&2\nexit 2\n");
        environment.prepend_path(&project.root().join("bin"));
        assert!(
            Repository::from_origin(&project.context())
                .unwrap_err()
                .to_string()
                .contains("could not read the origin")
        );

        environment.set("PATH", project.root());
        assert!(Repository::from_origin(&project.context()).is_err());
        environment.prepend_path(&project.root().join("bin"));

        for remote in [
            "https://example.com/socketry/project.git",
            "https://github.com",
            "https://github.com/",
            "https://github.com/socketry/project/extra",
            "https://github.com/bad owner/project",
        ] {
            std::fs::write(&git, &format!("#!/bin/sh\necho '{remote}'\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let error = Repository::from_origin(&project.context()).unwrap_err();
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn validates_setup_limits_and_reviewer_syntax() {
        let valid = vec!["Team:123".to_owned(), "User:456".to_owned()];
        validate_setup("main", 1, &[], &valid, Some(30), "crates-io").unwrap();
        assert!(validate_branch("").is_err());
        assert!(validate_branch("bad/branch").is_err());
        assert!(validate_setup("bad/branch", 1, &[], &[], None, "crates-io").is_err());
        assert!(
            validate_setup("main", 7, &[], &[], None, "crates-io")
                .unwrap_err()
                .to_string()
                .contains("zero and six")
        );
        assert!(
            validate_setup("main", 1, &[" ".to_owned()], &[], None, "crates-io")
                .unwrap_err()
                .to_string()
                .contains("cannot be empty")
        );
        assert!(
            validate_setup(
                "main",
                1,
                &[],
                &vec!["User:1".to_owned(); 7],
                None,
                "crates-io"
            )
            .is_err()
        );
        for reviewer in ["123", "Other:12", "User:no", "Team:"] {
            assert!(
                validate_setup("main", 1, &[], &[reviewer.to_owned()], None, "crates-io").is_err()
            );
        }
        assert!(validate_setup("main", 1, &[], &[], Some(43_201), "crates-io").is_err());
        assert!(validate_setup("main", 1, &[], &[], None, "").is_err());
        assert!(validate_setup("main", 1, &[], &[], None, "invalid/env").is_err());
    }

    #[test]
    fn chooses_default_checks_and_builds_repository_setup_plan() {
        assert_eq!(effective_checks(&[]), ["Publish to crates.io / check"]);
        let checks = vec!["Test / test".to_owned()];
        assert_eq!(effective_checks(&checks), checks);

        let plan = setup_plan(
            &repository(),
            "main",
            2,
            &["Test / test".to_owned()],
            &["Team:123".to_owned()],
            Some(30),
            "crates-io",
        );
        assert_eq!(plan["repository"], "socketry/fixture");
        assert_eq!(
            plan["branch_ruleset"]["conditions"]["ref_name"]["include"][0],
            "refs/heads/main"
        );
        assert_eq!(
            plan["tag_ruleset"]["conditions"]["ref_name"]["include"][0],
            "refs/tags/v*"
        );
        assert_eq!(plan["environment"]["wait_timer"], 30);
        assert_eq!(plan["environment"]["reviewers"][0]["id"], 123);
        assert_eq!(plan["apply"], "releases:cargo:setup:github:apply");
    }

    #[test]
    fn preserves_existing_environment_rules_unless_overridden() {
        let existing = json!({
                "protection_rules": [
                    {"type": "wait_timer", "wait_timer": 15},
                    {"type": "required_reviewers", "prevent_self_review": false, "reviewers": [
                        {"type": "Team", "reviewer": {"id": 55}},
                        {"reviewer": {"id": 54}},
                        {"type": "bad"},
                        {"type": 7, "reviewer": {"id": 56}},
                        {"type": "Team"},
                        {"type": "Team", "reviewer": {}},
                        {"type": "User", "reviewer": {"id": "wrong"}}
                    ]}
            ],
            "deployment_branch_policy": {"protected_branches": true}
        });
        let preserved = environment_payload("crates-io", &[], None, Some(&existing));
        assert_eq!(preserved["wait_timer"], 15);
        assert_eq!(preserved["prevent_self_review"], false);
        assert_eq!(preserved["reviewers"], json!([{"type":"Team","id":55}]));
        assert_eq!(
            preserved["deployment_branch_policy"]["protected_branches"],
            true
        );

        let requested = environment_payload(
            "crates-io",
            &["User:22".to_owned()],
            Some(60),
            Some(&existing),
        );
        assert_eq!(requested["wait_timer"], 60);
        assert_eq!(requested["prevent_self_review"], true);
        assert_eq!(requested["reviewers"], json!([{"type":"User","id":22}]));

        let empty = environment_payload("crates-io", &["invalid".to_owned()], None, None);
        assert_eq!(empty["reviewers"], json!([]));

        let incomplete = environment_payload(
            "crates-io",
            &[],
            None,
            Some(&json!({"protection_rules":[{"type":"required_reviewers"}]})),
        );
        assert!(incomplete.get("reviewers").is_none());
    }

    #[test]
    fn invokes_github_api_with_and_without_json_bodies() {
        let project = Project::new();
        let mut environment = Environment::new();
        let log = project.root().join("gh-log");
        let body = project.root().join("gh-body");
        let script = format!(
            "#!/bin/sh\nprintf '%s %s %s\\n' \"$2\" \"$3\" \"$4\" >> '{}'\nif [ \"$4\" = '/fail' ]; then echo 'api failed' >&2; exit 1; fi\nif [ \"$4\" = '/empty' ]; then exit 0; fi\nif [ \"$4\" = '/invalid' ]; then echo invalid; exit 0; fi\nif [ \"$4\" = '/body' ]; then cat > '{}'; fi\necho '{{\"ok\":true}}'\n",
            log.display(),
            body.display()
        );
        gh(&project, &mut environment, &script);
        let context = project.context();

        assert_eq!(
            github_api(&context, "GET", "/ok", None).unwrap(),
            json!({"ok":true})
        );
        assert!(
            github_api(&context, "GET", "/empty", None)
                .unwrap()
                .is_null()
        );
        assert!(
            github_api(&context, "GET", "/invalid", None)
                .unwrap_err()
                .to_string()
                .contains("could not parse GitHub API response")
        );
        assert!(
            github_api(&context, "GET", "/fail", None)
                .unwrap_err()
                .to_string()
                .contains("api failed")
        );
        assert_eq!(
            github_api(&context, "POST", "/body", Some(&json!({"value":1}))).unwrap(),
            json!({"ok":true})
        );
        assert_eq!(std::fs::read_to_string(body).unwrap(), r#"{"value":1}"#);
        assert!(std::fs::read_to_string(log).unwrap().contains("POST /body"));

        environment.set("BAKE_TEST_GITHUB_WAIT_FAILURE", "true");
        assert!(github_api(&context, "GET", "/ok", None).is_err());
    }

    #[test]
    fn reports_json_input_write_failures() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("write failed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut writer = FailingWriter;
        assert!(write_json_input(Some(&mut writer), &json!({"value":1})).is_err());
        assert!(write_json_input::<FailingWriter>(None, &json!({"value":1})).is_err());
        assert!(writer.flush().is_ok());
    }

    #[test]
    fn reports_request_body_write_failures_and_reaps_the_child() {
        let project = Project::new();
        let mut environment = Environment::new();
        gh(&project, &mut environment, "#!/bin/sh\nexit 0\n");
        let body = json!({"value": "x".repeat(1_048_576)});

        assert!(github_api(&project.context(), "POST", "/body", Some(&body)).is_err());
    }

    #[test]
    fn reports_a_missing_github_cli() {
        let project = Project::new();
        let mut environment = Environment::new();
        environment.set("PATH", project.root());

        assert!(
            github_api(&project.context(), "GET", "/", None)
                .unwrap_err()
                .to_string()
                .contains("could not start GitHub CLI")
        );
    }

    #[test]
    fn updates_rulesets_and_environment_with_existing_project_state() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);
        gh(
            &project,
            &mut environment,
            "#!/bin/sh\ncase \"$4\" in\n  */environments?per_page=100) echo '{\"environments\":[]}';;\n  */rulesets?per_page=100) echo '[]';;\n  *) cat >/dev/null; echo '{\"id\":1}';;\nesac\n",
        );

        let result = apply_setup(
            &project.context(),
            &repository(),
            "main",
            1,
            &["Test / test".to_owned()],
            &[],
            None,
            "crates-io",
        )
        .unwrap();

        assert_eq!(result["repository"], "socketry/fixture");
        assert!(result["branch_ruleset"]["id"].as_u64().is_some());
        assert!(result["tag_ruleset"]["id"].as_u64().is_some());
        assert!(result["environment"]["id"].as_u64().is_some());
    }

    #[test]
    fn preserves_an_existing_environment_and_rejects_empty_workspaces() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);
        gh(
            &project,
            &mut environment,
            "#!/bin/sh\ncase \"$4\" in\n  */environments?per_page=100) echo '{\"environments\":[{\"name\":\"crates-io\"}]}';;\n  */environments/crates-io) echo '{\"protection_rules\":[{\"type\":\"required_reviewers\",\"prevent_self_review\":false,\"reviewers\":[{\"type\":\"Team\",\"reviewer\":{\"id\":42}}]}],\"deployment_branch_policy\":{\"protected_branches\":true}}';;\n  */rulesets?per_page=100) echo '[]';;\n  *) cat >/dev/null; echo '{\"id\":1}';;\nesac\n",
        );

        apply_setup(
            &project.context(),
            &repository(),
            "main",
            1,
            &[],
            &[],
            None,
            "crates-io",
        )
        .unwrap();

        drop(environment);
        let empty = Project::new();
        let mut empty_environment = Environment::new();
        empty.write(
            "Cargo.toml",
            "[package]\nname = \"private\"\nversion = \"1.2.3\"\nedition = \"2024\"\npublish = false\n",
        );
        empty.write("src/lib.rs", "// fixture\n");
        empty.cargo_proxy(&mut empty_environment, None);
        assert!(
            apply_setup(
                &empty.context(),
                &repository(),
                "main",
                1,
                &[],
                &[],
                None,
                "crates-io",
            )
            .unwrap_err()
            .to_string()
            .contains("no publishable packages")
        );
    }

    #[test]
    fn reports_failures_while_applying_repository_settings() {
        let project = Project::new();
        let mut environment = Environment::new();
        project.single_package("fixture", "1.2.3");
        project.cargo_proxy(&mut environment, None);
        let marker = project.root().join("tag-ruleset-posted");
        let script = format!(
            "#!/bin/sh\ncase \"$4\" in\n  */environments?per_page=100) if [ \"$BAKE_TEST_GH_FAILURE\" = environment_list ]; then exit 1; fi; echo '{{\"environments\":[]}}';;\n  */rulesets?per_page=100) if [ \"$BAKE_TEST_GH_FAILURE\" = branch_ruleset_list ]; then exit 1; fi; echo '[]';;\n  */rulesets) cat >/dev/null; if [ \"$BAKE_TEST_GH_FAILURE\" = tag_ruleset_post ] && [ -f \"{}\" ]; then exit 1; fi; if [ \"$BAKE_TEST_GH_FAILURE\" = tag_ruleset_post ]; then touch \"{}\"; fi; echo '{{\"id\":1}}';;\n  */environments/crates-io) cat >/dev/null; if [ \"$BAKE_TEST_GH_FAILURE\" = environment_put ]; then exit 1; fi; echo '{{\"id\":1}}';;\nesac\n",
            marker.display(),
            marker.display()
        );
        gh(&project, &mut environment, &script);

        environment.set("CARGO", project.root().join("missing-cargo").as_os_str());
        assert!(
            apply_setup(
                &project.context(),
                &repository(),
                "main",
                1,
                &[],
                &[],
                None,
                "crates-io",
            )
            .is_err()
        );
        project.cargo_proxy(&mut environment, None);

        for failure in [
            "environment_list",
            "branch_ruleset_list",
            "tag_ruleset_post",
            "environment_put",
        ] {
            let _ = std::fs::remove_file(&marker);
            environment.set("BAKE_TEST_GH_FAILURE", failure);
            assert!(
                apply_setup(
                    &project.context(),
                    &repository(),
                    "main",
                    1,
                    &[],
                    &[],
                    None,
                    "crates-io",
                )
                .is_err()
            );
        }
    }

    #[test]
    fn reports_invalid_environment_responses_and_missing_ruleset_names() {
        let project = Project::new();
        let mut environment = Environment::new();
        environment.set("PATH", project.root());
        assert!(existing_environment(&project.context(), &repository(), "crates-io").is_err());
        assert!(
            upsert_ruleset(
                &project.context(),
                &repository(),
                &json!({"name":"fixture"})
            )
            .is_err()
        );

        gh(&project, &mut environment, "#!/bin/sh\nexit 1\n");
        assert!(
            upsert_ruleset(
                &project.context(),
                &repository(),
                &json!({"name":"fixture"})
            )
            .is_err()
        );

        gh(&project, &mut environment, "#!/bin/sh\necho '{}'\n");
        assert!(
            existing_environment(&project.context(), &repository(), "crates-io")
                .unwrap_err()
                .to_string()
                .contains("invalid environments response")
        );
        assert!(
            upsert_ruleset(&project.context(), &repository(), &json!({}))
                .unwrap_err()
                .to_string()
                .contains("no name")
        );
    }

    #[test]
    fn reuses_rulesets_and_preserves_environment_configuration() {
        let project = Project::new();
        let mut environment = Environment::new();
        let desired = json!({"name":"Socketry Cargo checks","repository":"ignored"});
        let existing = json!({"id":42,"name":"Socketry Cargo checks"});

        let gh_script = "#!/bin/sh\ncase \"$4\" in\n  */rulesets?per_page=100) echo '[{\"id\":42,\"name\":\"Socketry Cargo checks\"}]';;\n  */rulesets/42) cat >/dev/null; echo '{\"id\":42}';;\n  *) echo '{}';;\nesac\n";
        gh(&project, &mut environment, gh_script);
        let updated = upsert_ruleset(&project.context(), &repository(), &desired).unwrap();
        assert_eq!(updated["id"], 42);
        assert_eq!(existing["id"], 42);
    }

    #[test]
    fn validates_ruleset_responses_and_duplicate_managed_names() {
        let project = Project::new();
        let mut environment = Environment::new();
        let desired = json!({"name":"Socketry Cargo checks"});

        gh(&project, &mut environment, "#!/bin/sh\necho '{}'\n");
        assert!(
            upsert_ruleset(&project.context(), &repository(), &desired)
                .unwrap_err()
                .to_string()
                .contains("invalid rulesets response")
        );

        gh(
            &project,
            &mut environment,
            "#!/bin/sh\necho '[{\"name\":\"Socketry Cargo checks\"},{\"name\":\"Socketry Cargo checks\"}]'\n",
        );
        assert!(
            upsert_ruleset(&project.context(), &repository(), &desired)
                .unwrap_err()
                .to_string()
                .contains("multiple GitHub rulesets")
        );

        gh(
            &project,
            &mut environment,
            "#!/bin/sh\necho '[{\"name\":\"Socketry Cargo checks\"}]'\n",
        );
        assert!(
            upsert_ruleset(&project.context(), &repository(), &desired)
                .unwrap_err()
                .to_string()
                .contains("no numeric ID")
        );
    }
}
