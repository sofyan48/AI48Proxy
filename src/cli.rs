use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use std::{path::PathBuf, time::Duration};
use tokio::io::AsyncReadExt;

#[derive(Parser)]
#[command(version, about = "AI48Home-managed Rust gateway")]
pub(crate) struct Args {
    /// Bootstrap token from Home. Prefer HOME_JWT_FILE to avoid exposing the token.
    #[arg(
        long,
        env = "HOME_JWT",
        hide_env_values = true,
        conflicts_with = "home_jwt_file"
    )]
    pub home_jwt: Option<String>,
    /// Read the bootstrap token from a file instead of HOME_JWT.
    #[arg(long, env = "HOME_JWT_FILE", hide_env_values = true)]
    pub home_jwt_file: Option<PathBuf>,
    /// Private gateway identity directory, independent of CPA's identity.
    #[arg(long, default_value = ".ai48proxy")]
    pub identity_dir: PathBuf,
    /// Existing enrolled identity target, used instead of enrollment.
    #[arg(long, requires = "home_port", conflicts_with_all = ["home_jwt", "home_jwt_file"])]
    pub home_host: Option<String>,
    #[arg(long, requires = "home_host")]
    pub home_port: Option<u16>,
    /// Validate a saved Home configuration snapshot without starting the gateway.
    #[arg(long)]
    pub check_config: Option<PathBuf>,
    /// Probe an HTTP loopback URL without enrolling or loading credentials.
    #[arg(long)]
    pub healthcheck: Option<String>,
    /// Seconds to wait before enrollment, allowing old worker membership to expire.
    #[arg(long, env = "WORKER_START_DELAY", default_value_t = 0,
        value_parser = clap::value_parser!(u64).range(0..=u32::MAX as u64))]
    pub worker_start_delay: u64,
}

impl Args {
    pub async fn bootstrap_token(&self) -> Result<Option<String>> {
        let token = if let Some(path) = &self.home_jwt_file {
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|_| anyhow::anyhow!("cannot open Home JWT file"))?;
            ensure!(
                file.metadata()
                    .await
                    .context("cannot inspect Home JWT file")?
                    .is_file(),
                "Home JWT file must be a regular file"
            );
            let mut bytes = Vec::new();
            file.take(65537)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| anyhow::anyhow!("cannot read Home JWT file"))?;
            ensure!(bytes.len() <= 65536, "Home JWT file exceeds size limit");
            Some(
                String::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("Home JWT file must contain UTF-8 text"))?,
            )
        } else {
            self.home_jwt.clone()
        };
        token.map(|token| validate_token(&token)).transpose()
    }
}

pub(crate) async fn wait_for_start(
    seconds: u64,
    shutdown: impl std::future::Future<Output = ()>,
) -> bool {
    tokio::select! {
        biased;
        _ = shutdown => false,
        _ = tokio::time::sleep(Duration::from_secs(seconds)) => true,
    }
}

fn validate_token(token: &str) -> Result<String> {
    let token = token.trim();
    ensure!(
        !token.is_empty() && token.len() <= 65536,
        "invalid Home JWT length"
    );
    // Validate without including either the token or a parser error in diagnostics.
    ensure!(
        token.bytes().all(|byte| byte.is_ascii_graphic())
            && http::HeaderValue::from_str(token).is_ok(),
        "Home JWT contains invalid characters"
    );
    Ok(token.to_owned())
}

fn health_url(value: &str) -> Result<url::Url> {
    let mut url = url::Url::parse(value).map_err(|_| anyhow::anyhow!("invalid healthcheck URL"))?;
    ensure!(
        url.scheme() == "http"
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "healthcheck requires an HTTP loopback URL without credentials or fragment"
    );
    match url.host() {
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() => {}
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() => {}
        Some(url::Host::Domain("localhost")) => {
            // Pin localhost to loopback instead of trusting DNS or hosts-file resolution.
            url.set_host(Some("127.0.0.1"))
                .map_err(|_| anyhow::anyhow!("invalid healthcheck host"))?;
        }
        _ => bail!("healthcheck host must be a loopback IP address or localhost"),
    }
    Ok(url)
}

