// Released under the MIT License.
// Copyright, 2026, by Samuel Williams.

use bake_releases_cargo::releases::cargo::{
    bootstrap_task, create_package_archive_task, packages_task, publish_pending_task, publish_task,
    publish_workspace_task, release_task,
};

#[test]
fn legacy_descriptors_keep_their_registered_command_names() {
    for (task, name) in [
        (bootstrap_task(), "releases:cargo:bootstrap"),
        (create_package_archive_task(), "releases:cargo:package"),
        (packages_task(), "releases:cargo:packages"),
        (publish_pending_task(), "releases:cargo:publish:pending"),
        (publish_task(), "releases:cargo:publish"),
        (publish_workspace_task(), "releases:cargo:publish:workspace"),
        (release_task(), "releases:cargo:release"),
    ] {
        assert_eq!(task.name(), name);
    }
}
