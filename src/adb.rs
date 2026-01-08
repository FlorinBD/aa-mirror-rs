use std::borrow::Cow;
use std::ffi::OsStr;
use std::net::SocketAddrV4;
use std::process::Stdio;
use std::time::{Duration, Instant};
use async_arp::{Client, ClientConfigBuilder, ClientSpinner, ProbeStatus};
use port_check::is_port_reachable_with_timeout;
use simplelog::info;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use crate::{adb, arp_common};
use crate::config::{AppConfig, ADB_DEVICE_PORT, ADB_SERVER_PORT};
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

pub(crate) async fn get_first_adb_device(config: AppConfig) ->Option<String>
{
    let interface = arp_common::interface_from(&config.iface);
    let arp_client = Client::new(
        ClientConfigBuilder::new(&config.iface)
            .with_response_timeout(Duration::from_millis(500))
            .build(),
    ).unwrap();
    let spinner = ClientSpinner::new(arp_client).with_retries(3);
    let net = arp_common::net_from(&interface).unwrap();
    let start = Instant::now();
    let outcomes = spinner
        .probe_batch(&arp_common::generate_probe_inputs(net, interface))
        .await;

    let occupied = outcomes.unwrap()
        .into_iter()
        .filter(|outcome| outcome.status == ProbeStatus::Occupied);
    //let scan_duration = start.elapsed();
    info!("Found hosts: {}", occupied.clone().count());
    let dev_port=ADB_DEVICE_PORT;
    let mut connected_dev=String::from("");
    for outcome in occupied {
        //info!("ADB try to connect to {:?}", outcome.target_ip);
        let dev_socket=SocketAddrV4::new(outcome.target_ip, dev_port);
        if is_port_reachable_with_timeout(dev_socket, Duration::from_secs(5))
        {
            info!("{:?} found port {} open, trying to connect to ADB demon", outcome.target_ip, dev_port);
            let cmd_connect = Command::new("adb")
                .arg("connect")
                .arg(dev_socket.to_string())
                .output().await.unwrap();
            let lines=adb::parse_response_lines(cmd_connect.stdout).expect("TODO: panic message");
            if lines.len() > 0 {
                for line in lines {
                    info!("ADB connect response: {:?}", line);
                    if line.contains("connected") {
                        connected_dev=dev_socket.to_string();
                    }
                }
            }

            let cmd_dev = Command::new("adb")
                .arg("devices")
                .output().await.unwrap();
            let lines=adb::parse_response_lines(cmd_dev.stdout).expect("TODO: panic message");
            if lines.len() > 0 {
                for line in lines {
                    info!("ADB devices response: {:?}", line);
                    if line.contains("device") {
                        if connected_dev.is_empty() {
                            return None;
                        }
                        else {
                            return  Some(connected_dev);
                        }

                    }
                }
            }
        }
    }
    //info!("ADB Scan took {:?} seconds", scan_duration.as_secs());
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
        info!("ADB piped stdout: {:?}", line);
        return Ok(line);
    }

    Err("no output received".into())
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