pub(crate) async fn healthcheck(value: &str) -> Result<()> {
    let url = health_url(value)?;
    // This intentional health-probe deadline does not affect upstream proxy traffic.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| anyhow::anyhow!("cannot build healthcheck client"))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("healthcheck request failed"))?;
    ensure!(
        response.status().is_success(),
        "gateway is not ready (HTTP {})",
        response.status().as_u16()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn parse(values: &[&str]) -> std::result::Result<Args, clap::Error> {
        // Do not let developer/CI credentials affect argument-selection tests.
        let command = Args::command()
            .mut_arg("home_jwt", |arg| arg.env(None::<&str>))
            .mut_arg("home_jwt_file", |arg| arg.env(None::<&str>))
            .mut_arg("worker_start_delay", |arg| arg.env(None::<&str>));
        Args::from_arg_matches(&command.try_get_matches_from(values)?)
    }

    #[test]
    fn selection_and_delay() {
        assert!(parse(&["proxy", "--healthcheck", "http://127.0.0.1/healthz"]).is_ok());
        assert!(parse(&["proxy", "--home-jwt-file", "token"]).is_ok());
        assert!(parse(&["proxy", "--home-host", "home", "--home-port", "8327"]).is_ok());
        for values in [
            vec!["proxy", "--home-jwt", "secret", "--home-jwt-file", "token"],
            vec![
                "proxy",
                "--home-jwt-file",
                "token",
                "--home-host",
                "home",
                "--home-port",
                "8327",
            ],
            vec![
                "proxy",
                "--home-jwt",
                "secret",
                "--home-host",
                "home",
                "--home-port",
                "8327",
            ],
            vec!["proxy", "--home-host", "home"],
            vec!["proxy", "--home-port", "8327"],
            vec!["proxy", "--worker-start-delay", "-1"],
            vec!["proxy", "--worker-start-delay", "1.5"],
        ] {
            let error = parse(&values).err().expect("must reject invalid selection");
            assert!(!error.to_string().contains("secret"));
        }
        assert_eq!(parse(&["proxy"]).unwrap().worker_start_delay, 0);
        assert_eq!(
            parse(&["proxy", "--worker-start-delay", "15"])
                .unwrap()
                .worker_start_delay,
            15
        );
        let command = Args::command();
        for (id, name) in [
            ("home_jwt", "HOME_JWT"),
            ("home_jwt_file", "HOME_JWT_FILE"),
            ("worker_start_delay", "WORKER_START_DELAY"),
        ] {
            assert_eq!(
                command
                    .get_arguments()
                    .find(|arg| arg.get_id() == id)
                    .unwrap()
                    .get_env(),
                Some(std::ffi::OsStr::new(name))
            );
        }
    }

    #[test]
    fn tokens_are_header_safe_and_errors_are_redacted() {
        assert_eq!(validate_token("  abc.def.ghi\n").unwrap(), "abc.def.ghi");
        for token in [
            "",
            "secret\r\nX-Injected: yes",
            "secret\0",
            "secret token",
            "秘密",
        ] {
            assert!(validate_token(token).is_err());
            assert!(
                !validate_token(token)
                    .unwrap_err()
                    .to_string()
                    .contains("secret")
            );
        }
    }

    #[tokio::test]
    async fn token_file_is_bounded_and_trimmed() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let args = parse(&["proxy", "--home-jwt-file", path]).unwrap();
        tokio::fs::write(file.path(), b"abc.def.ghi\n")
            .await
            .unwrap();
        assert_eq!(
            args.bootstrap_token().await.unwrap().as_deref(),
            Some("abc.def.ghi")
        );
        tokio::fs::write(file.path(), b"secret\r\nInjected: yes")
            .await
            .unwrap();
        assert!(
            !args
                .bootstrap_token()
                .await
                .unwrap_err()
                .to_string()
                .contains("secret")
        );
        tokio::fs::write(file.path(), vec![b'a'; 65537])
            .await
            .unwrap();
        assert!(args.bootstrap_token().await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn startup_delay_is_interruptible() {
        assert!(!wait_for_start(60, std::future::ready(())).await);
        let start = tokio::time::Instant::now();
        assert!(wait_for_start(15, std::future::pending()).await);
        assert_eq!(start.elapsed(), Duration::from_secs(15));
    }

    #[test]
    fn health_targets_are_loopback_only() {
        for value in [
            "http://127.0.0.1/healthz",
            "http://127.2.3.4:8317/healthz",
            "http://[::1]/healthz",
            "http://localhost/healthz",
        ] {
            assert!(health_url(value).is_ok(), "{value}");
        }
        for value in [
            "https://127.0.0.1/",
            "http://example.com/",
            "http://169.254.169.254/",
            "http://0.0.0.0/",
            "http://[::]/",
            "http://localhost.example/",
            "http://user:secret@127.0.0.1/",
            "http://127.0.0.1/#secret",
            "not a url",
        ] {
            assert!(health_url(value).is_err(), "{value}");
        }
        assert_eq!(
            health_url("http://localhost/").unwrap().host_str(),
            Some("127.0.0.1")
        );
    }

    #[tokio::test]
    async fn probe_checks_readiness_without_credentials_or_redirects() {
        for (status, ready) in [
            ("200 OK", true),
            ("503 Service Unavailable", false),
            ("302 Found", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(socket.read_u8().await.unwrap());
                }
                let headers = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(headers.starts_with("get /healthz http/1.1\r\n"));
                assert!(!headers.contains("authorization:"));
                assert!(!headers.contains("cookie:"));
                socket.write_all(format!("HTTP/1.1 {status}\r\nLocation: http://192.0.2.1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            });
            assert_eq!(
                healthcheck(&format!("http://{address}/healthz"))
                    .await
                    .is_ok(),
                ready
            );
            server.await.unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        assert!(
            healthcheck(&format!("http://{address}/healthz"))
                .await
                .is_err()
        );
    }
}
