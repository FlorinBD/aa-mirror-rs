use bytesize::ByteSize;
use humantime::format_duration;
use simplelog::*;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use core::net::SocketAddr;
use std::collections::VecDeque;
use std::net::IpAddr;
use anyhow::Context;
use mac_address::MacAddress;
use nix::sys::prctl::get_name;
use tokio::sync::broadcast::Sender as BroadcastSender;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio::fs::File as TokioFile;
use tokio::io;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_uring::buf::BoundedBuf;
use tokio_uring::buf::BoundedBufMut;
use tokio_uring::fs::File;
use tokio_uring::fs::OpenOptions;
use tokio_uring::net::TcpListener;
use tokio_uring::net::TcpStream;
use tokio_uring::BufResult;
use tokio_uring::UnsubmittedWrite;
use crate::{bluetooth, scrcpy};
use crate::channel_manager::{ChannelProxyHandle, PacketProxy, SslMemBuf, HEADER_LENGTH, KEYS_PATH};
use crate::aa_services::{VideoStreamingParams, AudioStreamingParams};
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use protos::*;

// module name for logging engine
const NAME: &str = "<i><bright-black> io_uring: </>";

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const USB_ACCESSORY_PATH: &str = "/dev/usb_accessory";
pub const BUFFER_LEN: usize = 16 * 1024;
pub const TCP_CLIENT_TIMEOUT: Duration = Duration::new(30, 0);
const TCP_BUFFER_SIZE: usize = 32 * 1024; // 32 KB
// Keep only a small number of fully parsed AA packets buffered between tasks.
// A slow HU should stall the phone-side reader quickly so TCP backpressure can
// reach Android and trigger bitrate/resolution adaptation.

use crate::config::{Action, AppConfig, SharedConfig, WifiConfig, DEFAULT_WLAN_ADDR, TCP_DHU_PORT, TCP_MD_SERVER_PORT};
use crate::channel_manager::{endpoint_reader, ch_proxy, ENCRYPTED, FRAME_TYPE_FIRST, FRAME_TYPE_LAST};
use crate::channel_manager::Packet;
use crate::config_types::AAMode;
use crate::usb_gadget::UsbGadgetState;
use crate::usb_stream::{UsbStreamRead, UsbStreamWrite};

// tokio_uring::fs::File and tokio_uring::net::TcpStream are using different
// read and write calls:
// File is using read_at() and write_at(),
// TcpStream is using read() and write()
//
// In our case we are reading a special unix character device for
// the USB gadget, which is not a regular file where an offset is important.
// We just use offset 0 for reading and writing, so below is a trait
// for this, to be able to use it in a generic copy() function below.

pub trait Endpoint<E> {
    #[allow(async_fn_in_trait)]
    async fn read<T: BoundedBufMut>(&self, buf: T) -> BufResult<usize, T>;
    fn write<T: BoundedBuf>(&self, buf: T) -> UnsubmittedWrite<T>;
}

impl Endpoint<File> for File {
    async fn read<T: BoundedBufMut>(&self, buf: T) -> BufResult<usize, T> {
        self.read_at(buf, 0).await
    }
    fn write<T: BoundedBuf>(&self, buf: T) -> UnsubmittedWrite<T> {
        self.write_at(buf, 0)
    }
}

impl Endpoint<TcpStream> for TcpStream {
    async fn read<T: BoundedBufMut>(&self, buf: T) -> BufResult<usize, T> {
        self.read(buf).await
    }
    fn write<T: BoundedBuf>(&self, buf: T) -> UnsubmittedWrite<T> {
        self.write(buf)
    }
}

pub enum IoDevice<A: Endpoint<A>> {
    UsbReader(Arc<RefCell<UsbStreamRead>>, PhantomData<A>),
    UsbWriter(Arc<RefCell<UsbStreamWrite>>, PhantomData<A>),
    EndpointIo(Arc<A>),
    TcpStreamIo(Arc<TcpStream>),
}

/// Set SO_RCVBUF / SO_SNDBUF on any socket via its raw file descriptor.
/// Works for both `tokio::net::TcpStream` and `tokio_uring::net::TcpStream`
/// because both implement `AsRawFd`.
pub fn apply_tcp_buffer_sizes(fd: std::os::unix::io::RawFd) {
    use libc::{setsockopt, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};
    let buf_size = TCP_BUFFER_SIZE as libc::c_int;
    unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_RCVBUF,
            &buf_size as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_SNDBUF,
            &buf_size as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}
