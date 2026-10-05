use bake::{Error, Result, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::github::Repository;

const CRATES_IO_API: &str = "https://crates.io/api/v1";

#[derive(Debug, Deserialize)]
struct VersionsResponse {
    versions: Vec<VersionResponse>,
}

#[derive(Debug, Deserialize)]
struct VersionResponse {
    #[serde(rename = "num")]
    number: String,
}

#[derive(Debug, Deserialize)]
struct ConfigurationsResponse {
    #[serde(default)]
    github_configs: Vec<TrustedPublisher>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TrustedPublisher {
    id: u64,
    #[serde(rename = "crate")]
    package: String,
    repository_owner: String,
    repository_name: String,
    workflow_filename: String,
    environment: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ConfigurationResponse {
    github_config: TrustedPublisher,
}

pub(crate) fn trusted_publisher_plan(
    package: &str,
    repository: &Repository,
    workflow: &str,
    environment: &str,
) -> Result<Value> {
    validate_configuration(package, workflow, environment)?;
    let mut configuration = json!({
        "crate": package,
        "repository_owner": repository.owner,
        "repository_name": repository.name,
        "workflow_filename": workflow,
    });
    if !environment.is_empty() {
        configuration["environment"] = json!(environment);
    }
    Ok(json!({
        "endpoint": format!("{}/trusted_publishing/github_configs", api_base()),
        "method": "POST",
        "github_config": configuration,
        "next": "releases:cargo:trusted-publishing:configure",
    }))
}

pub(crate) fn version_is_published(package: &str, version: &str) -> Result<bool> {
    validate_package_name(package)?;
    let endpoint = format!("{}/crates/{package}/versions", api_base());
    let response = match ureq::get(&endpoint)
        .set("User-Agent", "bake-releases-cargo")
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Ok(false),
        Err(error) => return Err(api_error("check published crate versions", error)),
    };
    let response: VersionsResponse = response
        .into_json()
        .map_err(|error| Error::new(format!("could not decode crates.io response: {error}")))?;

    Ok(response
        .versions
        .iter()
        .any(|published| published.number == version))
}

pub(crate) fn validate_trusted_publisher_inputs(
    package: &str,
    workflow: &str,
    environment: &str,
) -> Result<()> {
    validate_configuration(package, workflow, environment)?;
    registry_token().map(|_| ())
}

pub(crate) fn configure_trusted_publisher(
    package: &str,
    repository: &Repository,
    workflow: &str,
    environment: &str,
) -> Result<Value> {
    validate_configuration(package, workflow, environment)?;
    let token = registry_token()?;

    let configurations = list_trusted_publishers(&token, package)?;
    if let Some(configuration) = configurations.iter().find(|configuration| {
        configuration
            .repository_owner
            .eq_ignore_ascii_case(&repository.owner)
            && configuration
                .repository_name
                .eq_ignore_ascii_case(&repository.name)
            && configuration.workflow_filename == workflow
            && configuration.environment.as_deref() == nonempty(environment)
    }) {
        return Ok(json!({"status": "already_configured", "github_config": configuration}));
    }

    let mut requested = json!({
        "crate": package,
        "repository_owner": repository.owner,
        "repository_name": repository.name,
        "workflow_filename": workflow,
    });
    if let Some(environment) = nonempty(environment) {
        requested["environment"] = json!(environment);
    }

    let endpoint = format!("{}/trusted_publishing/github_configs", api_base());
    let response: ConfigurationResponse = ureq::post(&endpoint)
        .set("Authorization", &token)
        .set("User-Agent", "bake-releases-cargo")
        .send_json(json!({"github_config": requested}))
        .map_err(|error| api_error("create trusted publisher configuration", error))?
        .into_json()
        .map_err(|error| Error::new(format!("could not decode crates.io response: {error}")))?;

    Ok(json!({"status": "created", "github_config": response.github_config}))
}

pub(crate) fn set_trusted_publishing_only(package: &str, required: bool) -> Result<Value> {
    validate_package_name(package)?;
    let token = registry_token()?;

    let endpoint = format!("{}/crates/{package}", api_base());
    let response = ureq::patch(&endpoint)
        .set("Authorization", &token)
        .set("User-Agent", "bake-releases-cargo")
        .send_json(json!({"crate": {"trustpub_only": required}}))
        .map_err(|error| api_error("update trusted-publishing requirement", error))?
        .into_json::<Value>()
        .map_err(|error| Error::new(format!("could not decode crates.io response: {error}")))?;

    Ok(json!({
        "status": "updated",
        "crate": package,
        "trustpub_only": required,
        "response": response,
    }))
}

fn list_trusted_publishers(token: &str, package: &str) -> Result<Vec<TrustedPublisher>> {
    validate_package_name(package)?;
    let endpoint = format!(
        "{}/trusted_publishing/github_configs?crate={package}",
        api_base()
    );
    let response: ConfigurationsResponse = ureq::get(&endpoint)
        .set("Authorization", token)
        .set("User-Agent", "bake-releases-cargo")
        .call()
        .map_err(|error| api_error("list trusted publisher configurations", error))?
        .into_json()
        .map_err(|error| Error::new(format!("could not decode crates.io response: {error}")))?;
    Ok(response.github_configs)
}

fn validate_configuration(package: &str, workflow: &str, environment: &str) -> Result<()> {
    validate_package_name(package)?;
    if workflow.is_empty()
        || !workflow.ends_with(".yml") && !workflow.ends_with(".yaml")
        || !workflow
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(Error::new(
            "workflow must be a filename ending in .yml or .yaml",
        ));
    }
    if environment.contains('/')
        || environment.contains('\\')
        || environment.chars().any(char::is_control)
    {
        return Err(Error::new(
            "environment must be a single GitHub environment name",
        ));
    }
    Ok(())
}

fn validate_package_name(package: &str) -> Result<()> {
    if package.is_empty()
        || !package
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::new(
            "package name must contain only letters, numbers, hyphens, or underscores",
        ));
    }
    Ok(())
}

