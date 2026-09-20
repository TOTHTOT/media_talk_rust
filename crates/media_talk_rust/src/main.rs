use clap::{Parser, Subcommand};
use std::process::ExitCode;
use tracing_subscriber::{EnvFilter, fmt};

mod ipc;

#[derive(Parser, Debug)]
#[command(name = "media_talk_rust", version, about = "media talk server")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Discover {
        #[arg(long, default_value_t = 2)]
        timeout_secs: u64,
        #[arg(long, default_value_t = false)]
        json: bool,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
    },
    Serve {
        #[arg(long, default_value = "0.0.0.0:8081")]
        bind: String,
        #[arg(long, default_value_t = 5)]
        discovery_timeout_secs: u64,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
        /// Manually inject a device using a known RTSP URL, bypassing
        /// ONVIF discovery / GetStreamUri. The URL's userinfo is used for
        /// RTSP auth. May be passed multiple times.
        #[arg(long, value_name = "RTSP_URL")]
        rtsp_url: Vec<String>,
        /// Play the camera's audio track on this ALSA device (target
        /// board speaker).
        #[arg(long, value_name = "ALSA_DEVICE")]
        audio_out: Option<String>,
    },
    DecodeBench {
        path: String,
        #[arg(long, default_value_t = 0)]
        max_frames: u64,
    },
    Audio {
        #[command(subcommand)]
        action: AudioAction,
    },
    V4l2 {
        #[command(subcommand)]
        action: V4l2Action,
    },
}

#[derive(Subcommand, Debug)]
enum AudioAction {
    ListDevices,
}

#[derive(Subcommand, Debug)]
enum V4l2Action {
    List,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();

    let result: anyhow::Result<()> = match cli.command {
        Commands::Discover {
            timeout_secs,
            json,
            username,
            password,
        } => ipc::discover::run(timeout_secs, json, username, password).await,
        Commands::Serve {
            bind,
            discovery_timeout_secs,
            username,
            password,
            rtsp_url,
            audio_out,
        } => {
            let shutdown = ipc::shutdown::install();
            ipc::serve::run(
                bind,
                discovery_timeout_secs,
                username,
                password,
                rtsp_url,
                audio_out,
                shutdown,
            )
            .await
        }
        Commands::DecodeBench { path, max_frames } => {
            ipc::decode_bench::run(&path, max_frames).await
        }
        Commands::Audio { action } => {
            match action {
                AudioAction::ListDevices => ipc::audio::list_devices(),
            }
            Ok(())
        }
        Commands::V4l2 { action } => {
            match action {
                V4l2Action::List => ipc::v4l2::list(),
            }
            Ok(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:?}");
            ExitCode::from(1)
        }
    }
}
