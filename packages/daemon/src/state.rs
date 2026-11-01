use crate::{
    agent, automation, config, daemon_admin, daemon_control::WorkerExit, files, git, mcp, plugins,
    projects,
    providers::registry::ProviderRegistry,
    terminals,
    transport::{self, RequestContext},
    usage::{UsageContext, UsageService},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU32, Ordering},
    },
};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

const TRANSPORT: u32 = 1;
const PROVIDERS: u32 = 2;
const AGENTS: u32 = 4;
const AUTOMATION: u32 = 8;
const FILES: u32 = 16;
const GIT: u32 = 32;
const TERMINALS: u32 = 64;
const PLUGINS: u32 = 256;

const EXIT_STOP: u8 = 1;
const EXIT_RESTART: u8 = 2;
const EXIT_FAILURE: u8 = 3;

pub struct AppState {
    pub home: PathBuf,
    pub user_home: PathBuf,
    pub executable: PathBuf,
    pub cancel: CancellationToken,
    pub config: Arc<config::ConfigService>,
    pub providers: Arc<ProviderRegistry>,
    pub agents: Arc<agent::AgentManager>,
    pub projects: Arc<projects::ProjectService>,
    pub git: Arc<git::GitService>,
    pub files: Arc<files::FileService>,
    pub terminals: Arc<terminals::TerminalService>,
    pub automation: Arc<automation::AutomationService>,
    pub mcp: Arc<mcp::McpService>,
    pub transport: Arc<transport::TransportService>,
    pub plugins: Arc<plugins::PluginService>,
    pub daemon_admin: Arc<daemon_admin::DaemonAdminService>,
    pub usage: Arc<UsageService>,
    started: AtomicU32,
    shutdown_kind: AtomicU8,
    shutdown_lock: Mutex<()>,
    failure: std::sync::Mutex<Option<anyhow::Error>>,
}

