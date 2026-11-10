use std::fs::File;
use std::io::Write;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::output_pipe;

#[tokio::test]
async fn synchronous_writer_preserves_bytes_and_eof() -> anyhow::Result<()> {
    let (mut reader, writer) = tokio::task::spawn_blocking(output_pipe).await??;
    let expected = b"stdout\0\r\nstderr\n".repeat(2048);
    let bytes = expected.clone();
    let write = tokio::task::spawn_blocking(move || File::from(writer).write_all(&bytes));
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut received)).await??;
    write.await??;
    assert_eq!(received, expected);
    Ok(())
}

#[test]
fn dropping_pending_reader_disconnects_live_writer() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let (mut reader, writer) = tokio::task::spawn_blocking(output_pipe).await??;
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), reader.read_exact(&mut byte))
                .await
                .is_err(),
            "the writer must remain open and idle during the pending read"
        );
        drop(reader);
        // Let the reactor collect the cancelled overlapped read before probing
        // the still-open peer. Closing must not depend on that peer exiting.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let write = tokio::task::spawn_blocking(move || File::from(writer).write_all(b"x"));
        assert!(
            tokio::time::timeout(Duration::from_secs(2), write)
                .await??
                .is_err(),
            "dropping a pending reader retained the pipe connection"
        );
        anyhow::Ok(())
    });
    // Bound a failure even if an unexpected synchronous peer write is stuck.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}
