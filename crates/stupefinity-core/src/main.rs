// No console window in release builds; logs go to %LOCALAPPDATA%\Stupefinity\stupefinity.log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    stupefinity_core::run_app().await
}
