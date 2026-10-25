pub async fn evaluate_views(
    input: &std::path::Path,
    output: &std::path::Path,
) -> anyhow::Result<()> {
    crate::web::views::worker(input, output).await
}

pub fn set_password() -> anyhow::Result<()> {
    println!(
        "Use at least 15 characters. Common passwords and repeating short patterns are not accepted."
    );
    let password = rpassword::prompt_password("New web password: ")?;
    let confirmation = rpassword::prompt_password("Confirm password: ")?;
    anyhow::ensure!(password == confirmation, "Passwords do not match");
    crate::web::auth::AuthStore::new(&crate::config::data_dir()?).set_password(&password)?;
    println!("Web password set. Existing web sessions have been revoked.");
    Ok(())
}
