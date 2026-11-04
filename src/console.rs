//! Server console access for the benchmark: set up scenarios with commands
//! like `/tp`, `/give` and `/time set`.
//!
//! `MC_CONSOLE` picks the transport:
//! - `rcon:localhost:25575` with the password in `MC_RCON_PASSWORD`
//!   (`enable-rcon=true` in server.properties)
//! - `fifo:/path/to/pipe`, a named pipe the server reads as stdin

use std::{io::Write, path::PathBuf};

use anyhow::{bail, Context, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub enum Console {
    Rcon { address: String, password: String },
    Fifo(PathBuf),
}

impl Console {
    pub fn from_env() -> Result<Self> {
        let spec = std::env::var("MC_CONSOLE")
            .context("set MC_CONSOLE to rcon:host:port (password in MC_RCON_PASSWORD) or fifo:/path/to/pipe")?;
        if let Some(address) = spec.strip_prefix("rcon:") {
            let password = std::env::var("MC_RCON_PASSWORD").context("MC_RCON_PASSWORD is not set")?;
            Ok(Self::Rcon {
                address: address.to_string(),
                password,
            })
        } else if let Some(path) = spec.strip_prefix("fifo:") {
            Ok(Self::Fifo(path.into()))
        } else {
            bail!("MC_CONSOLE must start with rcon: or fifo:")
        }
    }

    /// Runs one command (without the leading slash). RCON returns the
    /// server's reply; a pipe cannot, so it returns an empty string.
    pub async fn run(&self, command: &str) -> Result<String> {
        let command = command.trim_start_matches('/');
        match self {
            Self::Fifo(path) => {
                let path = path.clone();
                let line = format!("{command}\n");
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let mut pipe = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .with_context(|| format!("opening {}", path.display()))?;
                    pipe.write_all(line.as_bytes())?;
                    Ok(())
                })
                .await??;
                Ok(String::new())
            }
            Self::Rcon { address, password } => {
                let mut stream = TcpStream::connect(address).await.with_context(|| format!("connecting to RCON at {address}"))?;
                send_packet(&mut stream, 1, 3, password).await?;
                let (id, _) = read_packet(&mut stream).await?;
                if id == -1 {
                    bail!("RCON password rejected");
                }
                send_packet(&mut stream, 2, 2, command).await?;
                let (_, body) = read_packet(&mut stream).await?;
                Ok(body)
            }
        }
    }
}

async fn send_packet(stream: &mut TcpStream, id: i32, kind: i32, body: &str) -> Result<()> {
    let mut packet = Vec::with_capacity(body.len() + 14);
    packet.extend_from_slice(&((body.len() + 10) as i32).to_le_bytes());
    packet.extend_from_slice(&id.to_le_bytes());
    packet.extend_from_slice(&kind.to_le_bytes());
    packet.extend_from_slice(body.as_bytes());
    packet.extend_from_slice(&[0, 0]);
    stream.write_all(&packet).await?;
    Ok(())
}

async fn read_packet(stream: &mut TcpStream) -> Result<(i32, String)> {
    let len = stream.read_i32_le().await? as usize;
    if !(10..=4110).contains(&len) {
        bail!("bad RCON packet length {len}");
    }
    let mut buf = vec![0; len];
    stream.read_exact(&mut buf).await?;
    let id = i32::from_le_bytes(buf[0..4].try_into().unwrap());
    let body = String::from_utf8_lossy(&buf[8..len - 2]).to_string();
    Ok((id, body))
}
