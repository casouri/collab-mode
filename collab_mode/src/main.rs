use anyhow::Context;
use clap::{Parser, Subcommand};
use collab_mode::{
    config_man, editor_receptor, filewatch_receptor, filewatch_receptor::WatchFileMessage,
    server::Server,
};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the collab process
    Run {
        /// Use this flag to use socket instead of stdio between the
        /// editor and collab process.
        #[arg(long)]
        socket: bool,
        /// Port used by the socket.
        #[arg(long, short = 'p')]
        #[arg(default_value_t = 7701)]
        socket_port: u16,
        /// If given, configuration and data files are saved in this
        /// directory rather than the default XDG location.
        #[arg(long)]
        config: Option<String>,
        /// If using default XDG location for config and data files,
        /// use this to specify the XDG profile name. This flag can’t
        /// be used with the --config flag.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Run as an envoy spawned by a remote host over ssh. Reads a
    /// `Msg::EnvoyInit` from stdin, then proxies the host's editor
    /// requests over the same stdio connection. No flags — config is
    /// fully derived from `EnvoyInit`.
    Envoy,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Run {
            socket,
            socket_port,
            config,
            profile,
        }) => {
            if config.is_some() && profile.is_some() {
                panic!("Cannot use --config and --profile at the same time");
            }

            let runtime = tokio::runtime::Runtime::new().unwrap();
            let config_location = if let Some(config_path) = config {
                Some(
                    expanduser::expanduser(config_path)
                        .with_context(|| "Can't expand filename to absolute path".to_string())?,
                )
            } else {
                None
            };
            let mut config_man =
                config_man::ConfigManager::new(config_location, profile.to_owned())?;

            // Load or create our key/cert.
            let (name, generated_name) = match config_man.config().host_id.clone() {
                Some(n) => (n, false),
                None => (default_name(), true),
            };
            // Write back the created name.
            if generated_name {
                let mut new_config = config_man.config();
                new_config.host_id = Some(name.clone());
                if let Err(err) = config_man.replace_and_save(new_config) {
                    panic!("Failed to save generated host id to config: {:#?}", err);
                }
            }
            // Compose host_id from the name and cert hash.
            let cert_hash = config_man.get_key_and_cert(&name)?.cert_der_hash();
            let host_id = collab_mode::types::make_id(Some(&name), &cert_hash);

            let mut server = Server::new(host_id.clone(), config_man)?;
            let (server_in_tx, server_in_rx) = tokio::sync::mpsc::channel(32);
            let (server_out_tx, server_out_rx) = tokio::sync::mpsc::channel(32);
            let (filewatch_to_server_tx, filewatch_to_server_rx) =
                tokio::sync::mpsc::channel::<WatchFileMessage>(32);
            let (server_to_filewatch_tx, server_to_filewatch_rx) =
                crossbeam_channel::bounded::<WatchFileMessage>(32);
            let use_socket = *socket;
            let port = *socket_port;
            let shutdown = collab_mode::cancel::CancelManager::new();

            // Run editor receptor.
            let editor_shutdown = shutdown.clone();
            std::thread::spawn(move || {
                if !use_socket {
                    editor_receptor::run_stdio(server_in_tx, server_out_rx, editor_shutdown);
                } else if let Err(err) = editor_receptor::run_socket(
                    &format!("localhost:{}", port),
                    server_in_tx,
                    server_out_rx,
                    editor_shutdown,
                ) {
                    tracing::error!("Failed to listen on local port: {:#?}", err);
                }
            });

            // Run filewatch receptor.
            let filewatch_shutdown = shutdown.clone();
            std::thread::spawn(move || {
                if let Err(err) = filewatch_receptor::run(
                    filewatch_to_server_tx,
                    server_to_filewatch_rx,
                    filewatch_shutdown,
                ) {
                    tracing::error!("Filewatch receptor exited with error: {:#?}", err);
                }
            });

            // Run Ctrl-C listener.
            let shutdown_for_signal = shutdown.clone();
            runtime.spawn(async move {
                collab_mode::server::listen_for_signal(shutdown_for_signal).await;
            });

            // Run server.
            let shutdown_for_server = shutdown.clone();
            let shutdown_for_factory = shutdown.clone();
            runtime.block_on(async move {
                use collab_mode::{server::*, signaling, webchannel};
                let host_id_for_factory = host_id.clone();
                let web_factory: WebChannelFactory = Box::new(move |msg_tx, self_tx| {
                    webchannel::WebChannel::new(
                        host_id_for_factory,
                        msg_tx,
                        self_tx,
                        shutdown_for_factory,
                    )
                });
                let shutdown_for_sig = shutdown_for_server.clone();
                let sig_factory: SignalingChannelFactory = Box::new(move |sig_tx| {
                    signaling::client::SignalingChannel::new(sig_tx, shutdown_for_sig)
                });
                let res = server
                    .run(
                        server_out_tx,
                        server_in_rx,
                        server_to_filewatch_tx,
                        filewatch_to_server_rx,
                        web_factory,
                        sig_factory,
                        shutdown_for_server,
                    )
                    .await;
                if let Err(err) = res {
                    tracing::error!("Server exited with error: {:#?}", err);
                }
            });

            Ok(())
        }
        Some(Commands::Envoy) => run_envoy(),
        _ => {
            panic!("Unsupported command");
        }
    }
}

