use anyhow::{Context, ensure};
use clap::{Args as ClapArgs, Subcommand};

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create one single-use invitation; the secret URL is printed only once
    Create {
        #[arg(long)]
        label: String,
        #[arg(long, default_value_t = 168, value_parser = clap::value_parser!(u32).range(1..=720))]
        expires_in_hours: u32,
    },
    /// List invitation IDs, labels and admitted Telegram users (without secrets)
    List,
    /// Revoke an invitation and access granted through it
    Revoke { id: String },
    /// Revoke a Telegram user's access and web sessions
    RevokeUser { user_id: u64 },
}

pub fn run(args: Args) -> anyhow::Result<()> {
    let root = crate::config::data_dir()?;
    let auth = crate::web::auth::AuthStore::new(&root);
    match args.command {
        Command::Create {
            label,
            expires_in_hours,
        } => {
            let config = crate::config::Config::load()?;
            config.validate_owner()?;
            ensure!(!config.gateway.is_empty(), "A Telegram gateway is required");
            let settings = crate::web::Settings::from_config(&config.web)?
                .context("Enable web access before creating invitations")?;
            let now = chrono::Utc::now().timestamp();
            let invite =
                auth.create_invite(&label, now + i64::from(expires_in_hours) * 3600, now)?;
            let url = invitation_url(&settings, &invite.token)?;
            let expires = chrono::DateTime::from_timestamp(invite.expires, 0)
                .context("Invalid invitation expiry")?
                .with_timezone(&chrono::Local);
            println!(
                "Приглашение для «{}»\n\n{url}\n\nДействует до: {}\nСсылка одноразовая. Передайте её лично.\n\nОтозвать: kodrik invites revoke {}",
                label.trim(),
                expires.format("%d.%m.%Y %H:%M %:z"),
                invite.id,
            );
        }
        Command::List => println!("{}", serde_json::to_string_pretty(&auth.list_invites()?)?),
        Command::Revoke { id } => {
            ensure!(auth.revoke_invite(&id)?, "Invitation not found");
            println!("Invitation and its granted access revoked.");
        }
        Command::RevokeUser { user_id } => {
            ensure!(auth.revoke_user(user_id)?, "User has no invitation access");
            println!("User access and web sessions revoked.");
        }
    }
    Ok(())
}

fn invitation_url(settings: &crate::web::Settings, token: &str) -> anyhow::Result<String> {
    let mut url = settings.base_url.join("i")?;
    url.set_fragment(Some(token));
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitation_secret_is_only_in_fragment_and_proxy_prefix_is_preserved() {
        let settings = crate::web::Settings::from_config(&crate::config::WebConfig {
            enabled: true,
            base_url: Some("https://example.com/kodrik/".into()),
            ..Default::default()
        })
        .unwrap()
        .unwrap();
        let code = "AbCdEfGhIjKlMnOpQrSt_w";
        let url: reqwest::Url = invitation_url(&settings, code).unwrap().parse().unwrap();
        assert_eq!(url.path(), "/kodrik/i");
        assert_eq!(url.query(), None);
        assert_eq!(url.fragment(), Some(code));
    }
}
