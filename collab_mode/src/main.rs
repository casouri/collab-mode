use anyhow::Context;
use clap::{Parser, Subcommand};
use collab_mode::{config_man, editor_receptor, server::Server};
use std::sync::Arc;
use uuid::Uuid;

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

            // Determine host_id, if exists in config, use that,
            // otherwise generate one.
            let (host_id, generated_id) =
                if let Some(config_id) = config_man.config().host_id.clone() {
                    (config_id, false)
                } else {
                    (Uuid::new_v4().to_string(), true)
                };

            if generated_id {
                let mut new_config = config_man.config();
                new_config.host_id = Some(host_id.clone());
                let res = config_man.replace_and_save(new_config);
                if let Err(err) = res {
                    panic!(
                        "Failed to save generated host id to config file: {:#?}",
                        err
                    );
                }
            }

            let mut server = Server::new(host_id.clone(), config_man)?;
            let (server_in_tx, server_in_rx) = tokio::sync::mpsc::channel(32);
            let (server_out_tx, server_out_rx) = tokio::sync::mpsc::channel(32);
            let port = socket_port.clone();

            // Listen for SIGINT/SIGTERM and surface as a shutdown
            // notify that the server can wait on.
            let shutdown = collab_mode::server::listen_for_signal();

            let shutdown_for_server = shutdown.clone();
            let server_handle = runtime.spawn(async move {
                use collab_mode::{server::*, signaling, webchannel};
                let host_id_for_factory = host_id.clone();
                let web_factory: WebChannelFactory = Box::new(move |msg_tx, self_tx| {
                    let web = Arc::new(webchannel::WebChannel::new(
                        host_id_for_factory.clone(),
                        msg_tx.clone(),
                        self_tx.clone(),
                    ));
                    let ssh = Arc::new(collab_mode::ssh_channel::SshMsgChannel::new(
                        host_id_for_factory.clone(),
                        msg_tx.clone(),
                        self_tx.clone(),
                    ));
                    Arc::new(collab_mode::poly_channel::PolyMsgChannel::new(
                        host_id_for_factory,
                        web,
                        ssh,
                        self_tx,
                    ))
                });
                let sig_factory: SignalingChannelFactory = Box::new(move |sig_tx| {
                    Box::new(signaling::client_new::SignalingChannel::new(sig_tx))
                });
                let res = server
                    .run(
                        server_out_tx,
                        server_in_rx,
                        web_factory,
                        sig_factory,
                        shutdown_for_server,
                    )
                    .await;
                if let Err(err) = res {
                    tracing::error!("Server exited with error: {:#?}", err);
                }
            });

            // Block on editor receptor (sync). Returns when editor
            // disconnects.
            if !socket {
                editor_receptor::run_stdio(server_in_tx, server_out_rx);
            } else {
                editor_receptor::run_socket(
                    &format!("localhost:{}", port),
                    server_in_tx,
                    server_out_rx,
                )
                .with_context(|| "Failed to listen on local port")?;
            }

            // We’re done, shutdown server.
            shutdown.notify_waiters();
            let _ = runtime.block_on(server_handle);
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
    use collab_mode::io_channel::{frame_read, frame_write, IoMsgChannel, ReaderWriter};
    use collab_mode::message::Msg;
    use collab_mode::server::*;
    use collab_mode::signaling::client_new::NoopSignalingChannel;
    use collab_mode::webchannel;

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async move {
        // Read the EnvoyInit frame from stdin.
        let mut stdin = tokio::io::stdin();
        let init_frame = frame_read(&mut stdin)
            .await
            .context("reading EnvoyInit from stdin")?;
        let (host_id, envoy_id, host_cert, envoy_key_pem, envoy_cert_pem, projects) =
            match init_frame.body {
                Msg::EnvoyInit {
                    host_id,
                    envoy_id,
                    host_cert,
                    envoy_key_pem,
                    envoy_cert_pem,
                    projects,
                } => (
                    host_id,
                    envoy_id,
                    host_cert,
                    envoy_key_pem,
                    envoy_cert_pem,
                    projects,
                ),
                other => {
                    let err = format!("expected EnvoyInit, got {:?}", other);
                    let mut stdout = tokio::io::stdout();
                    let _ = frame_write(
                        &mut stdout,
                        &webchannel::Message {
                            host: "envoy".into(),
                            body: Msg::EnvoyInitError(err.clone()),
                            req_id: None,
                        },
                    )
                    .await;
                    return Err(anyhow::anyhow!("{}", err));
                }
            };

        // Use a temp dir for config. We don’t save config anyway.
        let temp = tempfile::TempDir::new().context("creating envoy tempdir")?;
        let config_man = config_man::ConfigManager::new(Some(temp.path().to_owned()), None)?;

        let key_cert = std::sync::Arc::new(config_man::key_cert_from_pem(
            &envoy_key_pem,
            &envoy_cert_pem,
        )?);
        let mut server = Server::new_with_keycert(envoy_id.clone(), config_man, key_cert)?;
        server.init_for_envoy(host_id.clone(), host_cert.clone(), projects)?;

        // Send Hey to the host to boostrap. Write directly to stdout.
        {
            let mut stdout = tokio::io::stdout();
            let hey = webchannel::Message {
                host: envoy_id.clone(),
                body: Msg::Hey(collab_mode::message::HeyMessage::new(
                    "Nice to meet ya".into(),
                    "".into(),
                    "v1.0.0".into(),
                )),
                req_id: None,
            };
            frame_write(&mut stdout, &hey).await?;
        }

        // Dummy editor tx, since we don’t listen to editor in envoy mode.
        let (editor_tx_alive, editor_rx) = tokio::sync::mpsc::channel::<lsp_server::Message>(1);
        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel::<lsp_server::Message>(1);
        tokio::spawn(async move { while sink_rx.recv().await.is_some() {} });
        let _editor_tx_alive = editor_tx_alive;

        // Use IoMsgChannel directly for envoy mode.
        let envoy_id_for_factory = envoy_id.clone();
        let host_id_for_factory = host_id.clone();
        let web_factory: WebChannelFactory = Box::new(move |msg_tx, self_tx| {
            let rw = ReaderWriter::new(tokio::io::stdin(), tokio::io::stdout(), ());
            let io = IoMsgChannel::new(
                envoy_id_for_factory,
                host_id_for_factory,
                rw,
                msg_tx,
                self_tx,
            );
            std::sync::Arc::new(io)
        });
        let sig_factory: SignalingChannelFactory = Box::new(|_| Box::new(NoopSignalingChannel));

        // Hold tempdir for the process lifetime.
        let _hold_temp = temp;

        let shutdown = collab_mode::server::listen_for_signal();

        server
            .run(sink_tx, editor_rx, web_factory, sig_factory, shutdown)
            .await
    })
}
