//! The one place an HTTPS client gets its identity: every request QuickDictate
//! makes (update check, settings sync, the usage rollup, local model downloads)
//! carries the same User-Agent, and each caller chooses only how long it is
//! willing to wait.

use std::time::Duration;

/// Sent with every request, so a server log can tell which version is talking.
pub(crate) const USER_AGENT: &str = concat!("QuickDictate/", env!("CARGO_PKG_VERSION"));

/// A blocking client with the shared User-Agent: `total` bounds a whole
/// request, `connect` just the TCP and TLS handshake. For worker threads only;
/// the async paths build their own `reqwest::Client` with [`USER_AGENT`].
pub(crate) fn blocking_client(
    total: Duration,
    connect: Duration,
) -> reqwest::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(total)
        .connect_timeout(connect)
        .build()
}