async fn transfer_monitor(
    stats_interval: Option<Duration>,
    usb_bytes_written: Arc<AtomicUsize>,
    tcp_bytes_written: Arc<AtomicUsize>,
    read_timeout: Duration,
    config: SharedConfig,
) -> Result<()> {
    let mut usb_bytes_out_last: usize = 0;
    let mut tcp_bytes_out_last: usize = 0;
    let mut stall_usb_bytes_last: usize = 0;
    let mut stall_tcp_bytes_last: usize = 0;
    let mut report_time = Instant::now();
    let mut stall_check = Instant::now();

    info!(
        "{} ⚙️ Showing transfer statistics: <b><blue>{}</>",
        NAME,
        match stats_interval {
            Some(d) => format_duration(d).to_string(),
            None => "disabled".to_string(),
        }
    );

    loop {
        // load current total transfer from AtomicUsize:
        let usb_bytes_out = usb_bytes_written.load(Ordering::Relaxed);
        let tcp_bytes_out = tcp_bytes_written.load(Ordering::Relaxed);

        // Stats printing
        if stats_interval.is_some() && report_time.elapsed() > stats_interval.unwrap() {
            // compute USB transfer
            usb_bytes_out_last = usb_bytes_out - usb_bytes_out_last;
            let usb_transferred_total = ByteSize::b(usb_bytes_out.try_into().unwrap());
            let usb_transferred_last = ByteSize::b(usb_bytes_out_last.try_into().unwrap());
            let usb_speed: u64 =
                (usb_bytes_out_last as f64 / report_time.elapsed().as_secs_f64()).round() as u64;
            let usb_speed = ByteSize::b(usb_speed);

            // compute TCP transfer
            tcp_bytes_out_last = tcp_bytes_out - tcp_bytes_out_last;
            let tcp_transferred_total = ByteSize::b(tcp_bytes_out.try_into().unwrap());
            let tcp_transferred_last = ByteSize::b(tcp_bytes_out_last.try_into().unwrap());
            let tcp_speed: u64 =
                (tcp_bytes_out_last as f64 / report_time.elapsed().as_secs_f64()).round() as u64;
            let tcp_speed = ByteSize::b(tcp_speed);

            info!(
                "{} {} {: >9} ({: >9}/s), {: >9} total | {} {: >9} ({: >9}/s), {: >9} total",
                NAME,
                "phone -> car 🔺",
                usb_transferred_last.to_string_as(true),
                usb_speed.to_string_as(true),
                usb_transferred_total.to_string_as(true),
                "car -> phone 🔻",
                tcp_transferred_last.to_string_as(true),
                tcp_speed.to_string_as(true),
                tcp_transferred_total.to_string_as(true),
            );

            // save values for next iteration
            report_time = Instant::now();
            usb_bytes_out_last = usb_bytes_out;
            tcp_bytes_out_last = tcp_bytes_out;
        }

        // transfer stall detection
        if stall_check.elapsed() > read_timeout {
            // compute delta since last check
            stall_usb_bytes_last = usb_bytes_out - stall_usb_bytes_last;
            stall_tcp_bytes_last = tcp_bytes_out - stall_tcp_bytes_last;

            if stall_usb_bytes_last == 0 || stall_tcp_bytes_last == 0 {
                return Err("Packet transfer watchdog: unexpected transfer stall, timeout exceeded".into());
            }

            // save values for next iteration
            stall_check = Instant::now();
            stall_usb_bytes_last = usb_bytes_out;
            stall_tcp_bytes_last = tcp_bytes_out;
        }

        // check pending action
        let action = config.read().await.action_requested.clone();
        if let Some(action) = action {
            // check if we need to restart or reboot
            if action == Action::Reconnect {
                config.write().await.action_requested = None;
            }
            return Err(format!("action request: {:?}", action).into());
        }

        sleep(Duration::from_millis(100)).await;
    }
}
fn init_wifi_config(cfg: &AppConfig) -> WifiConfig {
    let mut ip_addr = String::from(DEFAULT_WLAN_ADDR);

    // Get UP interface and IP
    for ifa in netif::up().unwrap() {
        match ifa.name() {
            val if val == cfg.iface => {
                debug!("Found interface: {:?}", ifa);
                // IPv4 Address contains None scope_id, while IPv6 contains Some
                match ifa.scope_id() {
                    None => {
                        ip_addr = ifa.address().to_string();
                        break;
                    }
                    _ => (),
                }
            }
            _ => (),
        }
    }

    let bssid = mac_address::mac_address_by_name(&cfg.iface)
        .expect(&format!("mac_address_by_name for {:?}", cfg.iface))
        .expect(&format!(
            "No MAC address found for interface: {:?}",
            cfg.iface
        ))
        .to_string();

    WifiConfig {
        ip_addr,
        port: TCP_MD_SERVER_PORT,
        ssid: cfg.ssid.clone(),
        bssid,
        wpa_key: cfg.wpa_passphrase.clone(),
    }
}
async fn flatten<T>(handle: &mut JoinHandle<Result<T>>, dbg_info:String) -> Result<T> {
    match handle.await {
        Ok(Ok(result)) => {
            error!("Task {} finished cleanly", dbg_info);
            Ok(result)
        },
        Ok(Err(err)) => {
            error!("Task {} finished with error: {:?}", dbg_info, err);
            Err(err.into())
        },
        Err(er) => {
            error!("Task handling failed for {} with error: {:?}", dbg_info, er);
            Err(er.into())
        },
    }
}

