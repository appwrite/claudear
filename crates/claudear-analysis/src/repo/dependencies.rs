//! Dependencies between repositories, discovered from the package manifests
//! under configured paths and saved for cascades to follow.

use crate::repo::relationships::dependency_type;
use abnegate_vcs::DependencyDiscovery;
use abnegate_vcs::DiscoveredDependency;
use claudear_storage::RepoStore;
use std::path::PathBuf;

/// The dependencies on `known_orgs` declared under the configured `paths`.
pub fn discover_configured(known_orgs: &[String], paths: &[String]) -> Vec<DiscoveredDependency> {
    let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    DependencyDiscovery::new(known_orgs.to_vec()).scan_directories(&paths)
}

/// Save every dependency a cascade can follow, returning how many were saved.
///
/// A dependency that cannot be saved is logged and skipped, so one bad row
/// does not lose the rest.
pub fn save_discovered<T: RepoStore + ?Sized>(
    tracker: &T,
    dependencies: &[DiscoveredDependency],
) -> usize {
    let mut saved = 0;
    for dependency in dependencies {
        let Some(kind) = dependency_type(dependency.manifest) else {
            tracing::warn!(
                manifest = %dependency.manifest,
                upstream = %dependency.depends_on,
                downstream = %dependency.repository,
                "Skipped a dependency from a manifest cascades do not follow"
            );
            continue;
        };
        match tracker.add_dependency(
            &dependency.depends_on,
            &dependency.repository,
            kind.as_str(),
        ) {
            Ok(()) => saved += 1,
            Err(error) => tracing::warn!(
                error = %error,
                upstream = %dependency.depends_on,
                downstream = %dependency.repository,
                "Failed to save dependency"
            ),
        }
    }
    saved
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    const COMPOSER_MANIFEST: &str = "composer.json";
    const PACKAGE_MANIFEST: &str = "package.json";

    fn write_manifest(directory: &Path, name: &str, manifest: serde_json::Value) {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join(name), manifest.to_string()).unwrap();
    }

    fn known_orgs() -> Vec<String> {
        vec!["appwrite".to_string(), "utopia-php".to_string()]
    }

    fn configured(path: &Path) -> Vec<String> {
        vec![path.to_string_lossy().to_string()]
    }

    #[test]
    fn test_a_configured_directory_of_checkouts_is_read_one_level_down() {
        let root = TempDir::new().unwrap();
        let cloud = root.path().join("cloud");
        let console = root.path().join("console");
        let unnamed = root.path().join("unnamed");
        let broken = root.path().join("broken");
        write_manifest(
            &cloud,
            COMPOSER_MANIFEST,
            serde_json::json!({
                "name": "appwrite/cloud",
                "require": { "utopia-php/database": "^1.0", "symfony/console": "^5.0" },
                "require-dev": { "utopia-php/database": "^1.0", "utopia-php/cli": "^1.0" },
            }),
        );
        write_manifest(
            &console,
            PACKAGE_MANIFEST,
            serde_json::json!({
                "name": "@appwrite/console",
                "dependencies": { "@appwrite/sdk": "^1.0", "react": "^18.0" },
                "devDependencies": { "@appwrite/sdk": "^1.0" },
            }),
        );
        write_manifest(
            &unnamed,
            COMPOSER_MANIFEST,
            serde_json::json!({ "require": { "appwrite/sdk-for-php": "^1.0" } }),
        );
        std::fs::create_dir(&broken).unwrap();
        std::fs::write(broken.join(COMPOSER_MANIFEST), "{ invalid json }").unwrap();

        let mut dependencies = discover_configured(&known_orgs(), &configured(root.path()));
        dependencies.sort_by(|left, right| {
            (&left.repository, &left.depends_on).cmp(&(&right.repository, &right.depends_on))
        });

        let found: Vec<_> = dependencies
            .iter()
            .map(|dependency| {
                (
                    dependency.repository.as_str(),
                    dependency.depends_on.as_str(),
                    dependency.manifest.as_str(),
                    dependency.repository_path.as_path(),
                )
            })
            .collect();
        assert_eq!(
            found,
            vec![
                (
                    "appwrite/cloud",
                    "utopia-php/cli",
                    "composer",
                    cloud.as_path()
                ),
                (
                    "appwrite/cloud",
                    "utopia-php/database",
                    "composer",
                    cloud.as_path()
                ),
                ("appwrite/console", "appwrite/sdk", "npm", console.as_path()),
                (
                    "unnamed",
                    "appwrite/sdk-for-php",
                    "composer",
                    unnamed.as_path()
                ),
            ]
        );
    }

    #[test]
    fn test_a_configured_repository_that_declares_dependencies_is_not_read_one_level_down() {
        let root = TempDir::new().unwrap();
        write_manifest(
            root.path(),
            COMPOSER_MANIFEST,
            serde_json::json!({
                "name": "appwrite/appwrite",
                "require": { "utopia-php/framework": "^1.0" },
            }),
        );
        write_manifest(
            &root.path().join("vendor-copy"),
            COMPOSER_MANIFEST,
            serde_json::json!({
                "name": "appwrite/vendored",
                "require": { "utopia-php/cache": "^1.0" },
            }),
        );

        let dependencies = discover_configured(&known_orgs(), &configured(root.path()));

        assert_eq!(dependencies.len(), 1, "{dependencies:?}");
        assert_eq!(dependencies[0].repository, "appwrite/appwrite");
        assert_eq!(dependencies[0].depends_on, "utopia-php/framework");
    }
}
