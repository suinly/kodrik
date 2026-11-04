use super::config::RemoteConfig;
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn remote_config_needs_no_provider_defaults_to_no_shell_and_preserves_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote.toml");
    std::fs::write(
        &path,
        "server = 'https://example.com/kodrik/'\nlabel = 'Laptop'\n",
    )
    .unwrap();
    let config = RemoteConfig::load(&path).unwrap();
    assert!(!config.allow_shell);
    assert!(config.mcp.is_empty());
    assert!(config.skill_roots.is_empty());
    assert_eq!(config.server.as_str(), "https://example.com/kodrik/");
    assert_eq!(
        config.credential_file,
        dir.path().join("remote/device.json")
    );
}

#[test]
fn remote_config_rejects_insecure_urls_unknown_options_and_unselected_optional_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote.toml");
    for text in [
        "server = 'http://example.com/'",
        "server = 'https://user:secret@example.com/'",
        "server = 'https://example.com/?token=secret'",
        "server = 'https://example.com/'\nallow_shel = true",
        "server = 'https://example.com/'\noptional_mcp = ['missing']",
        "server = 'https://example.com/'\nlabel = \"bad\\nlabel\"",
    ] {
        std::fs::write(&path, text).unwrap();
        assert!(RemoteConfig::load(&path).is_err(), "accepted {text}");
    }
    std::fs::write(&path, "server = 'http://127.0.0.1:1234/'").unwrap();
    assert!(RemoteConfig::load(&path).is_ok());
}

#[test]
fn credential_save_is_private_one_shot_and_rejects_symlinks_or_public_files() {
    use super::client::Credential;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote/device.json");
    let credential = Credential {
        device_id: uuid::Uuid::new_v4(),
        token: "fixture-token".into(),
    };
    credential.save(&path).unwrap();
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(Credential::load(&path).unwrap().token, "fixture-token");
    assert!(
        credential.save(&path).is_err(),
        "silently replaced an existing device"
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Credential::load(&path).is_err());
    let link = dir.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(Credential::load(&link).is_err());
    assert!(credential.save(&link).is_err());
}

#[test]
fn local_access_grant_is_bound_to_server_root_and_manifest_and_denial_saves_nothing() {
    use super::protocol::EnvironmentManifest;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote.toml");
    std::fs::write(&path, "server = 'https://example.com/'").unwrap();
    let mut config = RemoteConfig::load(&path).unwrap();
    let mut manifest = EnvironmentManifest {
        label: "Laptop".into(),
        project_root: "/project/a".into(),
        tools: vec![],
        skills: vec![],
        project_instructions: None,
        warnings: vec![],
    };
    assert!(config.authorize_manifest(&manifest, || Ok(false)).is_err());
    let confirmations = std::cell::Cell::new(0);
    config
        .authorize_manifest(&manifest, || {
            confirmations.set(confirmations.get() + 1);
            Ok(true)
        })
        .unwrap();
    config
        .authorize_manifest(&manifest, || panic!("identical grant asked twice"))
        .unwrap();
    assert_eq!(confirmations.get(), 1);
    manifest.project_root = "/project/b".into();
    assert!(config.authorize_manifest(&manifest, || Ok(false)).is_err());
    config.server = "https://other.example/".parse().unwrap();
    assert!(config.authorize_manifest(&manifest, || Ok(false)).is_err());
}
