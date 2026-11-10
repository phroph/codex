//! Idle Windows MCP stdout/stderr readers must not occupy Tokio's blocking pool.

#![cfg(windows)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::LocalStdioServerLauncher;
use codex_rmcp_client::McpProtocolMode;
use codex_rmcp_client::RmcpClient;
use futures::FutureExt;
use rmcp::model::ClientCapabilities;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::ProtocolVersion;

const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_CONTENT: &[u8] = b"shared blocking pool remains available";

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(4)
        .enable_all()
        .build()?)
}

async fn start_client(
    server: &Path,
    cwd: &Path,
    protocol_mode: McpProtocolMode,
) -> Result<RmcpClient> {
    let env = (protocol_mode == McpProtocolMode::V20260728).then(|| {
        HashMap::from([(
            OsString::from("CODEX_MCP_PROTOCOL_VERSION"),
            OsString::from("2026-07-28"),
        )])
    });
    tokio::time::timeout(
        OPERATION_TIMEOUT,
        RmcpClient::new_stdio_client_with_protocol_mode(
            server.as_os_str().to_owned(),
            Vec::new(),
            env,
            &[],
            Some(cwd.to_string_lossy().into_owned()),
            Arc::new(LocalStdioServerLauncher::new(cwd.to_path_buf())),
            protocol_mode,
        ),
    )
    .await
    .context("MCP process launch exhausted the blocking pool")?
    .map_err(Into::into)
}

async fn initialize(client: &RmcpClient) -> Result<()> {
    tokio::time::timeout(
        OPERATION_TIMEOUT,
        client.initialize(
            InitializeRequestParams::new(
                ClientCapabilities::default(),
                Implementation::new("windows-stdio-pool-test", "1.0.0"),
            )
            .with_protocol_version(ProtocolVersion::V_2025_06_18),
            Some(OPERATION_TIMEOUT),
            Box::new(|_, _| {
                async {
                    Ok(ElicitationResponse {
                        action: ElicitationAction::Decline,
                        content: None,
                        meta: None,
                    })
                }
                .boxed()
            }),
        ),
    )
    .await
    .context("MCP initialization exhausted the blocking pool")??;
    Ok(())
}

async fn probe_file_read(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    let read = tokio::task::spawn_blocking(move || std::fs::read(path));
    let bytes = tokio::time::timeout(OPERATION_TIMEOUT, read)
        .await
        .context("idle MCP readers prevented an unrelated blocking file read")???;
    ensure!(
        bytes == PROBE_CONTENT,
        "file probe returned unexpected bytes"
    );
    Ok(())
}

async fn shutdown_clients(clients: Vec<RmcpClient>) -> Result<()> {
    let mut first_error = None;
    // Keep cleanup bounded even when the baseline stalls during initialization.
    // Dropping every client also invokes the owned process termination path.
    for client in clients.into_iter().rev() {
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, client.shutdown())
            .await
            .is_err()
            && first_error.is_none()
        {
            first_error = Some(anyhow::anyhow!(
                "MCP shutdown did not finish within two seconds"
            ));
        }
        drop(client);
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn idle_readers_scenario(
    server: &Path,
    cwd: &Path,
    probe: &Path,
    protocol_mode: McpProtocolMode,
) -> Result<()> {
    let mut clients = Vec::new();
    let result = async {
        // Three initialized servers exceed the two idle readers per server
        // that would otherwise consume all four shared blocking workers.
        for index in 0..3 {
            let client = start_client(server, cwd, protocol_mode)
                .await
                .with_context(|| format!("launch MCP client {index}"))?;
            clients.push(client);
            initialize(&clients[index])
                .await
                .with_context(|| format!("initialize MCP client {index}"))?;
        }
        tokio::task::yield_now().await;
        probe_file_read(probe).await?;
        // The file probe must not succeed by closing or disabling the readers.
        for client in &clients {
            let tools = tokio::time::timeout(
                OPERATION_TIMEOUT,
                client.list_tools(None, Some(OPERATION_TIMEOUT)),
            )
            .await
            .context("MCP reader stopped servicing tool responses")??;
            ensure!(!tools.tools.is_empty(), "test MCP returned no tools");
        }
        Ok(())
    }
    .await;
    let cleanup = shutdown_clients(clients).await;
    result.and(cleanup)
}

#[test]
fn idle_stdio_readers_preserve_blocking_capacity_in_both_protocols() -> Result<()> {
    let server = codex_utils_cargo_bin::cargo_bin("test_stdio_server")?;
    let temporary = tempfile::tempdir()?;
    let probe = temporary.path().join("probe.txt");
    std::fs::write(&probe, PROBE_CONTENT)?;
    for protocol_mode in [McpProtocolMode::Legacy, McpProtocolMode::V20260728] {
        let runtime = runtime()?;
        let result = runtime.block_on(idle_readers_scenario(
            &server,
            temporary.path(),
            &probe,
            protocol_mode,
        ));
        // Never let a failing baseline retain blocking readers indefinitely.
        runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);
        result.with_context(|| format!("protocol mode {protocol_mode:?}"))?;
    }
    Ok(())
}

#[test]
fn failed_stdio_spawns_leave_the_launcher_usable() -> Result<()> {
    let server = codex_utils_cargo_bin::cargo_bin("test_stdio_server")?;
    let temporary = tempfile::tempdir()?;
    let missing_cwd = temporary.path().join("missing-working-directory");
    let probe = temporary.path().join("probe.txt");
    std::fs::write(&probe, PROBE_CONTENT)?;
    let runtime = runtime()?;
    let result = runtime.block_on(async {
        // The executable is valid, so failure occurs at process creation after
        // stdout/stderr pipe preparation, rather than program-name resolution.
        for _ in 0..8 {
            match start_client(&server, &missing_cwd, McpProtocolMode::Legacy).await {
                Ok(client) => {
                    shutdown_clients(vec![client]).await?;
                    anyhow::bail!("MCP unexpectedly launched in a missing working directory");
                }
                Err(error) => {
                    ensure!(
                        !error.to_string().contains("exhausted the blocking pool"),
                        "failed spawn left the launch worker blocked: {error:#}"
                    );
                }
            }
        }
        idle_readers_scenario(&server, temporary.path(), &probe, McpProtocolMode::Legacy).await
    });
    runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);
    result
}
