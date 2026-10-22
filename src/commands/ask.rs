use crate::{bootstrap::build_app, config::Config};

pub async fn run(config: &Config, prompt: impl Into<String>) -> anyhow::Result<()> {
    let app = build_app(config).await?;

    let work = async {
        let report = app
            .agent_runner
            .recover(crate::agent::recovery::RecoveryTarget::OneShots)
            .await?;
        for (_, answer) in report.completed {
            println!("{}", answer.content);
        }
        for (id, reason) in report.blocked {
            eprintln!("Recovery blocked for {id}: {reason}");
        }
        app.agent_runner.run_once(prompt).await
    };
    tokio::pin!(work);
    let response = tokio::select! {
        result=&mut work=>result?,
        reason=super::shutdown_signal()=>{
            log::info!("Suspending CLI after {reason}");
            app.agent_runner.suspend();
            work.await?
        }
    };

    println!("{response}");
    app.agent_runner.finish_learning().await;

    Ok(())
}