/// Async lookup MAC from IPv4 using /proc/net/arp
pub async fn mac_from_ipv4(addr: SocketAddr) -> Result<Option<MacAddress>> {
    let ip = match addr.ip() {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => return Ok(None),
    };

    let file = TokioFile::open("/proc/net/arp").await?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    // Skip header
    lines.next_line().await?;

    while let Some(line) = lines.next_line().await? {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 4 && cols[0] == ip.to_string() {
            if let Ok(mac) = cols[3].parse::<MacAddress>() {
                return Ok(Some(mac));
            }
        }
    }

    Ok(None)
}


/// Asynchronously wait for an inbound TCP connection
/// returning TcpStream of first client connected
async fn tcp_wait_for_md_connection(listener: & TcpListener) ->  Result<(TcpStream, SocketAddr)>  {
    let retval = listener.accept();
    let (stream, addr) = match timeout(TCP_CLIENT_TIMEOUT, retval)
        .await
        .map_err(|e| std::io::Error::other(e))
    {
        Ok(Ok((stream, addr))) => (stream, addr),
        Err(e) | Ok(Err(e)) => {
            error!("{} 📵 TCP server: {}, restarting...", NAME, e);
            return Err(Box::new(e));
        }
    };
    info!(
        "{} 📳 TCP server: new client connected: <b>{:?}</b>",
        NAME, addr
    );

    // Disable Nagle's algorithm for
    // high-throughput Android Auto video streaming.
    use std::os::unix::io::AsRawFd;
    stream.set_nodelay(true)?;
    //apply_tcp_buffer_sizes(stream.as_raw_fd());

    Ok((stream, addr))
}

async fn tcp_wait_for_hu_connection(listener: & TcpListener) -> Result<TcpStream> {
    // Accept one client
    let (stream, addr) = listener.accept().await?;
    println!("DHU Client connected: {:?}", addr);

    // Disable Nagle algorithm if you want low-latency small packets
    stream.set_nodelay(true)?;
    Ok(stream)
}

pub async fn usb_wait_for_hu_connection(timeout_secs: u64) -> Result<()> {
    let path = "/sys/class/android_usb/android0/state";

    let fut = async {
        loop {
            match std::fs::read_to_string(path) {
                Ok(state) => {
                    let state = state.trim();
                    debug!("{} USB state: {:?}", NAME, state);

                    if state == "CONFIGURED" {
                        return Ok(());
                    }
                }
                Err(e) => {
                    debug!("{}: USB state read error: {:?}", NAME, e);
                }
            }

            sleep(Duration::from_millis(200)).await;
        }
    };
    timeout(Duration::from_secs(timeout_secs), fut).await.unwrap_or_else(|_| Err("timeout".into()))
}

async fn enable_usb_if_present(usb: &mut Option<UsbGadgetState>) {
    if let Some(ref mut usb) = usb {
        usb.switch_to__accessory().await;
    }
}

