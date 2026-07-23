use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use om_api::materialize::{
    build_gfs_coverage, cleanup_stale_gfs_staging, default_gfs_coverage_id,
    latest_available_gfs_run, publish_gfs_coverage, validate_gfs_coverage, GfsBuildOptions,
};
use om_api::official::OfficialDecoder;
use serde::Serialize;
use serde_json::json;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Build, validate, and atomically publish native Open-Meteo GFS coverages"
)]
struct Cli {
    #[arg(long, env = "OM_OMFILE_LIB")]
    omfile_lib: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Materialize an immutable staging coverage without changing current.
    Build(BuildArgs),
    /// Decode-probe and validate an existing staging or immutable coverage.
    Validate {
        #[arg(long)]
        coverage_root: PathBuf,
        #[arg(
            long,
            env = "OM_DEM_ROOT",
            default_value = "/opt/1panel/apps/weather_om_api/static"
        )]
        dem_root: PathBuf,
    },
    /// Atomically promote a validated staging coverage and write current marker last.
    Publish {
        #[arg(long, env = "OM_DATA_ROOT", default_value = "/data/om_raw")]
        data_root: PathBuf,
        #[arg(
            long,
            env = "OM_DEM_ROOT",
            default_value = "/opt/1panel/apps/weather_om_api/static"
        )]
        dem_root: PathBuf,
        #[arg(long)]
        coverage_id: String,
    },
    /// Build, fully validate, and publish one native GFS coverage.
    BuildAndPublish(BuildArgs),
}

#[derive(Debug, Clone, ClapArgs)]
struct BuildArgs {
    #[arg(long, env = "OM_DATA_ROOT", default_value = "/data/om_raw")]
    data_root: PathBuf,

    #[arg(
        long,
        env = "OM_DEM_ROOT",
        default_value = "/opt/1panel/apps/weather_om_api/static"
    )]
    dem_root: PathBuf,

    /// Retained source run to materialize. Defaults to the newest complete
    /// retained GFS release, so production scheduling never hard-codes a batch.
    #[arg(long)]
    latest_run: Option<String>,

    #[arg(long)]
    coverage_id: Option<String>,

    #[arg(long, env = "OM_API_SOURCE_REVISION")]
    producer_revision: Option<String>,

    #[arg(
        long,
        env = "OM_API_SOURCE_REVISION_FILE",
        default_value = "/opt/1panel/apps/weather_om_api/source-revision"
    )]
    producer_revision_file: PathBuf,

    #[arg(long, env = "OM_NATIVE_MATERIALIZE_WORKERS", default_value_t = 2)]
    workers: usize,

    /// Free space that must remain after the conservative build estimate.
    #[arg(
        long,
        env = "OM_NATIVE_MINIMUM_FREE_BYTES",
        default_value_t = 2_147_483_648_u64
    )]
    minimum_free_bytes: u64,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .json()
        .init();
    let cli = Cli::parse();
    let decoder = OfficialDecoder::load(&cli.omfile_lib)?;
    match cli.command {
        Command::Build(args) => print_json(&build(&args, &decoder)?),
        Command::Validate {
            coverage_root,
            dem_root,
        } => print_json(&validate_gfs_coverage(&coverage_root, &dem_root, &decoder)?),
        Command::Publish {
            data_root,
            dem_root,
            coverage_id,
        } => print_json(&publish_gfs_coverage(
            &data_root,
            &dem_root,
            &coverage_id,
            &decoder,
        )?),
        Command::BuildAndPublish(args) => {
            let built = build(&args, &decoder)?;
            let published = publish_gfs_coverage(
                &args.data_root,
                &args.dem_root,
                &built.coverage_id,
                &decoder,
            )?;
            // Production invokes this command while holding the committed
            // helper's flock. Only post-publication, old managed staging is
            // eligible for age-gated cleanup.
            let staging_cleanup = cleanup_stale_gfs_staging(&args.data_root)?;
            print_json(&json!({
                "build": built,
                "publish": published,
                "staging_cleanup": staging_cleanup
            }))
        }
    }
}

fn build(
    args: &BuildArgs,
    decoder: &OfficialDecoder,
) -> Result<om_api::materialize::GfsBuildResult> {
    let producer_revision = resolve_producer_revision(args)?;
    let latest_run = match args.latest_run.as_deref() {
        Some(value) => value.to_string(),
        None => latest_available_gfs_run(&args.data_root)?,
    };
    let coverage_id = match args.coverage_id.clone() {
        Some(value) => value,
        None => default_gfs_coverage_id(&latest_run, &producer_revision)?,
    };
    build_gfs_coverage(
        &GfsBuildOptions {
            data_root: args.data_root.clone(),
            dem_root: args.dem_root.clone(),
            latest_run,
            coverage_id,
            producer_revision,
            workers: args.workers,
            minimum_free_bytes: args.minimum_free_bytes,
        },
        decoder,
    )
}

fn resolve_producer_revision(args: &BuildArgs) -> Result<String> {
    let revision = match args.producer_revision.as_deref() {
        Some(value) => value.trim().to_string(),
        None => fs::read_to_string(&args.producer_revision_file)
            .with_context(|| {
                format!(
                    "read installed source revision: {}",
                    args.producer_revision_file.display()
                )
            })?
            .trim()
            .to_string(),
    };
    if revision.len() != 40
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("producer revision must be a full lowercase 40-character Git commit SHA");
    }
    Ok(revision)
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let stdout = io::stdout();
    let mut locked = stdout.lock();
    serde_json::to_writer_pretty(&mut locked, value)?;
    locked.write_all(b"\n")?;
    Ok(())
}
