use std::env;
use std::io::{self, ErrorKind, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Builder;
use tokio::signal::ctrl_c;
use tokio::signal::unix::{SignalKind, signal};
use tokio::{select, spawn};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};
use webpki_roots::TLS_SERVER_ROOTS;

use crate::certs::Ca;
use crate::config::Config;
use crate::error::Error;
use crate::proxy::Proxy;
use crate::result::Result;
use crate::service::{PRIVILEGED, manage, privileged};
use crate::settings::Settings;
use crate::store::Store;
use crate::upload::Uploader;

pub fn run() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("traces=info"));
    let _ = fmt().with_env_filter(filter).with_ansi(io::stdout().is_terminal()).try_init();
    // Define the main serving logic as a closure to be executed if no command is provided.
    let serve = || -> Result<()> {
        // Load the config and settings
        let (config, settings) = (Config::load()?, Settings::load()?);
        // Load or create the CA certificate for TLS connections.
        let ca = Ca::load_or_create(&config.ca_dir()?)?;
        // Build the root certificate store for the client TLS configuration.
        let roots = RootCertStore { roots: TLS_SERVER_ROOTS.to_vec() };
        // Build the client TLS configuration with the root certificates and no client authentication.
        let client = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        // Initialize the Tokio runtime for asynchronous operations.
        let runtime = Builder::new_multi_thread().enable_all().build().map_err(Error::Runtime)?;
        // Execute the asynchronous serving logic within the runtime.
        runtime.block_on(async {
            let address = config.listen;
            // Bind the main listener to the configured address.
            let listener = TcpListener::bind(address).await.map_err(|source| Error::Bind { address, source })?;
            // Bind the listener for the /etc/hosts interface if configured.
            let hosts = match config.hosts_listen {
                // Attempt to bind the /etc/hosts listener to the specified address.
                Some(address) => match TcpListener::bind(address).await {
                    Ok(listener) => Some((listener, address.port())),
                    Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                        warn!("no permission for {address}, so the /etc/hosts listener is off (see `traces install`)");
                        None
                    }
                    Err(source) => return Err(Error::Bind { address, source }),
                },
                None => None,
            };
            // Set up signal handling for termination.
            let mut terminate = signal(SignalKind::terminate())?;
            // Initialize the in-memory store for trace data.
            let store = Arc::new(Store::new(settings.max_memory_bytes));
            // Start the uploader for sending trace data to the remote server.
            let uploader = Uploader::start(&settings, store.clone());
            info!("{}", banner(&config, &ca.cert_path, &settings)); // log the banner string
            // create the forwarder proxy
            let proxy = Arc::new(Proxy::new(config, ca, client, store));
            // set up the /etc/hosts listener if configured
            if let Some((listener, port)) = hosts {
                let proxy = proxy.clone();
                // spawn a task to handle incoming connections for the /etc/hosts listener
                spawn(async move {
                    if let Err(error) = accept(listener, |client| proxy.clone().hosts(client, port)).await {
                        error!("/etc/hosts listener stopped: {error}");
                    }
                });
            }
            // start accepting incoming connections for the main proxy tunnel
            // exit when either the main proxy tunnel stops or a termination signal is received
            select! {
                stopped = accept(listener, |client| proxy.clone().tunnel(client)) => return stopped,
                _ = ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            // when exit signal is received, flush the uploader and handle any remaining tasks
            select! {
                _ = uploader.flush() => {}
                _ = ctrl_c() => {}
            }
            Ok(())
        })
    };
    // Determine the command to execute based on the first argument.
    let outcome = match env::args().nth(1).as_deref() {
        Some(PRIVILEGED) => privileged(),
        Some(command @ ("install" | "start" | "uninstall")) => manage(command),
        Some(_) => Err(Error::Usage),
        None => serve(), // Default to serving if no command is provided.
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn accept<F, T>(listener: TcpListener, handle: F) -> Result<()>
where
    F: Fn(TcpStream) -> T,
    T: Future<Output = Result<()>> + Send + 'static,
{
    loop {
        let connection = handle(listener.accept().await?.0);
        spawn(async move {
            if let Err(error) = connection.await {
                warn!("connection error: {error}");
            }
        });
    }
}

/// Generate a banner string displaying the proxy configuration and storage settings.
fn banner(config: &Config, ca_cert: &Path, settings: &Settings) -> String {
    // Display the proxy listening address, CA certificate path, and storage settings.
    let (listen, ca) = (config.listen, ca_cert.display());
    // Extract the storage endpoint, bucket, and purge interval from the settings.
    let (endpoint, bucket, seconds) = (&settings.endpoint, &settings.bucket, settings.purge_seconds);
    // Convert the maximum memory bytes to MiB for display.
    let mib = settings.max_memory_bytes >> 20;
    // Prepare the hosts listener information for display.
    let hosts = config.hosts_listen.map_or_else(String::new, |address| {
        format!("/etc/hosts listener on {address} (`traces install` adds the LLM hosts to /etc/hosts)\n")
    });
    // create the display string
    format!(
        "traces listening on {listen}\nHTTPS_PROXY=http://{listen} NODE_USE_ENV_PROXY=1 NODE_EXTRA_CA_CERTS={ca} code\n{hosts}\
         traces go to bucket {bucket} at {endpoint}, every {seconds} s or at {mib} MiB, whichever comes first"
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::banner;
    use crate::config::Config;
    use crate::settings::Settings;

    #[test]
    fn banner_gives_the_client_settings_and_where_the_traces_go_without_secrets() {
        let settings = Settings {
            endpoint: "https://s3.example.com".into(),
            region: "auto".into(),
            bucket: "traces".into(),
            access_key_id: "key-id".into(),
            secret_access_key: "secret-value".into(),
            purge_seconds: 60,
            max_memory_bytes: 64 << 20,
        };
        let text = banner(&Config::load().unwrap(), Path::new("/home/a/.traces/ca.crt"), &settings);
        let client = "\nHTTPS_PROXY=http://127.0.0.1:7646 NODE_USE_ENV_PROXY=1 NODE_EXTRA_CA_CERTS=/home/a/.traces/ca.crt code\n";
        assert!(text.contains(client));
        assert!(
            text.contains("\n/etc/hosts listener on 0.0.0.0:443 (`traces install` adds the LLM hosts to /etc/hosts)\n")
        );
        assert!(
            text.ends_with("bucket traces at https://s3.example.com, every 60 s or at 64 MiB, whichever comes first")
        );
        assert!(!text.contains("key-id") && !text.contains("secret-value"));
    }
}
