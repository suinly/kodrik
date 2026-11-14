use crate::{
    GatewayRunner,
    bootstrap::build_app,
    config::{Config, GatewayConfig},
    interfaces::TelegramInterface,
};

/// Returns true only after a web-requested restart has drained gateway work.
pub async fn run(config: &Config) -> anyhow::Result<bool> {
    let data = crate::config::data_dir()?;
    let auth = crate::web::auth::AuthStore::new(&data);
    auth.configure_owner(config.owner.as_ref().map(|owner| owner.user_id.get()))?;
    if let Err(error) = tokio::task::spawn_blocking(move || auth.clear_telegram_bots()).await? {
        log::warn!("Telegram web login unavailable: {error:#}");
    }
    let app = build_app(config).await?;
    if config.owner.is_some() {
        app.agent_runner
            .sqlite_store()
            .ok_or_else(|| anyhow::anyhow!("Environments require durable owner storage"))?
            .remote_recover()
            .await?;
        app.agent_runner
            .enable_environments(crate::web::auth::AuthStore::new(&data))?;
    }
    let control = crate::web::gateway_control::GatewayControl::new();
    let users = std::sync::Arc::new(crate::users::UserSpaces::new(config, &data, app.clone()));
    let mut web_chat = None;
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
                .with_datasets(app.datasets.clone())
                .with_users(users.clone())
                .with_push(settings.clone(), crate::web::auth::AuthStore::new(&data))
                .await?;
            chat.recover_users().await?;
            web_chat = Some(chat.clone());
            Some((
                listener,
                crate::web::server::router_with_resources(
                    settings.clone(),
                    &data,
                    Some(chat),
                    Some(app.datasets.clone()),
                )
                .merge(crate::web::gateway_control::router(
                    settings,
                    crate::web::auth::AuthStore::new(&data),
                    control.clone(),
                    data.join("config.toml"),
                )),
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
    let outcome = tokio::select! {
        result=&mut work=>{result.map(|_| false)},
        _=control.wait()=>{
            log::info!("Suspending gateway for web-requested restart");
            app.agent_runner.suspend();
            work.await.map(|_| true)
        },
        reason=super::shutdown_signal()=>{
            log::info!("Suspending gateway after {reason}");
            app.agent_runner.suspend();
            work.await.map(|_| false)
        }
    };
    app.agent_runner.suspend();
    if let Some(chat) = web_chat {
        chat.drain().await;
    }
    outcome
}
