///! # Common network utilities.
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::net::{lookup_host, TcpStream};
use tokio::time::timeout;
use tokio_io_timeout::TimeoutStream;

use crate::context::Context;
use crate::tools::time;

async fn connect_tcp_inner(addr: SocketAddr, timeout_val: Duration) -> Result<TcpStream> {
    let tcp_stream = timeout(timeout_val, TcpStream::connect(addr))
        .await
        .context("connection timeout")?
        .context("connection failure")?;
    Ok(tcp_stream)
}

/// Looks up hostname and port using DNS and updates the address resolution cache.
///
/// If `load_cache` is true, appends cached results not older than 30 days to the end.
async fn lookup_host_with_cache(
    context: &Context,
    hostname: &str,
    port: u16,
    load_cache: bool,
) -> Result<Vec<SocketAddr>> {
    let now = time();
    let mut resolved_addrs: Vec<SocketAddr> = lookup_host((hostname, port)).await?.collect();

    for (i, addr) in resolved_addrs.iter().enumerate() {
        info!(context, "Resolved {}:{} into {}.", hostname, port, &addr);

        let i = i64::try_from(i).unwrap_or_default();

        // Update the cache.
        //
        // Add sequence number to the timestamp, so addresses are ordered by timestamp in the same
        // order as the resolver returns them.
        context
            .sql
            .execute(
                "INSERT INTO dns_cache
                 (hostname, port, address, timestamp)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT (hostname, port, address)
                 DO UPDATE SET timestamp=excluded.timestamp",
                paramsv![hostname, port, addr.to_string(), now.saturating_add(i)],
            )
            .await?;
    }

    if load_cache {
        for cached_address in context
            .sql
            .query_map(
                "SELECT address
                 FROM dns_cache
                 WHERE hostname = ?
                 AND ? < timestamp + 30 * 24 * 3600
                 ORDER BY timestamp DESC",
                paramsv![hostname, now],
                |row| {
                    let address: String = row.get(0)?;
                    Ok(address)
                },
                |rows| {
                    rows.collect::<std::result::Result<Vec<_>, _>>()
                        .map_err(Into::into)
                },
            )
            .await?
        {
            match SocketAddr::from_str(&cached_address) {
                Ok(addr) => {
                    if !resolved_addrs.contains(&addr) {
                        resolved_addrs.push(addr);
                    }
                }
                Err(err) => {
                    warn!(
                        context,
                        "Failed to parse cached address {:?}: {:#}.", cached_address, err
                    );
                }
            }
        }
    }

    Ok(resolved_addrs)
}

/// Returns a TCP connection stream with read/write timeouts set
/// and Nagle's algorithm disabled with `TCP_NODELAY`.
///
/// `TCP_NODELAY` ensures writing to the stream always results in immediate sending of the packet
/// to the network, which is important to reduce the latency of interactive protocols such as IMAP.
///
/// If `load_cache` is true, may use cached DNS results.
/// Use this only if the connection is going to be protected with TLS.
pub(crate) async fn connect_tcp(
    context: &Context,
    host: &str,
    port: u16,
    timeout_val: Duration,
    load_cache: bool,
) -> Result<Pin<Box<TimeoutStream<TcpStream>>>> {
    let mut tcp_stream = None;

    for resolved_addr in lookup_host_with_cache(context, host, port, load_cache).await? {
        match connect_tcp_inner(resolved_addr, timeout_val).await {
            Ok(stream) => {
                tcp_stream = Some(stream);
                break;
            }
            Err(err) => {
                warn!(
                    context,
                    "Failed to connect to {}: {:#}.", resolved_addr, err
                );
            }
        }
    }

    let tcp_stream =
        tcp_stream.with_context(|| format!("failed to connect to {}:{}", host, port))?;

    // Disable Nagle's algorithm.
    tcp_stream.set_nodelay(true)?;

    let mut timeout_stream = TimeoutStream::new(tcp_stream);
    timeout_stream.set_write_timeout(Some(timeout_val));
    timeout_stream.set_read_timeout(Some(timeout_val));
    let pinned_stream = Box::pin(timeout_stream);

    Ok(pinned_stream)
}