impl AppState {
    pub fn new(home: PathBuf, user_home: PathBuf, executable: PathBuf) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            config: Arc::new(config::ConfigService::new(weak.clone())),
            providers: Arc::new(ProviderRegistry::new(weak.clone())),
            agents: Arc::new(agent::AgentManager::new(weak.clone())),
            projects: Arc::new(projects::ProjectService::new(weak.clone())),
            git: Arc::new(git::GitService::new(weak.clone())),
            files: Arc::new(files::FileService::new(weak.clone())),
            terminals: Arc::new(terminals::TerminalService::new(weak.clone())),
            automation: Arc::new(automation::AutomationService::new(weak.clone())),
            mcp: Arc::new(mcp::McpService::new(weak.clone())),
            transport: Arc::new(transport::TransportService::new(weak.clone())),
            plugins: Arc::new(plugins::PluginService::new(weak.clone())),
            daemon_admin: Arc::new(daemon_admin::DaemonAdminService::new(weak.clone())),
            usage: Arc::new(UsageService::new(user_home.clone())),
            home,
            user_home,
            executable,
            cancel: CancellationToken::new(),
            started: AtomicU32::new(0),
            shutdown_kind: AtomicU8::new(0),
            shutdown_lock: Mutex::new(()),
            failure: std::sync::Mutex::new(None),
        })
    }

    fn starting(&self, service: u32) -> Result<()> {
        if self.cancel.is_cancelled() {
            bail!("Daemon startup cancelled");
        }
        // Record attempts before fallible IO so partially initialized owners are drained.
        self.started.fetch_or(service, Ordering::Release);
        Ok(())
    }

    async fn initialize(&self, options: &Value) -> Result<()> {
        self.config.initialize_with_overrides(options).await?;
        crate::logging::initialize(&self.config.snapshot()["log"])?;
        self.starting(TRANSPORT)?;
        self.transport.prepare().await?;
        self.projects.initialize().await?;
        self.starting(GIT)?;
        self.git.initialize().await?;
        self.starting(FILES)?;
        self.files.initialize().await?;
        self.starting(TERMINALS)?;
        self.terminals.initialize().await?;
        self.starting(PROVIDERS)?;
        self.providers.initialize().await?;
        self.starting(AGENTS)?;
        self.agents.initialize().await?;
        // Extension startup may call domain APIs through its prepared in-process channel.
        self.starting(PLUGINS)?;
        self.plugins.initialize().await?;
        let configuration = self.config.snapshot();
        self.providers
            .apply_config(&configuration, &configuration)
            .await?;
        self.projects.start_observations().await?;
        self.starting(AUTOMATION)?;
        self.automation.initialize().await?;
        if self.cancel.is_cancelled() {
            bail!("Daemon startup cancelled");
        }
        self.transport.start().await?;
        Ok(())
    }

    pub fn request_shutdown(&self, restart: bool) {
        self.request_exit(if restart { EXIT_RESTART } else { EXIT_STOP });
    }

    pub(crate) fn request_failure(&self, error: anyhow::Error) {
        let mut failure = self.failure.lock().unwrap();
        if failure.is_none() {
            *failure = Some(error);
        }
        drop(failure);
        self.request_exit(EXIT_FAILURE);
    }

    /// The first intent wins, so teardown failures never rewrite an explicit stop/restart.
    fn request_exit(&self, kind: u8) {
        let _ = self.shutdown_kind.compare_exchange(0, kind, Ordering::AcqRel, Ordering::Acquire);
        self.cancel.cancel();
    }

    /// Fixes the exit intent: any exit not explicitly requested by now (startup
    /// errors included) is a failure, keeping supervisor crash restart and
    /// uncommitted-update rollback available.
    fn settle_exit(&self) -> WorkerExit {
        self.request_exit(EXIT_FAILURE);
        match self.shutdown_kind.load(Ordering::Acquire) {
            EXIT_STOP => WorkerExit::Stop,
            EXIT_RESTART => WorkerExit::Restart,
            _ => WorkerExit::Failure,
        }
    }

    pub async fn dispatch(&self, context: &RequestContext, message: &Value) -> Result<()> {
        if self.cancel.is_cancelled() {
            bail!("Daemon is shutting down");
        }
        if message["type"] == "client_heartbeat" {
            self.transport.handle(context, message).await?;
            self.terminals.handle(context, message).await?;
            self.agents.handle(context, message).await?;
            return Ok(());
        }
        let _update_admission = crate::daemon_admin::update_operation::wire_admission(self, message)?;
        if self.transport.handle(context, message).await?
            || self.daemon_admin.handle(context, message).await?
            || self.config.handle(context, message).await?
            || self.providers.handle(context, message).await?
            || self.handle_usage(context, message).await?
            || self.plugins.handle(context, message).await?
            || self.projects.handle(context, message).await?
            || self.git.handle(context, message).await?
            || self.files.handle(context, message).await?
            || self.terminals.handle(context, message).await?
            || self.automation.handle(context, message).await?
            || self.agents.handle(context, message).await?
        {
            return Ok(());
        }
        bail!(
            "Unhandled session message '{}'",
            message["type"].as_str().unwrap_or("")
        )
    }

    async fn usage_context(&self) -> Result<UsageContext> {
        let sessions = self.agents.usage_session_snapshots().await?;
        Ok(UsageContext {
            env: self.config.environment(),
            user_home: Some(self.user_home.clone()),
            provider_specs: self.providers.specs(),
            sessions,
            muse_providers: self.providers.muse_instances(),
            plugins: Some(self.plugins.clone()),
        })
    }

    async fn handle_usage(&self, context: &RequestContext, message: &Value) -> Result<bool> {
        let kind = message["type"].as_str().unwrap_or("");
        if !matches!(
            kind,
            "usage.list_reports.request" | "provider.usage.list.request"
        ) {
            return Ok(false);
        }
        let request = transport::request_id(message).context("requestId is required")?;
        if kind == "provider.usage.list.request" {
            let result = async {
                let usage_context = self.usage_context().await?;
                self.usage
                    .list_legacy(&usage_context, context.cancellation())
                    .await
            }
            .await;
            match result {
                Ok(mut payload)=>{payload["requestId"]=json!(request);context.send(json!({"type":"provider.usage.list.response","payload":payload})).await?;},
                Err(error)=>context.send(json!({"type":"rpc_error","payload":{"requestId":request,"requestType":kind,"code":"provider_usage_list_failed","error":format!("Failed to list provider usage: {error}")}})).await?,
            }
            return Ok(true);
        }
        let mut unavailable_agents = Vec::new();
        let mut unavailable_sources = Vec::new();
        let result:Result<()>=async {
            let usage_context=self.usage_context().await?;
            unavailable_agents=usage_context.sessions.iter()
                .filter(|session| session["_runtimeUnavailable"] == true)
                .map(|session| session["agentId"].clone()).collect();
            let (sender,mut receiver)=mpsc::channel(32);
            let result={
                let fetch=self.usage.list_reports_streaming(message,&usage_context,context.cancellation(),sender);
                tokio::pin!(fetch);
                loop {tokio::select! {
                    result=&mut fetch=>break result,
                    Some(report)=receiver.recv()=>context.send(json!({"type":"usage.list_reports.update","payload":{"requestId":request,"report":report}})).await?,
                }}
            };
            while let Ok(report)=receiver.try_recv(){context.send(json!({"type":"usage.list_reports.update","payload":{"requestId":request,"report":report}})).await?;}
            unavailable_sources = result?.unavailable_sources;
            Ok(())
        }.await;
        let error = result.err().map(|error| error.to_string());
        context.send(json!({"type":"usage.list_reports.response","payload":{
            "requestId":request,"complete":error.is_none() && unavailable_agents.is_empty() && unavailable_sources.is_empty(),
            "error":error,"unavailableAgentIds":unavailable_agents,"unavailableSourceIds":unavailable_sources
        }})).await?;
        Ok(true)
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _guard = self.shutdown_lock.lock().await;
        self.cancel.cancel();
        let started = self.started.swap(0, Ordering::AcqRel);
        let mut failure = None;
        macro_rules! stop {
            ($flag:expr,$name:literal,$operation:expr)=>{if started&$flag!=0 {if let Err(error)=$operation.await {tracing::error!(service=$name,%error,"Service shutdown failed");if failure.is_none(){failure=Some(error);}}}};
        }
        stop!(TRANSPORT, "transport", self.transport.shutdown());
        stop!(AUTOMATION, "automation", self.automation.shutdown());
        stop!(AGENTS, "agents", self.agents.shutdown());
        stop!(PROVIDERS, "providers", self.providers.shutdown());
        stop!(PLUGINS, "plugins", self.plugins.shutdown());
        stop!(TERMINALS, "terminals", self.terminals.shutdown());
        stop!(FILES, "files", self.files.shutdown());
        stop!(GIT, "git", self.git.shutdown());
        if let Err(error) = self.mcp.shutdown().await {
            if failure.is_none() {
                failure = Some(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

async fn termination_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {result=tokio::signal::ctrl_c()=>result?,_=terminate.recv()=>{}}
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

pub async fn run_daemon(home: PathBuf, options: Value) -> Result<i32> {
    let app = AppState::new(
        home,
        dirs::home_dir().context("User home directory is unavailable")?,
        std::env::current_exe()?,
    );
    let _worker_guard = crate::daemon_control::attach_worker(&app)?;
    let weak = Arc::downgrade(&app);
    let cancel = app.cancel.clone();
    let signal = tokio::spawn(async move {
        let outcome =
            tokio::select! {_=cancel.cancelled()=>return,result=termination_signal()=>result};
        if let Some(app) = weak.upgrade() {
            match outcome {
                Ok(()) => app.request_shutdown(false),
                Err(error) => app.request_failure(error),
            }
        }
    });
    let result:Result<()>=async {
        app.initialize(&options).await?;
        let listen=app.transport.listen_string().context("Daemon listener is not ready")?;
        let identity=app.transport.identity();
        let server_id=identity["serverId"].as_str().context("Daemon identity is missing")?;
        crate::daemon_control::publish_worker_ready(&listen,server_id)?;
        tracing::info!(%listen,%server_id,version=env!("CARGO_PKG_VERSION"),"Paseo native daemon ready");
        app.cancel.cancelled().await;
        Ok(())
    }.await;
    signal.abort();
    let _ = signal.await;
    let exit = app.settle_exit();
    if let Err(error) = crate::daemon_control::publish_worker_stopping(exit) {
        tracing::error!(%error, "Failed to announce worker shutdown");
    }
    let shutdown = app.shutdown().await;
    if let Some(error) = app.failure.lock().unwrap().take() {
        return Err(error);
    }
    result?;
    shutdown?;
    match exit {
        WorkerExit::Stop => Ok(0),
        WorkerExit::Restart => Ok(75),
        WorkerExit::Failure => bail!("Daemon stopped without a shutdown request"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> (tempfile::TempDir, Arc<AppState>) {
        let root = tempfile::tempdir().unwrap();
        let app = AppState::new(
            root.path().join("home"),
            root.path().into(),
            std::env::current_exe().unwrap(),
        );
        (root, app)
    }

    #[tokio::test]
    async fn explicit_shutdown_intent_survives_later_failures() {
        let (_root, stopped) = app();
        stopped.request_shutdown(false);
        stopped.request_failure(anyhow::anyhow!("teardown failed"));
        assert!(stopped.cancel.is_cancelled());
        assert_eq!(stopped.settle_exit(), WorkerExit::Stop);
        assert!(stopped.failure.lock().unwrap().is_some());

        let (_root, restarted) = app();
        restarted.request_shutdown(true);
        restarted.request_failure(anyhow::anyhow!("teardown failed"));
        assert_eq!(restarted.settle_exit(), WorkerExit::Restart);
    }

    #[tokio::test]
    async fn fatal_and_unrequested_exits_are_failures_not_stops() {
        let (_root, failed) = app();
        failed.request_failure(anyhow::anyhow!("listener stopped"));
        assert!(failed.cancel.is_cancelled());
        failed.request_shutdown(false);
        assert_eq!(failed.settle_exit(), WorkerExit::Failure);

        // Initialization errors return without any shutdown request.
        let (_root, startup) = app();
        assert!(!startup.cancel.is_cancelled());
        assert_eq!(startup.settle_exit(), WorkerExit::Failure);
        assert!(startup.cancel.is_cancelled());
        startup.request_shutdown(true);
        assert_eq!(startup.settle_exit(), WorkerExit::Failure);
    }
}
