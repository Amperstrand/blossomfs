// BlossomFS — Read-only FUSE filesystem for Blossom/Nostr media
//
// This is the main entry point. CLI parsing and startup orchestration
// will be implemented in later waves.

mod blossom;
mod cache;
mod cli;
mod fuse;
mod nostr;
mod util;

fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("blossomfs starting up...");
    eprintln!("blossomfs: FUSE filesystem for Blossom/Nostr media");
    eprintln!("blossomfs: CLI and mount logic not yet implemented (Wave 1 scaffold)");
}
