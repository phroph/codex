//! Overlapped parent readers with ordinary synchronous child stdout/stderr.
//!
//! Tokio's Windows anonymous child pipes use the shared blocking pool for reads.
//! An idle MCP server can occupy two workers indefinitely. Named pipes let the
//! parent use IOCP without changing the server's stdio contract or stdin writes.

use std::fs::OpenOptions;
use std::io;
use std::os::windows::io::OwnedHandle;

use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::net::windows::named_pipe::ServerOptions;
use uuid::Uuid;

/// Called on the launcher's blocking worker, before spawning the child.
pub(super) fn output_pipe() -> io::Result<(NamedPipeServer, OwnedHandle)> {
    let name = format!(r"\\.\pipe\codex-mcp-output-{}", Uuid::new_v4());
    let reader = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .access_inbound(true)
        .access_outbound(false)
        .create(&name)?;
    let writer = OpenOptions::new().write(true).open(&name)?;
    // Both ends are opened here, so connecting does not depend on the child
    // running (it may be created suspended for Job Object assignment).
    tokio::runtime::Handle::current().block_on(reader.connect())?;
    Ok((reader, writer.into()))
}

#[cfg(test)]
#[path = "windows_stdio_tests.rs"]
mod tests;
