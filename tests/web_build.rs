use std::{fs, process::Command};

#[test]
fn web_assets_must_exist_before_building_a_deployable_binary() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("build-script");
    assert!(
        Command::new("rustc")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/build.rs"))
            .arg("-o")
            .arg(&script)
            .status()
            .unwrap()
            .success()
    );
    let out = temp.path().join("out");
    fs::create_dir(&out).unwrap();
    let run = || {
        Command::new(&script)
            .current_dir(temp.path())
            .env("OUT_DIR", &out)
            .output()
            .unwrap()
    };
    let missing = run();
    assert!(
        !missing.status.success(),
        "missing frontend must fail the build, not gateway startup"
    );
    assert!(String::from_utf8_lossy(&missing.stderr).contains("npm run build --prefix web"));
    fs::create_dir_all(temp.path().join("web/dist/assets")).unwrap();
    assert!(!run().status.success(), "an empty dist must also fail");
    fs::write(temp.path().join("web/dist/index.html"), "<html>chat</html>").unwrap();
    fs::write(
        temp.path().join("web/dist/assets/app.js"),
        "console.log('chat')",
    )
    .unwrap();
    assert!(run().status.success());
    assert_eq!(
        fs::read_to_string(out.join("chat-assets/index.html")).unwrap(),
        "<html>chat</html>"
    );
    assert_eq!(
        fs::read_to_string(out.join("chat-assets/assets/app.js")).unwrap(),
        "console.log('chat')"
    );
}
