use call_ui::session::{DEFAULT_AUDIO_RTP_PORT, DEFAULT_VIDEO_RTP_PORT, RtpPorts};
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
    /// SIP GUI (Slint): register + dial + answer
    Gui {
        /// SIP server address
        #[arg(long, default_value = "192.168.11.17:5062")]
        server: SocketAddr,
        /// SIP account (also used as username)
        #[arg(short, long, default_value = "9999")]
        username: String,
        /// Password
        #[arg(short, long, default_value = "changeme")]
        password: String,
        /// Registration expiry (seconds)
        #[arg(short, long, default_value_t = 60)]
        expires: u32,
        /// Video source: file:<path> | rtsp://<uri> | camera[:<device>]
        #[arg(long, default_value = "camera")]
        video_src: String,
        /// Audio source: file:<path> | mic
        #[arg(long, default_value = "mic")]
        audio_src: String,
        /// Max video width (peer decode capability); 0 = unlimited
        #[arg(long, default_value_t = 1920)]
        max_width: u32,
        /// Bind to a specific local IPv4 interface (multi-NIC / VPN scenarios)
        #[arg(long)]
        local_ip: Option<IpAddr>,
        /// Video RTP port
        #[arg(long, short, default_value_t = DEFAULT_VIDEO_RTP_PORT)]
        video_rtp_port: u16,
        /// Audio RTP port
        #[arg(long, short, default_value_t = DEFAULT_AUDIO_RTP_PORT)]
        audio_rtp_port: u16,
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
        } => {
            let local_ip = match local_ip {
                Some(IpAddr::V4(v4)) => Some(v4),
                Some(_) => anyhow::bail!("only support IPv4 for now!"),
                None => None,
            };
            let cfg = call_ui::GuiSipConfig {
                server,
                username,
                password,
                expires,
                local_ip,
                audio_src: ipcam_gst::parse_track_source(&audio_src).map_err(anyhow::Error::msg)?,
                video_src: ipcam_gst::parse_track_source(&video_src).map_err(anyhow::Error::msg)?,
                max_width,
                rtp_ports: RtpPorts {
                    audio: audio_rtp_port,
                    video: video_rtp_port,
                },
            };
            call_ui::run(cfg)?;
        }
    }
    Ok(())
}