async fn packet_proxy_pt<A: Endpoint<A>>(mut hu_rx: Receiver<Packet>,
                                         mut hu_tx: IoDevice<A>,
                                         mut md_rx: Receiver<Packet>,
                                         mut md_tx: IoDevice<TcpStream>,
                                         r_statistics: Arc<AtomicUsize>,
                                         w_statistics: Arc<AtomicUsize>,
                                        ) -> Result<()>
{

    info!( "{}: Starting message proxy loop MD<>HU", NAME);
    loop {
        tokio::select! {
            biased;

            // 🔴 highest priority, MD>HU
            Some(mut msg)=md_rx.recv() => {
                     // Increment byte counters for statistics
                        w_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                        msg.transmit(&mut hu_tx).await.with_context(|| format!("{}: Service transmit to HU failed", NAME))?;
            }
            // lower priority, HU>Service
            Some(mut msg) = hu_rx.recv() => {
                // Increment byte counters for statistics
                r_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                msg.transmit(&mut md_tx).await.with_context(|| format!("{}: Service transmit to MD failed", NAME))?;
            }
            else => {
            // all channels closed
            tokio::time::sleep(Duration::from_secs(1)).await;
                error!("packet_proxy_pt ALL CHANNELS CLOSED! handle app restart needed")
            }
        }
    }
}
///
/// IO Loop for Mirror mode only
pub async fn io_loop(
    need_restart: BroadcastSender<Option<Action>>,
    config: SharedConfig,
    tx: Arc<Mutex<Option<Sender<Packet>>>>,
) -> Result<()> {
    let shared_config = config.clone();
    #[allow(unused_variables)]
    //check if RSA cert files are present, if not, stop, this is FATAL error
    loop {
        let path_cert = format!("{KEYS_PATH}/md_cert.pem");
        let path_prv_key = format!("{KEYS_PATH}/md_key.pem");
        let path_gal_cert = format!("{KEYS_PATH}/galroot_cert.pem");
        if (!Path::new(&path_cert).exists()) || (!Path::new(&path_prv_key).exists()) || (!Path::new(&path_gal_cert).exists()){
            error!("{}: FATAL, RSA CERT Files doesn't exists", NAME);
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }
        break;
    }


    let cfg = shared_config.read().await.clone();
    let cfg_clone=cfg.clone();
    let hex_requested = cfg.hexdump_level;
    // prepare/bind needed TCP listeners
    let mut dhu_listener=None;
    let bind_addr = format!("0.0.0.0:{}", TCP_DHU_PORT).parse().unwrap();
    info!("{} 🛰️ Starting TCP server for DHU...", NAME);
    dhu_listener = Some(TcpListener::bind(bind_addr).unwrap());
    info!("{} 🛰️ DHU TCP server bound to: <u>{}</u>", NAME, bind_addr);

    //io channels for scrcpy
    //media frames channel, scrcpy>HU, TODO implement Arc<Packet> to solve copy
    let (tx_scrcpy, rx_scrcpy)=flume::bounded::<ChannelProxyHandle>(60);
    //cmd srv>scrcpy channel
    let (tx_scrcpy_cmd, rx_scrcpy_cmd)=flume::bounded::<Packet>(5);
    //cmd scrcpy>srv channel
    let (tx_scrcpy_srv_cmd, rx_scrcpy_srv_cmd)=flume::bounded::<Packet>(5);
    let md_connected = Arc::new(Notify::new());
    let mut tsk_adb;
    tsk_adb = tokio_uring::spawn(scrcpy::tsk_adb_scrcpy(
        tx_scrcpy,
        rx_scrcpy_cmd,
        tx_scrcpy_srv_cmd,
        md_connected.clone(),
        shared_config.clone(),
    ));
    loop {
        //drain scrcpy commands?
        //while let Ok(msg) = rx_scrcpy_srv_cmd.clone().try_recv() {
        //}
        // reload new config
        let config = config.read().await.clone();
        let cfg2=cfg_clone.clone();
        // generate Durations from configured seconds
        let stats_interval = {
            if config.stats_interval == 0 {
                None
            } else {
                Some(Duration::from_secs(config.stats_interval.into()))
            }
        };
        debug!("{}: Waiting on ADB device to be connected", NAME);
        md_connected.notified().await;
        let read_timeout = Duration::from_secs(config.timeout_secs.into());

        let mut hu_tcp = None;
        let mut hu_usb = None;
        let mut usb = None;

        if config.dhu {
            //info!("{} 🛰️ DHU TCP server: bind to local address",NAME);
            //dhu_listener = Some(TcpListener::bind(bind_addr).unwrap());
            debug!("{} 🛰️ DHU TCP server: listening for `Desktop Head Unit` connection...",NAME);
            if let Ok(s) = tcp_wait_for_hu_connection(& dhu_listener.as_mut().unwrap()).await {
                hu_tcp = Some(s);
            } else {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        } else {
            debug!("{} 🛰️ Waiting for `Head Unit` connection on USB...",NAME);
            usb = Some(UsbGadgetState::new(false, config.udc.clone()));
            if let Some(ref mut usb) = usb {
                if let Err(e) = usb.init() {
                    error!("{} 🔌 USB init error: {}", NAME, e);
                }
            }
            enable_usb_if_present(&mut usb).await;
            if let Ok(_)=usb_wait_for_hu_connection(config.hu_detect_timeout_secs as u64).await
            {
                debug!("{} 📂 Opening USB accessory device: <u>{}</u>",NAME, USB_ACCESSORY_PATH);
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(false)
                    .open(USB_ACCESSORY_PATH)
                    .await
                {
                    Ok(s) => hu_usb = Some(s),
                    Err(e) => {
                        error!("{} 🔴 Error opening USB accessory: {}", NAME, e);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let _ = need_restart.send(None);//restart usb detection
                        continue;//we can't break the loop because we can't recover ADB task
                    }
                }
            }
            else
            {
                error!("{} 🔴 Timeout waiting USB accessory", NAME);
                let _ = need_restart.send(None);//restart usb detection
                continue;//we can't break the loop because we can't recover ADB task
            }

        }

        info!("{} ♾️ Starting to proxy data between HU and MD...", NAME);
        let started = Instant::now();

        // `read` and `write` take owned buffers (more on that later), and
        // there's no "per-socket" buffer, so they actually take `&self`.
        // which means we don't need to split them into a read half and a
        // write half like we'd normally do with "regular tokio". Instead,
        // we can send a reference-counted version of it. also, since a
        // tokio-uring runtime is single-threaded, we can use `Rc` instead of
        // `Arc`.
        let stats_w_bytes = Arc::new(AtomicUsize::new(0));
        let stats_r_bytes = Arc::new(AtomicUsize::new(0));
        // mpsc channels:
        let (txr_hu, rxr_hu):       (Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
        let (tx_srv, rx_srv):   (Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
        let (txr_srv, rxr_srv): (Sender<Packet>, Receiver<Packet>) = mpsc::channel(20);
        //let tx_srv_cloned=tx_srv.clone();
        let mut tsk_ch_manager;
        let mut tsk_hu_read;
        let mut tsk_packet_proxy;
        // these will be used for cleanup
        //let mut hu_tcp_stream = None;


        // selecting I/O device for reading and writing
        // and creating desired objects for proxy functions
        let hu_r;
        let hu_w;

        // HU transfer device
        if let Some(hu) = hu_usb {
            // HU connected directly via USB
            let hu = Arc::new(hu);
            hu_r = IoDevice::EndpointIo(hu.clone());
            hu_w = IoDevice::EndpointIo(hu.clone());
        } else {
            // Head Unit Emulator via TCP
            let hu = Arc::new(hu_tcp.unwrap());
            hu_r = IoDevice::TcpStreamIo(hu.clone());
            hu_w = IoDevice::TcpStreamIo(hu.clone());
            //hu_tcp_stream = Some(hu.clone());
        }

        // dedicated reading threads:
        tsk_hu_read = tokio_uring::spawn(endpoint_reader(hu_r, txr_hu));

        //service packet proxy
        let pp=PacketProxy::new( stats_r_bytes.clone(), stats_w_bytes.clone(), hex_requested, cfg.ignore_media_ack);
        tsk_packet_proxy=pp.start(hu_w, rxr_hu, rxr_srv, tx_srv, rx_scrcpy.clone());
        //tsk_packet_proxy = tokio_uring::spawn(packet_tls_proxy(hu_w, rxr_hu, rxr_srv, tx_srv, rx_scrcpy.clone(), stats_r_bytes.clone(), stats_w_bytes.clone(), hex_requested));
        //tsk_packet_proxy=pp.start(hu_w, rxr_hu, rxr_srv, tx_srv, rx_scrcpy.clone(),tx_scrcpy_cmd.clone());
        // main processing threads:
        tsk_ch_manager = tokio_uring::spawn(ch_proxy(
            rx_srv,
            txr_srv,
            tx_scrcpy_cmd.clone(),
            rx_scrcpy_srv_cmd.clone(),
            cfg2,
        ));

        // Thread for monitoring transfer
        let mut tsk_monitor = tokio::spawn(transfer_monitor(
            stats_interval,
            stats_w_bytes,
            stats_r_bytes,
            read_timeout,
            shared_config.clone(),
        ));

        // Wait here and Stop as soon as one of them errors
        let res = tokio::try_join!(
            flatten(&mut tsk_hu_read, "tsk_hu_read".into()),
            flatten(&mut tsk_ch_manager, "tsk_ch_manager".into()),
            flatten(&mut tsk_monitor,"tsk_monitor".into()),
            flatten(&mut tsk_packet_proxy,"tsk_pkt_proxy".into()),
        );

        if let Err(e) = res {
            error!("{} 🔴 Connection error: {}", NAME, e);
        }

        //Stop SCRCPY task as well
        let mut payload: Vec<u8>=Vec::new();
        payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
        payload.extend_from_slice(&(CustomCommand::CANCEL as u16).to_be_bytes());
        let pkt_rsp = Packet {
            channel: 0,
            flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = tx_scrcpy_cmd.send_async(pkt_rsp).await{
            error!( "io_uring.io_loop() send error");
        };
        //Stop HU connection as well
        /*let mut bye_req = ByeByeRequest::new();
        bye_req.set_reason(ByeByeReason::USER_SELECTION);
        let mut payload: Vec<u8>=Vec::new();
        payload.extend_from_slice(&(ControlMessageType::MESSAGE_BYEBYE_REQUEST as u16).to_be_bytes());
        payload.extend_from_slice(&bye_req.write_to_bytes().expect("serialization failed"));
        let pkt_rsp = Packet {
            channel: 0,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = tx_srv_cloned.send(pkt_rsp).await{
            error!( "io_uring.io_loop() send error");
        };*/

        // Make sure the reference count drops to zero and the socket is
        // freed by aborting both tasks (which both hold a `Rc<TcpStream>`
        // for each direction)
        tsk_packet_proxy.abort();
        tsk_hu_read.abort();
        tsk_ch_manager.abort();
        tsk_monitor.abort();

        // make sure TCP connections are closed before next connection attempts
        /*if let Some(stream) = hu_tcp_stream {
            info!("{} 🛰️ DHU TCP server: closing client connection...", NAME );
            //let _ = stream.shutdown(std::net::Shutdown::Both);
            drop(stream);

        }*/

        // set webserver context EV stuff to None
        let mut tx_lock = tx.lock().await;
        *tx_lock = None;


        info!("{} ⌛ session time: {}", NAME, format_duration(started.elapsed()).to_string());
        // obtain action for passing it to broadcast sender
        let action = shared_config.read().await.action_requested.clone();
        // stream(s) closed, notify main loop to restart
        let _ = need_restart.send(action);
    }
}
/// IO Loop for PassTrough mode only
pub async fn io_loop_pt(
    need_restart: BroadcastSender<Option<Action>>,
    config: SharedConfig,
    tx: Arc<Mutex<Option<Sender<Packet>>>>,
) -> Result<()> {
    let shared_config = config.clone();
    #[allow(unused_variables)]


    let cfg = config.read().await.clone();
    let hex_requested = cfg.hexdump_level;
    // prepare/bind needed TCP listeners
    let mut dhu_listener=None;
    let bind_addr = format!("0.0.0.0:{}", TCP_DHU_PORT).parse().unwrap();
    info!("{} 🛰️ Starting TCP server for DHU...", NAME);
    dhu_listener = Some(TcpListener::bind(bind_addr).unwrap());
    info!("{} 🛰️ DHU TCP server bound to: <u>{}</u>", NAME, bind_addr);
    let bind_addr = format!("0.0.0.0:{}", TCP_MD_SERVER_PORT).parse().unwrap();
    let mut md_listener = Some(TcpListener::bind(bind_addr).unwrap());
    let mut client_mac: Option<MacAddress> = None;
    let mut md_tcp = None;
    let mut bt_stopped=false;

    loop {

        // generate Durations from configured seconds
        let stats_interval = {
            if cfg.stats_interval == 0 {
                None
            } else {
                Some(Duration::from_secs(cfg.stats_interval.into()))
            }
        };
        debug!("{}: Bluetooth init", NAME);
        // initial bluetooth setup
        let mut bluetooth;
        loop {
            match bluetooth::init(cfg.btalias.clone(), cfg.advertise, cfg.dongle_mode).await {
                Ok(result) => {
                    bluetooth = result;
                    break;
                }
                Err(e) => {
                    error!("{} Fatal error in Bluetooth setup: {}", NAME, e);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
        debug!("{}: Waiting bluetooth handshake", NAME);
        let wifi_conf = {
            if !cfg.wired.is_some() {
                Some(init_wifi_config(&cfg))
            } else {
                None
            }
        };
        // bluetooth handshake
        if let Err(e) = bluetooth
            .aa_handshake(
                cfg.connect.clone(),
                wifi_conf.clone().unwrap(),
                Duration::from_secs(cfg.bt_timeout_secs.into()),
                bt_stopped,
                cfg.bt_poweroff,
            )
            .await
        {
            error!("{} bluetooth AA handshake error: {}", NAME, e);
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        }
        bt_stopped=true;
        debug!("{}: Waiting on MD to be connected over TCP", NAME);
        if let Ok((s, ip)) = tcp_wait_for_md_connection(&mut md_listener.as_mut().unwrap()).await {
            md_tcp = Some(s);
            // Get MAC address of the connected client for later disassociation
            client_mac = mac_from_ipv4(ip).await.unwrap_or(None);
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        }
        // these will be used for cleanup
        let mut md_tcp_stream = None;
        let mut hu_tcp_stream = None;
        // selecting I/O device for reading and writing
        // and creating desired objects for proxy functions
        // MD using TCP stream (wireless)
        let md = Arc::new(md_tcp.unwrap());
        let md_r = IoDevice::EndpointIo(md.clone());
        let md_w = IoDevice::EndpointIo(md.clone());
        md_tcp_stream = Some(md.clone());

        let read_timeout = Duration::from_secs(cfg.timeout_secs.into());

        let mut hu_tcp = None;
        let mut hu_usb = None;
        let mut usb = None;

        if cfg.dhu {
            //info!("{} 🛰️ DHU TCP server: bind to local address",NAME);
            //dhu_listener = Some(TcpListener::bind(bind_addr).unwrap());
            debug!("{} 🛰️ DHU TCP server: listening for `Desktop Head Unit` connection...",NAME);
            if let Ok(s) = tcp_wait_for_hu_connection(& dhu_listener.as_mut().unwrap()).await {
                hu_tcp = Some(s);
            } else {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        } else {
            debug!("{} 🛰️ Waiting for `Head Unit` connection on USB...",NAME);
            usb = Some(UsbGadgetState::new(false, cfg.udc.clone()));
            if let Some(ref mut usb) = usb {
                if let Err(e) = usb.init() {
                    error!("{} 🔌 USB init error: {}", NAME, e);
                }
            }
            enable_usb_if_present(&mut usb).await;
            if let Ok(_)=usb_wait_for_hu_connection(cfg.hu_detect_timeout_secs as u64).await
            {
                debug!("{} 📂 Opening USB accessory device: <u>{}</u>",NAME, USB_ACCESSORY_PATH);
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(false)
                    .open(USB_ACCESSORY_PATH)
                    .await
                {
                    Ok(s) => hu_usb = Some(s),
                    Err(e) => {
                        error!("{} 🔴 Error opening USB accessory: {}", NAME, e);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let _ = need_restart.send(None);//restart usb detection
                        continue;//we can't break the loop because we can't recover ADB task
                    }
                }
            }
            else
            {
                error!("{} 🔴 Timeout waiting USB accessory", NAME);
                let _ = need_restart.send(None);//restart usb detection
                continue;//we can't break the loop because we can't recover ADB task
            }

        }

        info!("{} ♾️ Starting to proxy data between HU and MD...", NAME);
        let started = Instant::now();

        // `read` and `write` take owned buffers (more on that later), and
        // there's no "per-socket" buffer, so they actually take `&self`.
        // which means we don't need to split them into a read half and a
        // write half like we'd normally do with "regular tokio". Instead,
        // we can send a reference-counted version of it. also, since a
        // tokio-uring runtime is single-threaded, we can use `Rc` instead of
        // `Arc`.
        let stats_w_bytes = Arc::new(AtomicUsize::new(0));
        let stats_r_bytes = Arc::new(AtomicUsize::new(0));
        // mpsc channels:
        let (tx_hu, rx_hu):       (Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
        let (tx_md, rx_md):       (Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);

        let mut tsk_hu_read;
        let mut tsk_md_read;


        // selecting I/O device for reading and writing
        // and creating desired objects for proxy functions
        let hu_r;
        let hu_w;

        // HU transfer device
        if let Some(hu) = hu_usb {
            // HU connected directly via USB
            let hu = Arc::new(hu);
            hu_r = IoDevice::EndpointIo(hu.clone());
            hu_w = IoDevice::EndpointIo(hu.clone());
        } else {
            // Head Unit Emulator via TCP
            let hu = Arc::new(hu_tcp.unwrap());
            hu_r = IoDevice::TcpStreamIo(hu.clone());
            hu_w = IoDevice::TcpStreamIo(hu.clone());
            hu_tcp_stream = Some(hu.clone());
        }

        // dedicated reading threads:
        tsk_hu_read = tokio_uring::spawn(endpoint_reader(hu_r, tx_hu));
        tsk_md_read = tokio_uring::spawn(endpoint_reader(md_r, tx_md));

        //packet proxy
        let mut tsk_packet_proxy = tokio_uring::spawn(packet_proxy_pt(
            rx_hu, hu_w, rx_md, md_w,
            stats_r_bytes.clone(),
            stats_w_bytes.clone(),
        ));


        // Thread for monitoring transfer
        let mut tsk_monitor = tokio::spawn(transfer_monitor(
            stats_interval,
            stats_w_bytes,
            stats_r_bytes,
            read_timeout,
            shared_config.clone(),
        ));

        // Wait here and Stop as soon as one of them errors
        let res = tokio::try_join!(
            flatten(&mut tsk_hu_read, "tsk_hu_read".into()),
            flatten(&mut tsk_md_read, "tsk_md_read".into()),
            flatten(&mut tsk_monitor,"tsk_monitor".into()),
            flatten(&mut tsk_packet_proxy,"tsk_pkt_proxy".into()),
        );

        if let Err(e) = res {
            error!("{} 🔴 Connection error: {}", NAME, e);
        }
        //switch back to default to let HU switch again to accessory mode
        if let Some(ref mut usb) = usb {
            usb.switch_to_default().await;
        }

        // Do not await these handles here: `try_join!(flatten(&mut ...))` above
        // may already have polled one of them to completion, and polling a
        // `JoinHandle` after completion panics. Aborting is enough to request
        // cancellation of the remaining tasks before we drop the handles and
        // shut down the TCP streams below.

        // make sure TCP connections are closed before next connection attempts
        if let Some(stream) = md_tcp_stream {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(stream) = hu_tcp_stream {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }

        // Disassociate a client from the WiFi AP.
        // Mainly needed when a button was used to switch to the next device,
        // or when the stop_on_disconnect option was used.
        // Otherwise, the WiFi/AA connection remains hanging and the phone
        // won't switch back to the regular WiFi.
        if let Some(mac) = client_mac {
            info!("{} disassociating WiFi client: {}", NAME, mac);

            let _ = Command::new("/usr/bin/hostapd_cli")
                .args(&["disassociate", &mac.to_string()])
                .spawn();
        }

        // Make sure the reference count drops to zero and the socket is
        // freed by aborting both tasks (which both hold a `Rc<TcpStream>`
        // for each direction)
        tsk_packet_proxy.abort();
        tsk_hu_read.abort();
        tsk_md_read.abort();
        tsk_monitor.abort();

        // make sure TCP connections are closed before next connection attempts
        /*if let Some(stream) = hu_tcp_stream {
            info!("{} 🛰️ DHU TCP server: closing client connection...", NAME );
            //let _ = stream.shutdown(std::net::Shutdown::Both);
            drop(stream);

        }*/

        // set webserver context EV stuff to None
        let mut tx_lock = tx.lock().await;
        *tx_lock = None;


        info!("{} ⌛ session time: {}", NAME, format_duration(started.elapsed()).to_string());
        // obtain action for passing it to broadcast sender
        let action = shared_config.read().await.action_requested.clone();
        // stream(s) closed, notify main loop to restart
        let _ = need_restart.send(action);
    }
}