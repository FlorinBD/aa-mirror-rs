use bytesize::ByteSize;
use humantime::format_duration;
use simplelog::*;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream as TokioTcpStream;
use tokio::sync::broadcast::Sender as BroadcastSender;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_uring::buf::BoundedBuf;
use tokio_uring::buf::BoundedBufMut;
use tokio_uring::fs::File;
use tokio_uring::fs::OpenOptions;
use tokio_uring::net::TcpListener;
use tokio_uring::net::TcpStream;
use tokio_uring::BufResult;
use tokio_uring::UnsubmittedWrite;

// module name for logging engine
const NAME: &str = "<i><bright-black> io_uring: </>";

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const USB_ACCESSORY_PATH: &str = "/dev/usb_accessory";
pub const BUFFER_LEN: usize = 16 * 1024;
pub const TCP_CLIENT_TIMEOUT: Duration = Duration::new(30, 0);
const COMP_APP_TCP_PORT: u16 = 9999;

use crate::config::{Action, SharedConfig};
use crate::config::{TCP_DHU_PORT, TCP_SERVER_PORT};
use crate::ev::spawn_ev_client_task;
use crate::ev::EvTaskCommand;
use crate::channel_manager::{endpoint_reader,ch_proxy, packet_tls_proxy};
use crate::channel_manager::Packet;
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
    UsbReader(Rc<RefCell<UsbStreamRead>>, PhantomData<A>),
    UsbWriter(Rc<RefCell<UsbStreamWrite>>, PhantomData<A>),
    EndpointIo(Rc<A>),
    TcpStreamIo(Rc<TcpStream>),
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
                return Err("unexpected transfer stall".into());
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

async fn flatten<T>(handle: &mut JoinHandle<Result<T>>, dbg_info:String) -> Result<T> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(er) => Err(format!("task handling failed for {}", dbg_info).into()),
    }
}

