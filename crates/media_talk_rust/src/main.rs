use clap::{Parser, Subcommand};
use std::net::{IpAddr, SocketAddr};
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
    /// SIP GUI (Slint): register + dial + answer. 所有参数都是可选覆盖,
    /// 缺省由设置页 kv / 内置默认补全
    Gui {
        /// SIP server address
        #[arg(long)]
        server: Option<SocketAddr>,
        /// SIP account (also used as username)
        #[arg(short, long)]
        username: Option<String>,
        /// Password
        #[arg(short, long)]
        password: Option<String>,
        /// Registration expiry (seconds)
        #[arg(short, long)]
        expires: Option<u32>,
        /// Video source: file:<path> | rtsp://<uri> | camera[:<device>]
        #[arg(long)]
        video_src: Option<String>,
        /// Audio source: file:<path> | mic
        #[arg(long)]
        audio_src: Option<String>,
        /// Max video width (peer decode capability); 0 = unlimited
        #[arg(long)]
        max_width: Option<u32>,
        /// Bind to a specific local IPv4 interface (multi-NIC / VPN scenarios)
        #[arg(long)]
        local_ip: Option<IpAddr>,
        /// Video RTP port
        #[arg(long, short)]
        video_rtp_port: Option<u16>,
        /// Audio RTP port
        #[arg(long, short)]
        audio_rtp_port: Option<u16>,
        /// Enable ICE for SIP calls (standard SDP a=ice-*; falls back to
        /// plain RTP when the peer does not answer with ICE)
        #[arg(long)]
        ice: Option<bool>,
        /// STUN server URI used when --ice is enabled (empty = host only)
        #[arg(long)]
        stun: Option<String>,
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
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Commands::Discover {
            timeout_secs,
            json,
            username,
            password,
        } => ipc::discover::run(timeout_secs, json, username, password).await?,
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
            .await?
        }
        Commands::DecodeBench { path, max_frames } => {
            ipc::decode_bench::run(&path, max_frames).await?
        }
        Commands::Audio { action } => match action {
            AudioAction::ListDevices => ipc::audio::list_devices(),
        },
        Commands::V4l2 { action } => match action {
            V4l2Action::List => ipc::v4l2::list(),
        },
        Commands::Gui {
            server,
            username,
            password,
            expires,
            video_src,
            audio_src,
            max_width,
            local_ip,
            video_rtp_port,
            audio_rtp_port,
            ice,
            stun,
        } => {
            call_ui::run(call_ui::GuiSipOverrides {
                server,
                username,
                password,
                expires,
                local_ip,
                video_src,
                audio_src,
                max_width,
                ice,
                stun,
                video_rtp_port,
                audio_rtp_port,
            })?;
        }
    }
    Ok(())
}