/// Run as an envoy: read `Msg::EnvoyInit` from stdin, configure the
/// server from it, then run `Server::run` with stdio as the only peer
/// transport.
fn run_envoy() -> anyhow::Result<()> {
    use collab_mode::message::Msg;
    use collab_mode::server::*;
    use collab_mode::signaling::client::SignalingChannel;
    use collab_mode::webchannel;
    use collab_mode::webchannel::{frame_read, frame_write};

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async move {
        // Read the EnvoyInit frame from stdin.
        let mut stdin = tokio::io::stdin();
        let init_frame = frame_read(&mut stdin)
            .await
            .context("reading EnvoyInit from stdin")?;
        let (host_id, envoy_id, envoy_key_pem, envoy_cert_pem, projects) = match init_frame.body {
            Msg::EnvoyInit {
                host_id,
                envoy_id,
                envoy_key_pem,
                envoy_cert_pem,
                projects,
            } => (host_id, envoy_id, envoy_key_pem, envoy_cert_pem, projects),
            other => {
                let err = format!("expected EnvoyInit, got {:?}", other);
                let mut stdout = tokio::io::stdout();
                let _ = frame_write(
                    &mut stdout,
                    &webchannel::Message {
                        host: "envoy".into(),
                        body: Msg::EnvoyInitError {
                            reason: err.clone(),
                        },
                        req_id: None,
                    },
                )
                .await;
                return Err(anyhow::anyhow!("{}", err));
            }
        };

        // Use a temp dir for config. We don’t save config anyway.
        let temp = tempfile::TempDir::new().context("Create envoy tempdir")?;
        let config_man = config_man::ConfigManager::new(Some(temp.path().to_owned()), None)?;

        let key_cert = std::sync::Arc::new(config_man::key_cert_from_pem(
            &envoy_key_pem,
            &envoy_cert_pem,
        )?);
        let envoy_name = collab_mode::types::id_name(&envoy_id)
            .ok_or_else(|| anyhow::anyhow!("envoy_id has no name: {:?}", envoy_id))?
            .to_string();
        let mut server =
            Server::new_with_keycert(envoy_id.clone(), envoy_name, config_man, key_cert)?;
        server.init_for_envoy(host_id.clone(), projects)?;

        // Send Hey to the host to boostrap. Write directly to stdout.
        {
            let mut stdout = tokio::io::stdout();
            let hey = webchannel::Message {
                host: envoy_id.clone(),
                body: Msg::Hey {
                    hey: collab_mode::message::HeyMessage::new(
                        "Nice to meet ya".into(),
                        "".into(),
                        "v1.0.0".into(),
                    ),
                },
                req_id: None,
            };
            frame_write(&mut stdout, &hey).await?;
        }

        // Dummy editor tx, since we don’t listen to editor in envoy mode.
        let (editor_tx_alive, editor_rx) = tokio::sync::mpsc::channel::<lsp_server::Message>(1);
        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel::<lsp_server::Message>(1);
        tokio::spawn(async move { while sink_rx.recv().await.is_some() {} });
        let _editor_tx_alive = editor_tx_alive;

        // Envoy mode currently relies on Io transport, which the new
        // actor-model WebChannel doesn’t implement yet. The IoRemote
        // refactor will restore this; for now we hand back a vanilla
        // WebChannel so the binary builds.
        let shutdown = collab_mode::cancel::CancelManager::new();
        let shutdown_for_factory = shutdown.clone();
        let envoy_id_for_factory = envoy_id.clone();
        let web_factory: WebChannelFactory = Box::new(move |msg_tx, self_tx| {
            webchannel::WebChannel::new(envoy_id_for_factory, msg_tx, self_tx, shutdown_for_factory)
        });

        let shutdown_for_sig = shutdown.clone();
        let sig_factory: SignalingChannelFactory =
            Box::new(move |sig_tx| SignalingChannel::new(sig_tx, shutdown_for_sig));

        // Hold tempdir for the process lifetime.
        let _hold_temp = temp;

        // Already inside the runtime via block_on, so spawn the
        // signal task directly.
        let shutdown_for_signal = shutdown.clone();
        tokio::spawn(async move {
            collab_mode::server::listen_for_signal(shutdown_for_signal).await;
        });

        let (filewatch_to_server_tx, filewatch_to_server_rx) =
            tokio::sync::mpsc::channel::<WatchFileMessage>(32);
        let (server_to_filewatch_tx, server_to_filewatch_rx) =
            crossbeam_channel::bounded::<WatchFileMessage>(32);
        let filewatch_shutdown = shutdown.clone();
        std::thread::spawn(move || {
            if let Err(err) = filewatch_receptor::run(
                filewatch_to_server_tx,
                server_to_filewatch_rx,
                filewatch_shutdown,
            ) {
                tracing::error!("Filewatch receptor exited with error: {:#?}", err);
            }
        });
        server
            .run(
                sink_tx,
                editor_rx,
                server_to_filewatch_tx,
                filewatch_to_server_rx,
                web_factory,
                sig_factory,
                shutdown,
            )
            .await
    })
}

/// Pick a default name when the user hasn't set `host_id` in config.
/// Use the OS hostname, replace non-`[A-Za-z0-9_-]` char with
/// `-` and truncating to 20 chars. Falls back to "dude" if the
/// hostname is unavailable or sanitizes to empty.
fn default_name() -> String {
    let raw = hostname::get()
        .ok()
        .and_then(|os| os.into_string().ok())
        .unwrap_or_default();
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(20)
        .collect();
    if sanitized.is_empty() {
        "dude".to_string()
    } else {
        sanitized
    }
}
