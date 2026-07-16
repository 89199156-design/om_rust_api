use anyhow::Result;
use clap::Parser;
use om_api::api::{serve, AppState};
use om_api::official::OfficialDecoder;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(version, about = "Open-Meteo point API over .omranges bundles")]
struct Args {
    #[arg(long, env = "OM_DATA_ROOT", default_value = "/data/om_raw")]
    data_root: PathBuf,

    #[arg(long, env = "OM_API_BIND", default_value = "0.0.0.0:8088")]
    bind: SocketAddr,

    #[arg(long, env = "OM_OMFILE_LIB")]
    omfile_lib: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let args = Args::parse();
    let decoder = match args.omfile_lib.as_ref() {
        Some(path) => Some(OfficialDecoder::load(path)?),
        None => None,
    };
    let state = AppState::new(args.data_root, decoder)?;
    serve(state, args.bind).await
}