fn nonempty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn registry_token() -> Result<String> {
    let token = std::env::var("CARGO_REGISTRY_TOKEN").map_err(|_| {
        Error::new(
            "set CARGO_REGISTRY_TOKEN to a crates.io token with Trusted Publishing permission",
        )
    })?;
    if token.trim().is_empty() {
        return Err(Error::new("CARGO_REGISTRY_TOKEN is empty"));
    }
    Ok(token)
}

fn api_error(action: &str, error: ureq::Error) -> Error {
    match error {
        ureq::Error::Status(status, response) => {
            let details = response.into_string().unwrap_or_default();
            Error::new(format!(
                "could not {action}: crates.io returned HTTP {status}: {details}"
            ))
        }
        error => Error::new(format!("could not {action}: {error}")),
    }
}

#[cfg(test)]
fn api_base() -> String {
    std::env::var("BAKE_TEST_CRATES_IO_API").unwrap_or_else(|_| CRATES_IO_API.to_owned())
}

#[cfg(not(test))]
fn api_base() -> String {
    CRATES_IO_API.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Environment, http_server};

    fn repository() -> Repository {
        Repository {
            owner: "socketry".to_owned(),
            name: "fixture".to_owned(),
        }
    }

    fn publisher_response(owner: &str, environment: Option<&str>) -> String {
        let environment = environment
            .map(|environment| format!(", \"environment\":\"{environment}\""))
            .unwrap_or_default();
        format!(
            "{{\"github_config\":{{\"id\":1,\"crate\":\"fixture\",\"repository_owner\":\"{owner}\",\"repository_name\":\"fixture\",\"workflow_filename\":\"publish.yml\"{environment}}}}}"
        )
    }

    #[test]
    fn validates_package_workflow_and_environment_values() {
        for package in ["", "../fixture", "bad/name"] {
            assert!(
                validate_package_name(package)
                    .unwrap_err()
                    .to_string()
                    .contains("package name")
            );
        }
        for workflow in ["", "publish.json", "../publish.yml", "publish yml"] {
            assert!(
                validate_configuration("fixture", workflow, "crates-io")
                    .unwrap_err()
                    .to_string()
                    .contains("workflow must be")
            );
        }
        for environment in ["a/b", "a\\b", "bad\nname"] {
            assert!(
                validate_configuration("fixture", "publish.yml", environment)
                    .unwrap_err()
                    .to_string()
                    .contains("single GitHub environment")
            );
        }
        validate_configuration("fixture_2", "publish.yaml", "").unwrap();
    }

    #[test]
    fn plans_a_trusted_publisher_with_optional_environment() {
        let mut environment = Environment::new();
        environment.remove("BAKE_TEST_CRATES_IO_API");
        let plan = trusted_publisher_plan("fixture", &repository(), "publish.yml", "").unwrap();
        assert_eq!(
            plan["endpoint"],
            format!("{CRATES_IO_API}/trusted_publishing/github_configs")
        );
        assert!(plan["github_config"]["environment"].is_null());
        assert_eq!(plan["next"], "releases:cargo:trusted-publishing:configure");

        let plan =
            trusted_publisher_plan("fixture", &repository(), "publish.yml", "crates-io").unwrap();
        assert_eq!(plan["github_config"]["environment"], "crates-io");
        assert!(trusted_publisher_plan("../fixture", &repository(), "publish.yml", "").is_err());
    }

    #[test]
    fn validates_registry_token_presence_and_nonempty_contents() {
        let mut environment = Environment::new();
        environment.remove("CARGO_REGISTRY_TOKEN");
        assert!(
            validate_trusted_publisher_inputs("fixture", "publish.yml", "crates-io")
                .unwrap_err()
                .to_string()
                .contains("set CARGO_REGISTRY_TOKEN")
        );

        environment.set("CARGO_REGISTRY_TOKEN", " \t");
        assert!(
            validate_trusted_publisher_inputs("fixture", "publish.yml", "crates-io")
                .unwrap_err()
                .to_string()
                .contains("CARGO_REGISTRY_TOKEN is empty")
        );

        environment.set("CARGO_REGISTRY_TOKEN", "secret");
        validate_trusted_publisher_inputs("fixture", "publish.yml", "crates-io").unwrap();
    }

    #[test]
    fn checks_published_versions_for_present_missing_and_invalid_crates() {
        let mut environment = Environment::new();
        let (api, server) = http_server(vec![
            (200, r#"{"versions":[{"num":"1.2.3"}]}"#.to_owned()),
            (200, r#"{"versions":[{"num":"1.2.2"}]}"#.to_owned()),
            (404, "{}".to_owned()),
            (200, "not-json".to_owned()),
            (500, "registry error".to_owned()),
        ]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);

        assert!(version_is_published("fixture", "1.2.3").unwrap());
        assert!(!version_is_published("fixture", "1.2.3").unwrap());
        assert!(!version_is_published("missing", "1.2.3").unwrap());
        assert!(
            version_is_published("fixture", "1.2.3")
                .unwrap_err()
                .to_string()
                .contains("could not decode crates.io response")
        );
        assert!(
            version_is_published("fixture", "1.2.3")
                .unwrap_err()
                .to_string()
                .contains("crates.io returned HTTP 500")
        );
        server.join().unwrap();
        assert!(version_is_published("../fixture", "1.2.3").is_err());
    }

    #[test]
    fn reports_transport_errors_from_crates_io() {
        let mut environment = Environment::new();
        environment.set("BAKE_TEST_CRATES_IO_API", "http://127.0.0.1:1");

        assert!(
            version_is_published("fixture", "1.2.3")
                .unwrap_err()
                .to_string()
                .contains("could not check published crate versions")
        );
    }

    #[test]
    fn creates_a_trusted_publisher_when_no_matching_configuration_exists() {
        let mut environment = Environment::new();
        environment.set("CARGO_REGISTRY_TOKEN", "secret");
        let created = publisher_response("socketry", Some("crates-io"));
        let (api, server) = http_server(vec![
            (200, r#"{"github_configs":[]}"#.to_owned()),
            (200, created.clone()),
        ]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);

        let response =
            configure_trusted_publisher("fixture", &repository(), "publish.yml", "crates-io")
                .unwrap();

        assert_eq!(response["status"], "created");
        assert_eq!(response["github_config"]["repository_owner"], "socketry");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /trusted_publishing/github_configs?crate=fixture "));
        assert!(requests[1].starts_with("POST /trusted_publishing/github_configs "));
    }

    #[test]
    fn recognizes_an_existing_trusted_publisher_case_insensitively() {
        let mut environment = Environment::new();
        environment.set("CARGO_REGISTRY_TOKEN", "secret");
        let configuration = r#"{"github_configs":[{"id":1,"crate":"fixture","repository_owner":"SOCKETRY","repository_name":"FIXTURE","workflow_filename":"publish.yml","environment":"crates-io"}]}"#.to_string();
        let (api, server) = http_server(vec![(200, configuration)]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);

        let response =
            configure_trusted_publisher("fixture", &repository(), "publish.yml", "crates-io")
                .unwrap();

        assert_eq!(response["status"], "already_configured");
        server.join().unwrap();
    }

    #[test]
    fn reports_trusted_publisher_api_and_decode_errors() {
        let mut environment = Environment::new();
        environment.set("CARGO_REGISTRY_TOKEN", "secret");
        let (api, server) = http_server(vec![(500, "registry unavailable".to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(
            configure_trusted_publisher("fixture", &repository(), "publish.yml", "")
                .unwrap_err()
                .to_string()
                .contains("crates.io returned HTTP 500")
        );
        server.join().unwrap();

        let (api, server) = http_server(vec![
            (200, r#"{"github_configs":[]}"#.to_owned()),
            (500, "registry unavailable".to_owned()),
        ]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(
            configure_trusted_publisher("fixture", &repository(), "publish.yml", "")
                .unwrap_err()
                .to_string()
                .contains("could not create trusted publisher configuration")
        );
        server.join().unwrap();

        let (api, server) = http_server(vec![
            (200, r#"{"github_configs":[]}"#.to_owned()),
            (200, "not-json".to_owned()),
        ]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(
            configure_trusted_publisher("fixture", &repository(), "publish.yml", "")
                .unwrap_err()
                .to_string()
                .contains("could not decode crates.io response")
        );
        server.join().unwrap();

        let (api, server) = http_server(vec![(200, "not-json".to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(
            configure_trusted_publisher("fixture", &repository(), "publish.yml", "")
                .unwrap_err()
                .to_string()
                .contains("could not decode crates.io response")
        );
        server.join().unwrap();
    }

    #[test]
    fn updates_trusted_publishing_only_and_handles_bad_responses() {
        let mut environment = Environment::new();
        environment.remove("CARGO_REGISTRY_TOKEN");
        assert!(
            set_trusted_publishing_only("fixture", true)
                .unwrap_err()
                .to_string()
                .contains("set CARGO_REGISTRY_TOKEN")
        );
        environment.set("CARGO_REGISTRY_TOKEN", "secret");

        let (api, server) = http_server(vec![(200, r#"{"trustpub_only":true}"#.to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        let response = set_trusted_publishing_only("fixture", true).unwrap();
        assert_eq!(response["status"], "updated");
        assert_eq!(response["trustpub_only"], true);
        server.join().unwrap();

        let (api, server) = http_server(vec![(500, "registry error".to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(
            set_trusted_publishing_only("fixture", false)
                .unwrap_err()
                .to_string()
                .contains("crates.io returned HTTP 500")
        );
        server.join().unwrap();

        let (api, server) = http_server(vec![(200, "not-json".to_owned())]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(
            set_trusted_publishing_only("fixture", false)
                .unwrap_err()
                .to_string()
                .contains("could not decode crates.io response")
        );
        server.join().unwrap();
        assert!(set_trusted_publishing_only("../fixture", false).is_err());
    }

    #[test]
    fn lists_trusted_publishers_and_reports_decode_failures() {
        let mut environment = Environment::new();
        let (api, server) = http_server(vec![
            (200, r#"{"github_configs":[]}"#.to_owned()),
            (200, "not-json".to_owned()),
        ]);
        environment.set("BAKE_TEST_CRATES_IO_API", &api);
        assert!(
            list_trusted_publishers("token", "fixture")
                .unwrap()
                .is_empty()
        );
        assert!(
            list_trusted_publishers("token", "fixture")
                .unwrap_err()
                .to_string()
                .contains("could not decode crates.io response")
        );
        assert!(list_trusted_publishers("token", "../fixture").is_err());
        server.join().unwrap();
    }

    #[test]
    fn validates_configuration_and_token_before_trusted_publisher_requests() {
        let mut environment = Environment::new();
        assert!(configure_trusted_publisher("fixture", &repository(), "bad.json", "").is_err());
        environment.remove("CARGO_REGISTRY_TOKEN");
        assert!(
            configure_trusted_publisher("fixture", &repository(), "publish.yml", "")
                .unwrap_err()
                .to_string()
                .contains("CARGO_REGISTRY_TOKEN")
        );
    }

    #[test]
    fn reports_crates_io_transport_errors_for_registry_operations() {
        let mut environment = Environment::new();
        environment.set("CARGO_REGISTRY_TOKEN", "secret");
        environment.set("BAKE_TEST_CRATES_IO_API", "http://127.0.0.1:1");
        assert!(
            configure_trusted_publisher("fixture", &repository(), "publish.yml", "")
                .unwrap_err()
                .to_string()
                .contains("could not list trusted publisher configurations")
        );
        assert!(
            set_trusted_publishing_only("fixture", true)
                .unwrap_err()
                .to_string()
                .contains("could not update trusted-publishing requirement")
        );
    }
}
