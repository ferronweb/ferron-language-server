use std::path::PathBuf;

use clap::Parser;

mod directives;
mod ferron_dl;
mod lsp;

shadow_rs::shadow!(build);

/// A language server for "ferron.conf" configuration files
#[derive(Debug, clap::Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// A path to the extracted contents of a Ferron 3 archive (downloaded from dl.ferron.sh).
    /// By default, the archive is downloaded and extracted automatically.
    #[arg(long = "ferron")]
    path_to_ferron: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let path_to_ferron = if let Some(path) = &args.path_to_ferron {
        path.clone()
    } else {
        crate::ferron_dl::obtain_ferron().await?
    };
    crate::lsp::lsp_main(path_to_ferron).await?;
    Ok(())
}
