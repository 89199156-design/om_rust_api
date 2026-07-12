use anyhow::{bail, Context, Result};
use clap::Parser;
use image::ImageReader;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Inspect and decode one lossless WebP data pixel")]
struct Args {
    file: PathBuf,
    #[arg(long)]
    x: u32,
    #[arg(long)]
    y: u32,
    #[arg(long)]
    vmin: Option<f32>,
    #[arg(long)]
    scale: Option<f32>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let image = ImageReader::open(&args.file)
        .with_context(|| format!("open {}", args.file.display()))?
        .with_guessed_format()?
        .decode()?
        .to_rgba8();
    if args.x >= image.width() || args.y >= image.height() {
        bail!(
            "pixel ({},{}) is outside {}x{} image",
            args.x,
            args.y,
            image.width(),
            image.height()
        );
    }
    let pixel = image.get_pixel(args.x, args.y).0;
    let encoded = u16::from_be_bytes([pixel[0], pixel[1]]);
    let value = match (args.vmin, args.scale) {
        (Some(vmin), Some(scale)) if pixel[3] != 0 => Some(encoded as f32 / scale + vmin),
        _ => None,
    };
    println!(
        "{}",
        serde_json::json!({
            "width": image.width(),
            "height": image.height(),
            "x": args.x,
            "y": args.y,
            "rgba": pixel,
            "encoded_u16": encoded,
            "value": value,
        })
    );
    Ok(())
}
