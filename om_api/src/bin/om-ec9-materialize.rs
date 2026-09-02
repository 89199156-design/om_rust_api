use anyhow::{bail, Context, Result};
use clap::Parser;
use om_api::ec9_materialize::{build_and_publish_ec9_coverage, Ec9BuildOptions};
use om_api::official::OfficialDecoder;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Materialize and atomically publish an EC9 RegionPack release as native Open-Meteo OM"
)]
struct Args {
    #[arg(long, env = "OM_OMFILE_LIB")]
    omfile_lib: PathBuf,

    /// Root containing the complete RegionPack group marker.  This may be an
    /// event-specific staging root and is never retained by the native output.
    #[arg(long)]
    source_root: PathBuf,

    #[arg(long, env = "OM_DATA_ROOT", default_value = "/data/om_raw")]
    data_root: PathBuf,

    #[arg(long, env = "OM_API_SOURCE_REVISION")]
    producer_revision: Option<String>,

    #[arg(
        long,
        env = "OM_API_SOURCE_REVISION_FILE",
        default_value = "/opt/1panel/apps/weather_om_api/source-revision"
    )]
    producer_revision_file: PathBuf,

    #[arg(long, env = "OM_EC9_MATERIALIZE_WORKERS", default_value_t = 1)]
    workers: usize,

    #[arg(
        long,
        env = "OM_EC9_NATIVE_MINIMUM_FREE_BYTES",
        default_value_t = 2_147_483_648_u64
    )]
    minimum_free_bytes: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let revision = match args.producer_revision {
        Some(value) => value,
        None => fs::read_to_string(&args.producer_revision_file).with_context(|| {
            format!(
                "read installed source revision {}",
                args.producer_revision_file.display()
            )
        })?,
    };
    let revision = revision.trim().to_string();
    if args.workers == 0 {
        bail!("--workers must be positive");
    }
    let decoder = OfficialDecoder::load(&args.omfile_lib)?;
    let result = build_and_publish_ec9_coverage(
        &Ec9BuildOptions {
            source_root: args.source_root,
            data_root: args.data_root,
            producer_revision: revision,
            workers: args.workers,
            minimum_free_bytes: args.minimum_free_bytes,
        },
        &decoder,
    )?;
    serde_json::to_writer_pretty(std::io::stdout().lock(), &result)?;
    println!();
    Ok(())
}
