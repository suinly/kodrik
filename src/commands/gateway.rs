use crate::{
    GatewayRunner,
    bootstrap::build_app,
    config::{Config, GatewayConfig},
    interfaces::TelegramInterface,
};

pub async fn run(config: &Config) -> anyhow::Result<()> {
    let data = crate::config::data_dir()?;
    let auth = crate::web::auth::AuthStore::new(&data);
    auth.configure_owner(config.owner.as_ref().map(|owner| owner.user_id.get()))?;
    if let Err(error) = tokio::task::spawn_blocking(move || auth.clear_telegram_bots()).await? {
        log::warn!("Telegram web login unavailable: {error:#}");
    }
    let app = build_app(config).await?;
    let users = std::sync::Arc::new(crate::users::UserSpaces::new(config, &data, app.clone()));
    let web_server = match crate::web::Settings::from_config(&config.web)? {
        Some(settings) => {
            anyhow::ensure!(
                crate::web::chat_assets::available(),
                "Web chat assets are missing. Run npm ci --prefix web && npm run build --prefix web, then rebuild Kodrik."
            );
            let listener = tokio::net::TcpListener::bind(settings.listen).await?;
            log::info!(
                "Web listening on {} (public URL {})",
                listener.local_addr()?,
                settings.base_url
            );
            let chat = crate::web::chat::Chat::new(&app.agent_runner, &data)
                .await?
                .with_users(users.clone());
            chat.recover_users().await?;
            Some((
                listener,
                crate::web::server::router_with_resources(
                    settings,
                    &data,
                    Some(chat),
                    Some(app.datasets.clone()),
                ),
            ))
        }
        None => None,
    };
    let restart = std::sync::Arc::new(crate::gateway::restart::RestartCoordinator::new(
        data.clone(),
    ));

    let proactive_recipient = config
        .companion
        .recipient(&config.gateway, config.owner.as_ref())?;
    if config.companion.proactive_enabled && proactive_recipient.is_none() {
        log::warn!("Proactivity needs an owner, or companion.interface and companion.chat_id");
    }
    let mut gateway = GatewayRunner::new(app.agent_runner.clone())
        .with_restart(restart)
        .with_user_spaces(users)
        .with_telegram_spaces(config, &data, app.clone())
        .await?;
    if config.owner.is_none() {
        log::warn!("owner is not configured: only invited Telegram users can access the assistant");
    }

    for (key, interface) in &config.gateway {
        let GatewayConfig::Telegram { token, stream } = interface;
        gateway = gateway.add_interface(
            TelegramInterface::new(key, token, data.join("attachments"))
                .with_space_root(data.clone())
                .with_access(data.clone())
                .with_stream(*stream)
                .with_journal(app.journal.clone()),
        );
    }

    let work = async {
        match web_server {
            None => gateway.run().await,
            Some((listener, router)) if config.gateway.is_empty() => {
                crate::web::server::serve_with_shutdown(
                    listener,
                    router,
                    app.agent_runner.shutdown_receiver(),
                )
                .await
            }
            Some((listener, router)) => {
                let gateway_work = gateway.run();
                let web_work = crate::web::server::serve_with_shutdown(
                    listener,
                    router,
                    app.agent_runner.shutdown_receiver(),
                );
                tokio::pin!(gateway_work, web_work);
                tokio::select! {
                    result=&mut gateway_work=>{app.agent_runner.suspend();web_work.await?;result},
                    result=&mut web_work=>{app.agent_runner.suspend();gateway_work.await?;result},
                }
            }
        }
    };
    tokio::pin!(work);
    tokio::select! {
        result=&mut work=>result,
        reason=super::shutdown_signal()=>{
            log::info!("Suspending gateway after {reason}");
            app.agent_runner.suspend();
            work.await
        }
    }
}
