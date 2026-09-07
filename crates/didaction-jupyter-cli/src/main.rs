mod compose;
mod config;
mod tui;

use clap::{Parser, Subcommand};
use std::{fs, io, process::Command};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "djupctl",
    version,
    about = "Configure and run Didaction Jupyter kernels"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    Init,
    Config,
    Show,
    Up,
    Down,
    Status,
}

fn main() -> io::Result<()> {
    let action = Cli::parse().command.unwrap_or(Action::Config);
    match action {
        Action::Init => {
            let settings = config::Settings::default();
            let directory = config::save(&settings)?;
            let secrets = directory.join("secrets");
            fs::create_dir_all(&secrets)?;
            let token = secrets.join("jupyter-token");
            if !token.exists() {
                fs::write(token, format!("{}{}\n", Uuid::new_v4(), Uuid::new_v4()))?;
            }
            compose::write(&settings, &directory)?;
            println!("Initialized {}", directory.display());
            Ok(())
        }
        Action::Config => {
            let mut settings = config::load().unwrap_or_default();
            tui::run(&mut settings)?;
            let directory = config::save(&settings)?;
            compose::write(&settings, &directory)?;
            println!("Saved {}", directory.display());
            Ok(())
        }
        Action::Show => {
            println!("{:#?}", config::load()?);
            Ok(())
        }
        Action::Up => compose_command(["up", "-d"]),
        Action::Down => compose_command(["down"]),
        Action::Status => compose_command(["ps"]),
    }
}

fn compose_command<const N: usize>(args: [&str; N]) -> io::Result<()> {
    let path = config::directory()?.join("compose.yaml");
    let status = Command::new("docker")
        .arg("compose")
        .arg("--file")
        .arg(path)
        .args(args)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("docker compose failed"))
    }
}
