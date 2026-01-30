use std::borrow::Cow;
use std::ffi::OsStr;
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use std::process::Stdio;
use std::time::{Duration};
use log::error;
use port_check::is_port_reachable_with_timeout;
use simplelog::info;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use crate::{adb};
use crate::config::{AppConfig, ADB_DEVICE_PORT};
use simplelog;


///ADB wrapper, needs adb binary installed
pub(crate) fn parse_response_lines(rsp: Vec<u8>) -> Result<Vec<String>, String> {
    // Convert bytes to UTF-8 safely, replacing invalid bytes with �
    let s = String::from_utf8_lossy(&rsp);

    // Iterate over lines, take first tab-separated column, skip empty lines
    let response: Vec<String> = s
        .lines()
        .filter_map(|line| line.split('\t').next()) // first column
        .filter(|col| !col.is_empty())             // skip empty
        .map(|col| col.to_string())                // make owned
        .collect();

    Ok(response)
}

pub fn parse_response_lines_old(rsp: Vec<u8>) ->Result<Vec<String>, String>
{
    let lines=String::from_utf8_lossy(&rsp).to_string();
    let mut response = vec![];
    if !lines.is_empty() {
        lines.lines().into_iter().for_each(|line| {
            let parts: Vec<&str> = line.split("\t").collect();
            if !parts.is_empty() {
                response.push(parts[0].to_string());
            }
        })
    };
    Ok(response)
}

