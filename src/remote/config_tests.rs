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

fn grant_fixture() -> (
    tempfile::TempDir,
    RemoteConfig,
    super::protocol::EnvironmentManifest,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote.toml");
    std::fs::write(&path, "server = 'https://example.com/'").unwrap();
    let config = RemoteConfig::load(&path).unwrap();
    let manifest = super::protocol::EnvironmentManifest {
        label: "Laptop".into(),
        project_root: "/project/a".into(),
        tools: vec![],
        skills: vec![],
        project_instructions: None,
        warnings: vec![],
    };
    (dir, config, manifest)
}

#[test]
fn manifest_authorized_without_grant_is_read_only() {
    let (_dir, config, manifest) = grant_fixture();
    assert!(!config.manifest_authorized(&manifest).unwrap());
    assert!(!config.credential_file.parent().unwrap().exists());
    assert!(!config.credential_file.exists());
    assert!(!config.credential_file.with_extension("grant").exists());
}

#[test]
fn manifest_authorized_recognizes_cached_grant_without_writing() {
    let (_dir, config, manifest) = grant_fixture();
    config.authorize_manifest(&manifest, || Ok(true)).unwrap();
    let path = config.credential_file.with_extension("grant");
    let saved = std::fs::read(&path).unwrap();
    assert_eq!(saved.len(), 64);
    assert!(saved.iter().all(u8::is_ascii_hexdigit));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert!(config.manifest_authorized(&manifest).unwrap());
    config
        .authorize_manifest(&manifest, || panic!("cached grant asked again"))
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o400
    );
    assert!(!config.credential_file.exists());
}

#[test]
fn manifest_authorized_requires_exact_manifest_and_config() {
    let (_dir, config, manifest) = grant_fixture();
    config.authorize_manifest(&manifest, || Ok(true)).unwrap();
    let path = config.credential_file.with_extension("grant");
    let saved = std::fs::read(&path).unwrap();
    let mut changed = manifest.clone();
    changed.project_root = "/project/b".into();
    assert!(!config.manifest_authorized(&changed).unwrap());
    let mut changed = config.clone();
    changed.server = "https://other.example/".parse().unwrap();
    assert!(!changed.manifest_authorized(&manifest).unwrap());
    let mut changed = config.clone();
    changed.allow_shell = true;
    assert!(!changed.manifest_authorized(&manifest).unwrap());
    let mut changed = manifest.clone();
    changed.label.clear();
    assert!(config.manifest_authorized(&changed).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), saved);
}

#[test]
fn manifest_authorized_propagates_unsafe_grant_errors() {
    for case in [
        "public",
        "oversized",
        "invalid_utf8",
        "directory",
        "symlink",
        "dangling",
    ] {
        let (dir, config, manifest) = grant_fixture();
        let path = config.credential_file.with_extension("grant");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        match case {
            "directory" => std::fs::create_dir(&path).unwrap(),
            "symlink" | "dangling" => {
                let target = dir.path().join("target");
                if case == "symlink" {
                    std::fs::write(&target, "0".repeat(64)).unwrap();
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                }
                symlink(target, &path).unwrap();
            }
            _ => {
                let bytes = match case {
                    "oversized" => vec![b'0'; 65],
                    "invalid_utf8" => vec![0xff],
                    _ => vec![b'0'; 64],
                };
                std::fs::write(&path, bytes).unwrap();
                let mode = if case == "public" { 0o644 } else { 0o600 };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        }
        assert!(
            config.manifest_authorized(&manifest).is_err(),
            "accepted {case}"
        );
        assert!(
            config
                .authorize_manifest(&manifest, || panic!("unsafe grant prompted: {case}"))
                .is_err(),
            "authorization accepted {case}"
        );
    }
}
