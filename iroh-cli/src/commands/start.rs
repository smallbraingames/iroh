//! Define commands to manage the start of the iroh node.

use crate::config::NodeConfig;
use anyhow::Result;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use iroh::{
    net::{
        discovery::pkarr::dht::DhtDiscovery,
        endpoint::{TransportConfig, VarInt},
        key::SecretKey,
        relay::{RelayMap, RelayMode},
    },
    node::{DiscoveryConfig, Node, RpcStatus, DEFAULT_RPC_ADDR},
};
use std::{
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tracing::{info_span, trace, Instrument};

/// Whether to stop the node after running a command or run forever until stopped.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RunType {
    /// Run a single command, and then shutdown the node. Allow to abort with Ctrl-C.
    SingleCommandAbortable,
    /// Run a single command, and then shutdown the node. Do not abort on Ctrl-C (expects Ctrl-C to be handled internally).
    SingleCommandNoAbort,
    /// Run until manually stopped (through Ctrl-C or shutdown RPC command)
    UntilStopped,
}

/// Error to show that iroh is already running in some port.
#[derive(thiserror::Error, Debug)]
#[error("iroh is already running on port {0}")]
pub struct AlreadyRunningError(u16);

/// Runs an iroh node with a given command.
pub async fn run_with_command<F, T>(
    config: &NodeConfig,
    iroh_data_root: &Path,
    rpc_addr: Option<SocketAddr>,
    run_type: RunType,
    command: F,
) -> Result<()>
where
    F: FnOnce(iroh::client::Iroh) -> T + Send + 'static,
    T: Future<Output = Result<()>> + 'static,
{
    let _guard = crate::logging::init_terminal_and_file_logging(&config.file_logs, iroh_data_root)?;
    let metrics_fut = start_metrics_server(config.metrics_addr);
    let metrics_dumper_fut =
        start_metrics_dumper(config.metrics_dump_path.clone(), Duration::from_millis(100));

    let res = run_with_command_inner(config, iroh_data_root, rpc_addr, run_type, command).await;

    // If `Some`thing is returned, it means the starting has failed and the tasks should be aborted.
    if let Some(metrics_fut) = metrics_fut {
        metrics_fut.abort();
    }
    // If `Some`thing is returned, it means the starting has failed and the tasks should be aborted.
    if let Some(metrics_dumper_fut) = metrics_dumper_fut {
        metrics_dumper_fut.abort();
    }

    let (clear_rpc, res) = match res {
        Ok(()) => (true, res),
        Err(e) => match e.downcast::<AlreadyRunningError>() {
            // iroh is already running in a different process, do no remove the rpc lockfile
            Ok(already_running) => (false, Err(already_running.into())),
            Err(e) => (true, Err(e)),
        },
    };

    if clear_rpc {
        RpcStatus::clear(iroh_data_root).await?;
    }

    res
}

/// Runs an iroh node with the given command (private function).
async fn run_with_command_inner<F, T>(
    config: &NodeConfig,
    iroh_data_root: &Path,
    rpc_addr: Option<SocketAddr>,
    run_type: RunType,
    command: F,
) -> Result<()>
where
    F: FnOnce(iroh::client::Iroh) -> T + Send + 'static,
    T: Future<Output = Result<()>> + 'static,
{
    trace!(?config, "using config");
    let relay_map = config.relay_map()?;

    let spinner = create_spinner("Iroh booting...");
    let node = start_node(iroh_data_root, rpc_addr, relay_map).await?;
    drop(spinner);

    eprintln!("{}", welcome_message(&node)?);

    let client = node.client().clone();

    let mut command_task = node.local_pool_handle().spawn(move || {
        async move {
            match command(client).await {
                Err(err) => Err(err),
                Ok(()) => {
                    // keep the task open forever if not running in single-command mode
                    if run_type == RunType::UntilStopped {
                        futures_lite::future::pending().await
                    }
                    Ok(())
                }
            }
        }
        .instrument(info_span!("command"))
    });

    tokio::select! {
        biased;
        // always abort on signal-c
        _ = tokio::signal::ctrl_c(), if run_type != RunType::SingleCommandNoAbort => {
            command_task.abort();
            node.shutdown().await?;
        }
        // abort if the command task finishes (will run forever if not in single-command mode)
        res = &mut command_task => {
            let _ = node.shutdown().await;
            res??;
        }
    }
    Ok(())
}