async fn tcp_bridge(remote_addr: &str, local_addr: &str) {
    loop {
        debug!(
            "{} tcp_bridge: before connect, local={} remote={}",
            NAME, local_addr, remote_addr
        );
        match TokioTcpStream::connect(remote_addr).await {
            Ok(mut remote) => {
                debug!(
                    "{} tcp_bridge: remote side connected: ({})",
                    NAME, remote_addr
                );
                match TokioTcpStream::connect(local_addr).await {
                    Ok(mut local) => {
                        debug!(
                            "{} tcp_bridge: local side connected: ({})",
                            NAME, local_addr
                        );
                        info!("{} Connected to companion app TCP server ({}), starting bidirectional transfer...", NAME, local_addr);
                        match copy_bidirectional(&mut remote, &mut local).await {
                            Ok((from_remote, from_local)) => {
                                debug!(
                                    "{} tcp_bridge: Connection closed: remote->local={} local->remote={}",
                                    NAME, from_remote, from_local
                                );
                            }
                            Err(e) => {
                                error!("{} Error during bidirectional copy: {}", NAME, e);
                            }
                        }
                    }
                    Err(e) => {
                        debug!(
                            "{} tcp_bridge: Failed to connect to local server {}: {}",
                            NAME, local_addr, e
                        );
                    }
                }
            }
            Err(e) => {
                debug!(
                    "{} tcp_bridge: Failed to connect to remote server {}: {}",
                    NAME, remote_addr, e
                );
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Asynchronously wait for an inbound TCP connection
/// returning TcpStream of first client connected
async fn tcp_wait_for_connection(listener: &mut TcpListener) -> Result<TcpStream> {
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

    // disable Nagle algorithm, so segments are always sent as soon as possible,
    // even if there is only a small amount of data
    stream.set_nodelay(true)?;
    Ok(stream)
}

pub async fn io_loop(
    need_restart: BroadcastSender<Option<Action>>,
    tcp_start: Arc<Notify>,
    config: SharedConfig,
    tx: Arc<Mutex<Option<Sender<Packet>>>>,
    sensor_channel: Arc<Mutex<Option<u8>>>,
    md_connected: Arc<AtomicBool>,
) -> Result<()> {
    let shared_config = config.clone();
    #[allow(unused_variables)]
    //let (client_handler, ev_tx) = spawn_ev_client_task().await;

    // prepare/bind needed TCP listeners
    /*info!("{} 🛰️ Starting TCP server for MD...", NAME);
    let bind_addr = format!("0.0.0.0:{}", TCP_SERVER_PORT).parse().unwrap();
    let mut md_listener = Some(TcpListener::bind(bind_addr).unwrap());
    info!("{} 🛰️ MD TCP server bound to: <u>{}</u>", NAME, bind_addr);*/
    info!("{} 🛰️ Starting TCP server for DHU...", NAME);
    let bind_addr = format!("0.0.0.0:{}", TCP_DHU_PORT).parse().unwrap();
    //let mut dhu_listener = Some(TcpListener::bind(bind_addr).unwrap());
    info!("{} 🛰️ DHU TCP server bound to: <u>{}</u>", NAME, bind_addr);

    loop {
        // reload new config
        let config = config.read().await.clone();

        // generate Durations from configured seconds
        let stats_interval = {
            if config.stats_interval == 0 {
                None
            } else {
                Some(Duration::from_secs(config.stats_interval.into()))
            }
        };
        let read_timeout = Duration::from_secs(config.timeout_secs.into());

        let mut hu_tcp = None;
        let mut hu_usb = None;
        let mut dhu_listener = None;
        if config.dhu {
            info!("{} 🛰️ DHU TCP server: bind to local address",NAME);
            dhu_listener = Some(TcpListener::bind(bind_addr).unwrap());
            info!("{} 🛰️ DHU TCP server: listening for `Desktop Head Unit` connection...",NAME);
            if let Ok(s) = tcp_wait_for_connection(&mut dhu_listener.as_mut().unwrap()).await {
                hu_tcp = Some(s);
            } else {
                // notify main loop to restart
                let _ = need_restart.send(None);
                continue;
            }
        } else {
            info!(
                "{} 📂 Opening USB accessory device: <u>{}</u>",
                NAME, USB_ACCESSORY_PATH
            );
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
                    // notify main loop to restart
                    let _ = need_restart.send(None);
                    continue;
                }
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

        let mut tsk_ch_manager;
        let mut tsk_hu_read;
        let mut tsk_packet_proxy;
        // these will be used for cleanup
        let mut hu_tcp_stream = None;

        // mpsc channels:
        let (txr_hu, rxr_hu):       (Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
        let (tx_srv, rx_srv):   (Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
        let (txr_srv, rxr_srv): (Sender<Packet>, Receiver<Packet>) = mpsc::channel(20);
        // selecting I/O device for reading and writing
        // and creating desired objects for proxy functions
        let hu_r;
        let hu_w;

        // HU transfer device
        if let Some(hu) = hu_usb {
            // HU connected directly via USB
            let hu = Rc::new(hu);
            hu_r = IoDevice::EndpointIo(hu.clone());
            hu_w = IoDevice::EndpointIo(hu.clone());
        } else {
            // Head Unit Emulator via TCP
            let hu = Rc::new(hu_tcp.unwrap());
            hu_r = IoDevice::TcpStreamIo(hu.clone());
            hu_w = IoDevice::TcpStreamIo(hu.clone());
            hu_tcp_stream = Some(hu.clone());
        }
        let cfg = shared_config.read().await.clone();
        let hex_requested = cfg.hexdump_level;
        //service packet proxy
        tsk_packet_proxy = tokio_uring::spawn(packet_tls_proxy(hu_w, rxr_hu, rxr_srv, tx_srv, stats_r_bytes.clone(), stats_w_bytes.clone(), hex_requested));

        // dedicated reading threads:
        tsk_hu_read = tokio_uring::spawn(endpoint_reader(hu_r, txr_hu));
        
        // main processing threads:
        tsk_ch_manager = tokio_uring::spawn(ch_proxy(
            rx_srv,
            txr_srv,
        ));
        
        // Thread for monitoring transfer
        let mut tsk_monitor = tokio::spawn(transfer_monitor(
            stats_interval,
            stats_w_bytes,
            stats_r_bytes,
            read_timeout,
            shared_config.clone(),
        ));

        // Stop as soon as one of them errors
        let res = tokio::try_join!(
            flatten(&mut tsk_hu_read, "tsk_hu_read".into()),
            flatten(&mut tsk_ch_manager, "tsk_ch_manager".into()),
            flatten(&mut tsk_monitor,"tsk_monitor".into()),
            flatten(&mut tsk_packet_proxy,"tsk_pkt_proxy".into())
        );

        if let Err(e) = res {
            error!("{} 🔴 Connection error: {}", NAME, e);
        }

        // Make sure the reference count drops to zero and the socket is
        // freed by aborting both tasks (which both hold a `Rc<TcpStream>`
        // for each direction)
        tsk_packet_proxy.abort();
        tsk_hu_read.abort();
        tsk_ch_manager.abort();
        tsk_monitor.abort();


        // make sure TCP connections are closed before next connection attempts
        if let Some(stream) = hu_tcp_stream {
            info!("{} 🛰️ DHU TCP server: closing client connection...", NAME );
            let _ = stream.shutdown(std::net::Shutdown::Both);

        }

        // set webserver context EV stuff to None
        let mut tx_lock = tx.lock().await;
        *tx_lock = None;
        let mut sc_lock = sensor_channel.lock().await;
        *sc_lock = None;

        info!(
            "{} ⌛ session time: {}",
            NAME,
            format_duration(started.elapsed()).to_string()
        );
        // obtain action for passing it to broadcast sender
        let action = shared_config.read().await.action_requested.clone();
        // stream(s) closed, notify main loop to restart
        let _ = need_restart.send(action);
    }
}
