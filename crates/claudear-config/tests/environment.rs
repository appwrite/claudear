use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use abnegate_config::EnvironmentFile;
use claudear_config::environment;
use claudear_core::error::Error;
use tempfile::TempDir;

const TRICKY_VALUES: [&str; 7] = [
    "4f9c2a7e1b3d5f60",
    "https://discord.com/api/webhooks/123/abc-DEF_ghi",
    "with spaces",
    "pa$$word",
    "it's \"quoted\" \\ `id`",
    "line\nbreak\rreturn",
    "",
];

fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn read(path: &Path) -> BTreeMap<String, String> {
    EnvironmentFile::new(path).read().unwrap()
}

#[test]
fn test_every_value_reads_back_unchanged_under_every_key_claudear_writes() {
    for value in TRICKY_VALUES {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(".env");
        let written: BTreeMap<String, String> = environment::KEYS
            .iter()
            .map(|key| (key.to_string(), value.to_string()))
            .collect();
        assert_eq!(written.len(), environment::KEYS.len());

        environment::update(&path, &written).unwrap();

        assert_eq!(read(&path), written, "value {value:?}");
    }
}

#[test]
fn test_existing_lines_survive_and_new_keys_are_appended_sorted_without_a_header() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(".env");
    fs::write(
        &path,
        "# Claudear\nCLAUDEAR_LINEAR_API_KEY=lin_api_123\n\nGITHUB_WEBHOOK_SECRET=old\nCLAUDEAR_LOG=info\n",
    )
    .unwrap();

    environment::update(
        &path,
        &values(&[
            (environment::LINEAR_WEBHOOK_SECRET, "linear"),
            (environment::GITHUB_WEBHOOK_SECRET, "new"),
            (environment::GITLAB_WEBHOOK_SECRET, "gitlab"),
        ]),
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "# Claudear\nCLAUDEAR_LINEAR_API_KEY=lin_api_123\n\nGITHUB_WEBHOOK_SECRET=new\nCLAUDEAR_LOG=info\n\nGITLAB_WEBHOOK_SECRET=gitlab\nLINEAR_WEBHOOK_SECRET=linear\n"
    );
}

#[test]
fn test_values_are_quoted_so_a_shell_reads_them_literally() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(".env");

    environment::update(
        &path,
        &values(&[
            (
                environment::DISCORD_WEBHOOK_URL,
                "https://discord.com/api/webhooks/1/a-b_c",
            ),
            (environment::GITHUB_APP_CLIENT_SECRET, "with spaces"),
            (environment::GITHUB_APP_ID, "123456"),
            (environment::GITHUB_WEBHOOK_SECRET, "pa$$word#1"),
            (environment::LINEAR_WEBHOOK_SECRET, "it's"),
            (environment::SENTRY_CLIENT_SECRET, "line\nINJECTED=1"),
        ]),
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        concat!(
            "DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/1/a-b_c\n",
            "GITHUB_APP_CLIENT_SECRET='with spaces'\n",
            "GITHUB_APP_ID=123456\n",
            "GITHUB_WEBHOOK_SECRET='pa$$word#1'\n",
            "LINEAR_WEBHOOK_SECRET=\"it's\"\n",
            "SENTRY_CLIENT_SECRET=\"line\\nINJECTED=1\"\n",
        )
    );
}

#[test]
fn test_a_file_written_by_the_previous_writer_still_reads_and_keeps_its_lines() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(".env");
    let previous = "EXISTING=keep\n\n# Auto-configured webhook secrets\nLINEAR_WEBHOOK_SECRET=\"with spaces\"\nSENTRY_CLIENT_SECRET=\"say \\\"hi\\\" \\\\ $HOME\"\n";
    fs::write(&path, previous).unwrap();

    environment::update(
        &path,
        &values(&[(environment::TELEGRAM_WEBHOOK_SECRET, "telegram")]),
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{previous}\nTELEGRAM_WEBHOOK_SECRET=telegram\n")
    );
    assert_eq!(
        read(&path),
        values(&[
            ("EXISTING", "keep"),
            (environment::LINEAR_WEBHOOK_SECRET, "with spaces"),
            (environment::SENTRY_CLIENT_SECRET, "say \"hi\" \\ $HOME"),
            (environment::TELEGRAM_WEBHOOK_SECRET, "telegram"),
        ])
    );
}

#[test]
fn test_writing_the_same_values_again_changes_nothing() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(".env");
    let secrets = values(&[
        (environment::GITHUB_WEBHOOK_SECRET, "with spaces"),
        (environment::LINEAR_WEBHOOK_SECRET, "abc123"),
    ]);
    environment::update(&path, &secrets).unwrap();
    let first = fs::read_to_string(&path).unwrap();

    environment::update(&path, &secrets).unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), first);
}

#[test]
fn test_a_missing_file_and_its_directory_are_created() {
    let directory = TempDir::new().unwrap();
    let nested = directory.path().join("nested");
    let path = nested.join(".env");

    environment::update(&path, &values(&[(environment::GITHUB_APP_ID, "1")])).unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), "GITHUB_APP_ID=1\n");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "directory mode was {mode:o}");
    }
}

#[cfg(unix)]
#[test]
fn test_the_file_is_readable_only_by_its_owner() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().unwrap();
    let path = directory.path().join(".env");
    fs::write(&path, "EXISTING=value\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    environment::update(
        &path,
        &values(&[(environment::GITHUB_WEBHOOK_SECRET, "secret")]),
    )
    .unwrap();

    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mode was {mode:o}");
}

#[cfg(unix)]
#[test]
fn test_a_read_only_directory_fails_with_a_config_error_naming_the_cause() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().unwrap();
    let locked = directory.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();

    let probe = locked.join("probe");
    if fs::write(&probe, "").is_ok() {
        fs::remove_file(&probe).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipped: this user can write to a 0500 directory");
        return;
    }

    let result = environment::update(
        &locked.join(".env"),
        &values(&[(environment::GITHUB_WEBHOOK_SECRET, "secret")]),
    );

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    match result {
        Err(Error::Config(message)) => {
            assert!(message.contains("Failed to write"), "{message}");
            assert!(message.contains("Permission denied"), "{message}");
        }
        other => panic!("expected a config error, got {other:?}"),
    }
}

#[test]
fn test_probe_leaves_no_trace_when_the_file_and_its_directory_are_missing() {
    let directory = TempDir::new().unwrap();
    let nested = directory.path().join("nested");
    let path = nested.join(".env");

    environment::probe(&path).unwrap();

    assert!(!nested.exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn test_probe_leaves_an_existing_file_untouched() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(".env");
    fs::write(&path, "EXISTING=value\n").unwrap();

    environment::probe(&path).unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), "EXISTING=value\n");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn test_probe_fails_like_update_when_the_directory_is_read_only() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().unwrap();
    let locked = directory.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();

    let probe = locked.join("probe");
    if fs::write(&probe, "").is_ok() {
        fs::remove_file(&probe).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipped: this user can write to a 0500 directory");
        return;
    }

    let probed = environment::probe(&locked.join(".env"));
    let nested = environment::probe(&locked.join("missing").join(".env"));

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    for result in [probed, nested] {
        match result {
            Err(Error::Config(message)) => {
                assert!(message.contains("Failed to write"), "{message}");
                assert!(message.contains("Permission denied"), "{message}");
            }
            other => panic!("expected a config error, got {other:?}"),
        }
    }
}