///Find an ADB device, connect to it and return TCP address
pub(crate) async fn get_first_adb_device( config: AppConfig) ->Option<String>
{
    // Run `ip -j neigh` asynchronously
    let cmd_ip_neigh = Command::new("ip")
        .args(&["neigh"])
        .output()
        .await
        .ok()?; // return None if command fails

    let stdout = String::from_utf8_lossy(&cmd_ip_neigh.stdout);

    for line in stdout.lines() {
        info!("Shell response for ip neigh: {:?}", line.to_string());
        let parts: Vec<&str> = line.split_whitespace().collect();

        // IP is always first
        let ip = match parts.get(0) {
            Some(v) => *v,
            None => continue,
        };

        // State is usually the last token
        let state = parts.last().copied();
        if !matches!(state, Some("REACHABLE")) {
            continue;
        }
        // Find lladdr <MAC>
        let mac = parts
            .windows(2)
            .find(|w| w[0] == "lladdr")
            .map(|w| w[1]);

        let mac = match mac {
            Some(v) => v,
            None => continue,
        };

        info!("Potential ADB client found: {:?} with MAC: {:?}", ip.to_string(), mac.to_string());
        let dev_port=ADB_DEVICE_PORT;
        // parse the &str into Ipv4Addr
        if let Ok(client_ip) = ip.parse::<Ipv4Addr>() {
            let dev_socket = SocketAddrV4::new(client_ip, dev_port);
            if is_port_reachable_with_timeout(dev_socket, Duration::from_secs(5))
            {
                info!("{:?} found port {} open, trying to connect to ADB demon. MAC= {:?}", ip.to_string(), dev_port, mac.to_string());
                let cmd_connect = Command::new("adb")
                    .arg("connect")
                    .arg(dev_socket.to_string())
                    .output().await.unwrap();
                let lines=adb::parse_response_lines(cmd_connect.stdout).expect("TODO: panic message");
                if lines.len() > 0 {
                    for line in lines {
                        info!("ADB connect response: {:?}", line);
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                let cmd_dev = Command::new("adb")
                    .arg("devices")
                    .output().await.unwrap();
                let lines=adb::parse_response_lines(cmd_dev.stdout).expect("TODO: panic message");
                if lines.len() > 0 {
                    for line in lines {
                        info!("ADB devices response: {:?}", line);
                        if line.contains(&dev_socket.to_string()) {
                            return  Some(dev_socket.to_string());

                        }
                    }
                }

            }
        }
        else
        {
            error!("Invalid IP address: {}", ip);
        }
    }
    None
}

pub(crate) async fn run_piped_cmd<I,S>(args: I) ->Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    I: IntoIterator<Item = S>,
    I::Item: AsRef<OsStr>,
{
    let mut adb_cmd = Command::new("adb")
        .args(args)
        .stdout(Stdio::piped())
        //.stderr(Stdio::piped())
        .spawn()?;

    let stdout = adb_cmd.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();

    if let Some(line) = lines.next_line().await? {
        //info!("ADB piped stdout: {:?}", line);
        return Ok(line);
    }

    Err("no output received".into())
}

pub(crate) async fn shell_cmd<I,S>(args: I) ->Result<(tokio::process::Child, BufReader<tokio::process::ChildStdout>, String), Box<dyn std::error::Error + Send + Sync>>
where
    I: IntoIterator<Item = S>,
    I::Item: AsRef<OsStr>,
{
    // Collect args so we can reuse them
    let args: Vec<S> = args.into_iter().collect();

    let mut adb_cmd = Command::new("adb")
        .arg("shell")
        //.args(args)
        .args(args)
        .stdout(Stdio::piped())
        //.stderr(Stdio::piped())
        .spawn()?;

    let stdout = adb_cmd
        .stdout
        .take()
        .ok_or("stdout not piped")?;
    let mut reader = BufReader::new(stdout);
    let mut buf = vec![0u8; 1024];

    // Read *some* output, not a full line
    let n = reader.read(&mut buf).await?;
    if n == 0 {
        return Err("no output received".into());
    }

    let first_output = String::from_utf8_lossy(&buf[..n]).to_string();

    // IMPORTANT:
    // - child is still alive
    // - stdout is now partially consumed
    // - process keeps running
    Ok((adb_cmd,reader, first_output))
}

pub(crate) async fn run_cmd<I, S>(args: I) ->Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>>
where
    I: IntoIterator<Item = S>,
    I::Item: AsRef<OsStr>,
{
    let adb_cmd = Command::new("adb")
        .args(args)
        .output().await?;
    // Optional: check exit status
    /*if !adb_cmd.status.success() {
        return Err(format!("process exited with {}", adb_cmd.status).into());
    }*/
    let raw_rsp=adb_cmd.stdout;
    info!("ADB stdout: {:?}", raw_rsp);
    let stdout = String::from_utf8_lossy(&raw_rsp);

    Ok(stdout.lines().filter_map(|line| line.split('\t').next()) // first column
           .filter(|col| !col.is_empty())             // skip empty
           .map(|col| col.to_string())                // make owned
           .collect())
}

pub(crate) async fn push_cmd<I, S>(args: I) ->Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>>
where
    I: IntoIterator<Item = S>,
    I::Item: AsRef<OsStr>,
{
    let adb_cmd = Command::new("adb")
        .arg("push")
        .args(args)
        .output().await?;
    let raw_rsp=adb_cmd.stderr;
    //info!("ADB stdout: {:?}", raw_rsp);
    let stdout = String::from_utf8_lossy(&raw_rsp);

    Ok(stdout.lines().filter_map(|line| line.split('\t').next()) // first column
        .filter(|col| !col.is_empty())             // skip empty
        .map(|col| col.to_string())                // make owned
        .collect())
}

pub(crate) async fn forward_cmd<I, S>(args: I) ->Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>>
where
    I: IntoIterator<Item = S>,
    I::Item: AsRef<OsStr>,
{
    let adb_cmd = Command::new("adb")
        .arg("forward")
        .args(args)
        .output().await?;
    let raw_rsp=adb_cmd.stderr;
    //info!("ADB stdout: {:?}", raw_rsp);
    let stdout = String::from_utf8_lossy(&raw_rsp);

    Ok(stdout.lines().filter_map(|line| line.split('\t').next()) // first column
        .filter(|col| !col.is_empty())             // skip empty
        .map(|col| col.to_string())                // make owned
        .collect())
}

/// Escape a shell argument safely for single-token usage
fn shell_escape(s: &str) -> String {
    if s.contains([' ', '"', '\'', '$', '&', '|', ';', '<', '>']) {
        // Wrap in single quotes, escape existing single quotes
        format!("'{}'", s.replace('\'', r"'\''"))
    } else {
        s.to_string()
    }
}
