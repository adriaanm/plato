//! The device's HTTPS client: blocking, rustls, bundled roots, hard limits.
//!
//! Everything that reaches the public internet from the reader goes through
//! [`Http::get`]. That is the point of the crate being this small: one place
//! sets the timeouts, the response ceiling and the user agent, so no caller can
//! forget to.
//!
//! Deliberately *not* here: any dependency on `plato-core`. Core stays
//! network-free, and the view layer will talk to a trait it owns, with this
//! crate's `Http` injected by the `plato` binary at startup.

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{bail, Error};
use ureq::config::RedirectAuthHeaders;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::Agent;

/// A response, already read into memory -- everything this fetches is a page of
/// text or a small image, and streaming would only complicate the callers.
pub struct Fetched {
    pub status: u16,
    pub body: Vec<u8>,
    pub elapsed: Duration,
}

impl Fetched {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// How long to wait for the whole exchange. The radio takes seconds to come
/// back from idle and the CPU is a 1 GHz Cortex-A9 doing its own handshake
/// arithmetic, so this is generous by desktop standards on purpose.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Everything worth reading here is tens of kilobytes; the largest thing this
/// project knowingly fetches is a big Hacker News thread, ~384 KB of JSON.
/// The ceiling exists so a redirect into something enormous cannot exhaust a
/// 256 MB device -- it is a safety rail, not a tuning knob.
const MAX_BODY: u64 = 4 * 1024 * 1024;

const USER_AGENT: &str = concat!("plato-net/", env!("CARGO_PKG_VERSION"),
                                 " (+https://github.com/adriaanm/plato)");

pub struct Http {
    agent: Agent,
    max_body: u64,
}

impl Default for Http {
    fn default() -> Self {
        Http::new()
    }
}

impl Http {
    pub fn new() -> Http {
        // RootCerts::WebPki, not the platform verifier: /etc/ssl/certs/certs on
        // this device holds zero certificates, so the only trust anchors that
        // exist are the ones compiled in. They are a static snapshot -- they go
        // stale with the binary, which is a thing to remember at update time,
        // not a thing to solve here.
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::WebPki)
            .build();

        let config = Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .max_redirects(4)
            // A redirect must never carry the Authorization header to another
            // host. Nothing here sends one today; the day something does, this
            // line is the reason it stays safe.
            .redirect_auth_headers(RedirectAuthHeaders::SameHost)
            .user_agent(USER_AGENT)
            .tls_config(tls)
            .build();

        Http { agent: config.new_agent(), max_body: MAX_BODY }
    }

    pub fn with_max_body(mut self, max_body: u64) -> Http {
        self.max_body = max_body;
        self
    }

    pub fn get(&self, url: &str) -> Result<Fetched, Error> {
        let start = Instant::now();
        let mut response = self.agent.get(url).call()?;
        let status = response.status().as_u16();

        // `take` rather than ureq's own limit so an oversized body is a short
        // read we notice, not a silent truncation deeper in a parser.
        let mut body = Vec::new();
        response.body_mut()
                .as_reader()
                .take(self.max_body + 1)
                .read_to_end(&mut body)?;

        if body.len() as u64 > self.max_body {
            bail!("{}: response exceeds {} bytes", url, self.max_body);
        }

        Ok(Fetched { status, body, elapsed: start.elapsed() })
    }
}

/// The `plato-core` side of the join, behind the `core-client` feature: core
/// declares what it needs from the network as a trait and stays free of a TLS
/// stack, this crate answers it and stays free of MuPDF, and the front ends
/// link both.
#[cfg(feature = "core-client")]
pub mod client {
    use plato_core::anyhow::{format_err, Error};
    use plato_core::news::HttpClient;

    use super::Http;

    pub struct NetClient(Http);

    impl NetClient {
        pub fn new() -> NetClient {
            NetClient(Http::new())
        }
    }

    impl Default for NetClient {
        fn default() -> Self {
            NetClient::new()
        }
    }

    impl HttpClient for NetClient {
        fn get(&self, url: &str) -> Result<Vec<u8>, Error> {
            let response = self.0.get(url)?;
            if response.status != 200 {
                return Err(format_err!("{url}: HTTP {}", response.status));
            }
            Ok(response.body)
        }
    }
}

/// Turn a TLS failure into the sentence that actually names the cause on this
/// device. Both of these have bitten the project before, and the raw rustls
/// text ("invalid peer certificate: Expired") does not say which.
pub fn diagnose(err: &Error) -> Option<&'static str> {
    let text = format!("{:#}", err).to_lowercase();
    if text.contains("expired") || text.contains("not yet valid") {
        Some("certificate rejected on validity dates -- check the clock \
              (platokin scripts/clock-sync.sh; an unsynced device reads 2023)")
    } else if text.contains("unknown issuer") || text.contains("unknowncert") {
        Some("no trust anchor matched -- the bundled webpki roots are stale, \
              or something is intercepting the connection")
    } else {
        None
    }
}
