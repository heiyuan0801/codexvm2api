use std::error::Error;

use codex_slot_sidecar::SidecarConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = SidecarConfig::from_env()?;
    codex_slot_sidecar::serve(config).await?;
    Ok(())
}