/// Starts an iroh node.
pub(crate) async fn start_node(
    iroh_data_root: &Path,
    rpc_addr: Option<SocketAddr>,
    relay_map: Option<RelayMap>,
) -> Result<Node<iroh::blobs::store::fs::Store>> {
    let rpc_status = RpcStatus::load(iroh_data_root).await?;
    match rpc_status {
        RpcStatus::Running { port, .. } => {
            return Err(AlreadyRunningError(port).into());
        }
        RpcStatus::Stopped => {
            // all good, we can start the node
        }
    }

    let relay_mode = match relay_map {
        None => RelayMode::Default,
        Some(relay_map) => RelayMode::Custom(relay_map),
    };

    let rpc_addr = rpc_addr.unwrap_or(DEFAULT_RPC_ADDR);
    let secret_key_path = iroh_data_root.join("keypair");
    println!("secret key path {}", secret_key_path.display());

    let secret_key = SecretKey::try_from_openssh(std::fs::read(&secret_key_path)?)?;
    println!("secret key: {}", secret_key.to_string());

    let builder = DhtDiscovery::builder().dht(true).n0_dns_pkarr_relay();
    let discovery = builder.secret_key(secret_key).build().unwrap();
    let discovery_config = DiscoveryConfig::Custom(Box::new(discovery));

    let mut transport_config = TransportConfig::default();
    transport_config.keep_alive_interval(Some(Duration::from_millis(250)));
    transport_config.max_idle_timeout(Some(VarInt::from_u32(1_000).into()));

    println!("starting w/ pkarr discovery");
    Node::persistent(iroh_data_root)
        .await?
        .relay_mode(relay_mode)
        .transport_config(transport_config)
        .node_discovery(discovery_config)
        .enable_docs()
        .enable_rpc_with_addr(rpc_addr)
        .await?
        .spawn()
        .await
}

/// Creates a welcome message for the given [`Node`].
fn welcome_message<B: iroh::blobs::store::Store>(node: &Node<B>) -> Result<String> {
    let msg = format!(
        "{}\nNode ID: {}\n",
        "Iroh is running".green(),
        node.node_id()
    );

    Ok(msg)
}

/// Creates a nice spinner.
fn create_spinner(msg: &'static str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(80));
    pb.set_draw_target(ProgressDrawTarget::stderr());
    pb.set_style(
        ProgressStyle::with_template("{spinner:.blue} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg);
    pb.with_finish(indicatif::ProgressFinish::AndClear)
}

/// Start an iroh metrics server to serve the OpenMetrics endpoint.
///
/// Returns `None` if succeeded; otherwise, returns the `JoinHandle` with which the task can be aborted.
pub fn start_metrics_server(
    metrics_addr: Option<SocketAddr>,
) -> Option<tokio::task::JoinHandle<()>> {
    // doesn't start the server if the address is None
    if let Some(metrics_addr) = metrics_addr {
        // metrics are initilaized in iroh::node::Node::spawn
        // here we only start the server
        return Some(tokio::task::spawn(async move {
            if let Err(e) = iroh_metrics::metrics::start_metrics_server(metrics_addr).await {
                eprintln!("Failed to start metrics server: {e}");
            }
        }));
    }
    tracing::info!("Metrics server not started, no address provided");
    None
}

/// Starts an iroh metrics dumper service.
///
/// Returns `None` if succeeded; otherwise, returns the `JoinHandle` with which the task can be aborted.
pub fn start_metrics_dumper(
    path: Option<PathBuf>,
    interval: Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    // doesn't start the dumper if the address is None
    Some(tokio::task::spawn(async move {
        if let Some(path) = path {
            if let Err(e) = iroh_metrics::metrics::start_metrics_dumper(path, interval).await {
                eprintln!("Failed to start metrics dumper: {e}");
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use iroh::util::path::IrohPaths;

    #[tokio::test]
    async fn test_run_rpc_lock_file() -> Result<()> {
        let data_dir = tempfile::TempDir::with_prefix("rpc-lock-file-")?;
        let lock_file_path = data_dir
            .path()
            .join(IrohPaths::RpcLock.with_root(data_dir.path()));
        let data_dir_path = data_dir.path().to_path_buf();

        let (ready_s, ready_r) = tokio::sync::oneshot::channel();
        let (close_s, close_r) = tokio::sync::oneshot::channel();

        // run the first start command, using channels to coordinate so we know when the node has fully booted up, and when we need to shut the node down
        let start = tokio::spawn(async move {
            run_with_command(
                &NodeConfig::default(),
                &data_dir_path,
                None,
                RunType::SingleCommandAbortable,
                |_| async move {
                    // inform the test the node is booted up
                    ready_s.send(()).unwrap();

                    // wait until the test tells us to shut down the node
                    close_r.await?;
                    Ok(())
                },
            )
            .await
        });

        // allow ample time for iroh to boot up
        if tokio::time::timeout(Duration::from_millis(20000), ready_r)
            .await
            .is_err()
        {
            start.abort();
            bail!("First `run_with_command` call never started");
        }

        // ensure the rpc lock file exists
        if !lock_file_path.try_exists()? {
            start.abort();
            bail!("First `run_with_command` call never made the rpc lockfile");
        }

        // run the second command, this should fail
        if run_with_command(
            &NodeConfig::default(),
            data_dir.path(),
            None,
            RunType::SingleCommandAbortable,
            |_| async move { Ok(()) },
        )
        .await
        .is_ok()
        {
            start.abort();
            bail!("Second `run_with_command` call should return error");
        }

        // ensure the rpc lock file still exists
        if !lock_file_path.try_exists()? {
            start.abort();
            bail!("Second `run_with_command` removed the rpc lockfile");
        }

        // inform the node it should close
        close_s.send(()).unwrap();

        // wait for the node to close
        if tokio::time::timeout(Duration::from_secs(5), start)
            .await
            .is_err()
        {
            bail!("First `run_with_command` never closed");
        }

        // ensure the lockfile no longer exists
        if lock_file_path.try_exists()? {
            bail!("First `run_with_command` closed without removing the rpc lockfile");
        }
        Ok(())
    }
}
