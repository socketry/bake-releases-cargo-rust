// Released under the MIT License.
// Copyright, 2026, by Samuel Williams.

use bake::{Registry, Result};

fn run(registry: Result<Registry>) -> Result<()> {
    registry?.run()
}

fn main() -> Result<()> {
    run(Registry::discover())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bake::Error;

    #[test]
    fn reports_registry_discovery_errors() {
        let result = run(Err(Error::new("invalid task registry")));

        assert_eq!(result.unwrap_err().to_string(), "invalid task registry");
    }
}

#[path = "bake_generated_tasks/mod.rs"]
mod bake_generated_tasks;
