fn main() {
    println!("cargo:rerun-if-changed=skills");
    println!("cargo:rerun-if-changed=web/dist");
    assert!(
        std::path::Path::new("web/dist/index.html").is_file(),
        "Web chat assets are missing. Run npm ci --prefix web && npm run build --prefix web before building Kodrik."
    );
    let destination =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("chat-assets");
    if destination.exists() {
        std::fs::remove_dir_all(&destination).unwrap();
    }
    std::fs::create_dir_all(&destination).unwrap();
    copy_assets(std::path::Path::new("web/dist"), &destination);
}

fn copy_assets(source: &std::path::Path, destination: &std::path::Path) {
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy_assets(&entry.path(), &target);
        } else if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
