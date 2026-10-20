use std::error::Error;

use clap::{Parser, ValueEnum};
#[cfg(feature = "desktop")]
use fft_plug::dsp::DspConfig;
use fft_plug::dsp::DynamicsProfile;

#[cfg(feature = "desktop")]
mod audio;
#[cfg(feature = "desktop")]
mod render;
#[cfg(feature = "desktop")]
mod shell;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    Bars,
    Curve,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DynamicsArg {
    Soft,
    Balanced,
    Punchy,
}

impl From<DynamicsArg> for DynamicsProfile {
    fn from(value: DynamicsArg) -> Self {
        match value {
            DynamicsArg::Soft => Self::Soft,
            DynamicsArg::Balanced => Self::Balanced,
            DynamicsArg::Punchy => Self::Punchy,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "fft-plug",
    version,
    about = "PipeWire default-output spectrum visualizer for niri"
)]
struct Cli {
    #[arg(long, value_enum, default_value = "bars")]
    mode: Mode,

    /// Choose the DSP attack/release response profile.
    #[arg(long, value_enum, default_value = "punchy")]
    dynamics: DynamicsArg,

    /// Print one low-frequency diagnostic snapshot per second.
    #[arg(long)]
    debug: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fft-plug: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    #[cfg(feature = "desktop")]
    {
        let config = DspConfig::default().with_dynamics(cli.dynamics.into());
        let pipeline = fft_plug::pipeline::Pipeline::start(config)?;
        let status = std::sync::Arc::new(audio::AudioStatus::new());
        let mut capture =
            match audio::AudioCapture::start(pipeline.ring(), std::sync::Arc::clone(&status)) {
                Ok(capture) => capture,
                Err(error) => {
                    pipeline.shutdown();
                    return Err(error.into());
                }
            };
        let mode = match cli.mode {
            Mode::Bars => render::RenderMode::Bars,
            Mode::Curve => render::RenderMode::Curve,
        };
        let shell_result = shell::run(mode, std::sync::Arc::clone(&pipeline), status, cli.debug);
        let capture_result = capture.stop();
        pipeline.shutdown();
        shell_result?;
        capture_result?;
        Ok(())
    }

    #[cfg(not(feature = "desktop"))]
    {
        let _ = cli;
        Err("fft-plug was built without the desktop feature".into())
    }
}
