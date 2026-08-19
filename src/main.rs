//use aa_mirror_rs::bluetooth;
use aa_mirror_rs::config::SharedConfig;
use aa_mirror_rs::config::SharedConfigJson;
use aa_mirror_rs::config::WifiConfig;
use aa_mirror_rs::config::{Action, AppConfig};
use aa_mirror_rs::config::{DEFAULT_WLAN_ADDR, TCP_MD_SERVER_PORT};
use aa_mirror_rs::io_uring::{io_loop, io_loop_pt};
use aa_mirror_rs::led::{LedColor, LedManager, LedMode};
use aa_mirror_rs::channel_manager::Packet;
//use aa_mirror_rs::usb_gadget::uevent_listener;
use aa_mirror_rs::usb_gadget::UsbGadgetState;
use aa_mirror_rs::web;
use clap::Parser;
use humantime::format_duration;
use simplelog::*;
use std::os::unix::fs::PermissionsExt;
use std::panic;

use std::fs;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::{Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::runtime::Builder;
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender as BroadcastSender;
use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::time::Instant;

use std::net::SocketAddr;
use tokio::sync::RwLock;
use aa_mirror_rs::config_types::{AAMode, WiFiMode};

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// module name for logging engine
const NAME: &str = "<i><bright-black> main: </>";
const HOSTAPD_CONF_IN: &str = "/etc/hostapd.conf.in";
const WPA_SUPPLICANT_CONF_IN: &str = "/etc/wpa_supplicant.conf.in";
const HOSTAPD_CONF_OUT: &str = "/var/run/hostapd.conf";
const WPA_SUPPLICANT_TEMP_CONF_OUT: &str = "/var/run/wpa_supplicant.conf";
const WPA_SUPPLICANT_CONF_OUT: &str = "/etc/wpa_supplicant.conf";
const UMTPRD_CONF_IN: &str = "/etc/umtprd/umtprd.conf.in";
const UMTPRD_CONF_OUT: &str = "/var/run/umtprd.conf";
const GADGET_INIT_IN: &str = "/etc/S92usb_gadget.in";
const GADGET_INIT_OUT: &str = "/var/run/S92usb_gadget";
const REBOOT_CMD: &str = "/sbin/reboot";

/// AndroidAuto wired/wireless proxy
#[derive(Parser, Debug)]
#[clap(version, long_about = None, about = format!(
    "🛸 aa-mirror-rs, build: {}, git: {}-{}",
    env!("BUILD_DATE"),
    env!("GIT_DATE"),
    env!("GIT_HASH")
))]
struct Args {
    /// Config file path
    #[clap(
        short,
        long,
        value_parser,
        default_value = "/etc/aa-mirror-rs/config.toml"
    )]
    config: PathBuf,

    /// Generate system config and exit
    #[clap(short, long)]
    generate_system_config: bool,
}


fn logging_init(debug: bool, disable_console_debug: bool, log_path: &PathBuf) {
    let conf = ConfigBuilder::new()
        .set_time_format("%F, %H:%M:%S%.3f".to_string())
        .set_write_log_enable_colors(true)
        .build();

    let mut loggers = vec![];

    let requested_level = if debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    let console_logger: Box<dyn SharedLogger> = TermLogger::new(
        {
            if disable_console_debug {
                LevelFilter::Info
            } else {
                requested_level
            }
        },
        conf.clone(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    );
    loggers.push(console_logger);

    let mut logfile_error: Option<String> = None;
    let logfile = OpenOptions::new().create(true).append(true).open(&log_path);
    match logfile {
        Ok(logfile) => {
            loggers.push(WriteLogger::new(requested_level, conf, logfile));
        }
        Err(e) => {
            logfile_error = Some(format!(
                "Error creating/opening log file: {:?}: {:?}",
                log_path, e
            ));
        }
    }

    CombinedLogger::init(loggers).expect("Cannot initialize logging subsystem");
    if logfile_error.is_some() {
        error!("{} {}", NAME, logfile_error.unwrap());
        warn!("{} Will do console logging only...", NAME);
    }
}

/*async fn enable_usb_if_present(usb: &mut Option<UsbGadgetState>, accessory_started: Arc<Notify>) {
    if let Some(ref mut usb) = usb {
        usb.enable_default_and_wait_for_accessory(accessory_started)
            .await;
    }
}*/

async fn action_handler(config: &mut SharedConfig) {
    // check pending action
    let action = config.read().await.action_requested.clone();
    if let Some(action) = action {
        // check if we need to reboot
        if action == Action::Reboot {
            config.write().await.action_requested = None;
            info!("{} 🔁 Rebooting now!", NAME);
            let _ = Command::new(REBOOT_CMD).spawn();
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    }
}

async fn tokio_main(
    config: SharedConfig,
    config_json: SharedConfigJson,
    restart_tx: BroadcastSender<Option<Action>>,
    config_file: PathBuf,
    led_support: bool,

) -> Result<()> {
    //let accessory_started = Arc::new(Notify::new());
    let state = web::AppState {
        config: config.clone(),
        config_json: config_json.clone(),
        config_file: config_file.into(),
    };

    // LED support
    let mut led_manager = if led_support {
        Some(LedManager::new(100))
    } else {
        None
    };

    let mut cfg = config.read().await.clone();
    if let Some(bindaddr) = cfg.webserver.clone() {
        // preparing AppState and starting webserver
        /*let app = web::app(state.clone().into());

        match bindaddr.parse::<SocketAddr>() {
            Ok(addr) => {
                let server = hyper::Server::bind(&addr).serve(app.into_make_service());

                // run webserver in separate task
                tokio::spawn(async move {
                    if let Err(e) = server.await {
                        error!("{} webserver starting error: {}", NAME, e);
                    }
                });

                info!("{} webserver running at http://{addr}/", NAME);
            }
            Err(e) => {
                error!("{} webserver address/port parse: {}", NAME, e);
            }
        }*/
        let app = web::app(Arc::new(state));
        //start webserver in a dedicated Tokyo runtime
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create webserver runtime");

            rt.block_on(async move {
                let addr: SocketAddr = match bindaddr.parse() {
                    Ok(addr) => addr,
                    Err(e) => {
                        error!("{} webserver address/port parse: {}", NAME, e);
                        return;
                    }
                };

                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => listener,
                    Err(e) => {
                        error!("{} webserver bind error: {}", NAME, e);
                        return;
                    }
                };

                info!("{} webserver running at http://{addr}/", NAME);

                if let Err(e) = axum::serve(listener, app).await {
                    error!("{} webserver error: {}", NAME, e);
                }
            });
        });
    }

    // spawn a background task for reboot detection
    let mut config_cloned = config.clone();
    let _ = tokio::spawn(async move {
        loop {
            action_handler(&mut config_cloned).await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });

    

    // main connection loop
    let mut need_restart = restart_tx.subscribe();
    loop {
        if let Some(ref mut leds) = led_manager {
            leds.set_led(LedColor::Green, LedMode::Heartbeat).await;
        }
        /*if let Some(ref mut usb) = usb {
            if let Err(e) = usb.init() {
                error!("{} 🔌 USB init error: {}", NAME, e);
            }
        }*/

        //enable_usb_if_present(&mut usb, accessory_started.clone()).await;

        // inform via LED about successful connection
        if let Some(ref mut leds) = led_manager {
            leds.set_led(LedColor::Blue, LedMode::On).await;
        }
        info!("{} 📵 Init done, waiting for main app to finnish...",NAME);
        // wait for restart notification
        let _ = need_restart.recv().await;
        info!("{} 📵 Main app finished, trying again...",NAME);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // TODO: make proper main loop with cancellation
    }
}

/// Returns the SBC model string (currently supports only Raspberry Pi)
pub fn get_sbc_model() -> Result<String> {
    Ok(fs::read_to_string("/sys/firmware/devicetree/base/model")?
        .trim_end_matches(char::from(0))
        .trim()
        .to_string())
}

/// Returns the full device serial number from Device Tree
pub fn get_serial_number() -> Result<String> {
    Ok(
        fs::read_to_string("/sys/firmware/devicetree/base/serial-number")?
            .trim_end_matches(char::from(0))
            .trim()
            .to_string(),
    )
}

fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut output = template.to_string();
    for (key, value) in vars {
        let placeholder = format!("{{{{{}}}}}", key);
        output = output.replace(&placeholder, value);
    }
    output
}

fn generate_hostapd_conf(config: AppConfig) -> std::io::Result<()> {
    info!(
        "{} 🗃️ Generating config from input template: <bold><green>{}</>",
        NAME, HOSTAPD_CONF_IN
    );

    // Technically for IEEE802.11g we have to use g but AFAIK b is fine.
    let hostapd_mode = if config.band == "5" || config.band == "6" {
        "a"
    } else {
        "g"
    };

    let template = fs::read_to_string(HOSTAPD_CONF_IN)?;

    // Eventually: For 6 GHz, we will need more options like opclass.
    let rendered = render_template(
        &template,
        &[
            ("HW_MODE", hostapd_mode),
            ("BE_MODE", if config.wifi_version >= 7 { "1" } else { "0" }),
            ("AX_MODE", if config.wifi_version >= 6 { "1" } else { "0" }),
            ("AC_MODE", if config.wifi_version >= 5 { "1" } else { "0" }),
            ("N_MODE", if config.wifi_version >= 4 { "1" } else { "0" }),
            ("COUNTRY_CODE", &config.country_code),
            ("CHANNEL", &config.channel.to_string()),
            ("SSID", &config.ssid),
            ("WPA_PASSPHRASE", &config.wpa_passphrase),
        ],
    );

    info!(
        "{} 💾 Saving generated file as: <bold><green>{}</>",
        NAME, HOSTAPD_CONF_OUT
    );
    fs::write(HOSTAPD_CONF_OUT, rendered)
}

fn generate_wpa_supplicant_conf(config: AppConfig) -> std::io::Result<()> {
    info!(
        "{} 🗃️ Generating config from input template: <bold><green>{}</>",
        NAME, WPA_SUPPLICANT_CONF_IN
    );


    let template = fs::read_to_string(WPA_SUPPLICANT_CONF_IN)?;

    /*let mut freq_list="2412 2417 2422 2427 2432 2437 2442 2447 2452 2457 2462 2467 2472 2484";
    if config.band == "5"
    {
        freq_list="5180 5200 5220 5240 5260 5280 5300 5320 5500 5520 5540 5560 5580 5600 5620 5640 5660 5680 5700 5720";
    }*/
    // Eventually: For 6 GHz, we will need more options like opclass.
    let rendered = render_template(
        &template,
        &[
            ("STA_SSID", &format!("\"{}\"", config.ap_ssid)),
            ("STA_PWD",  &format!("\"{}\"", config.ap_psw)),
        ],
    );

    info!(
        "{} 💾 Saving generated file as: <bold><green>{}</>",
        NAME, WPA_SUPPLICANT_CONF_OUT
    );
    fs::write(WPA_SUPPLICANT_TEMP_CONF_OUT, rendered.clone()).expect("error writing config file");
    fs::write(WPA_SUPPLICANT_CONF_OUT, rendered)
}

fn is_hostapd_configured() -> bool {
    let command="hostapd";
    let file_path="/etc/network/interfaces";
    let content = match fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with('#') {
            continue;
        }

        if trimmed.contains(command) {
            return true;
        }
    }

    false
}

fn generate_usb_strings(input: &str, output: &str) -> std::io::Result<()> {
    info!(
        "{} 🗃️ Generating config from input template: <bold><green>{}</>",
        NAME, input
    );

    let template = fs::read_to_string(input)?;

    let rendered = render_template(
        &template,
        &[
            (
                "MODEL",
                &get_sbc_model().map_or(String::new(), |model| format!(" ({})", model)),
            ),
            (
                "SERIAL",
                &get_serial_number().unwrap_or("0123456".to_string()),
            ),
            (
                "FIRMWARE_VER",
                &format!(
                    "{}, git: {}-{}",
                    env!("BUILD_DATE"),
                    env!("GIT_DATE"),
                    env!("GIT_HASH")
                ),
            ),
        ],
    );

    info!(
        "{} 💾 Saving generated file as: <bold><green>{}</>",
        NAME, output
    );
    fs::write(output, rendered)
}

fn main() -> Result<()> {

    let started = Instant::now();
    // CLI arguments
    let args = Args::parse();

    // parse config
    let config = match AppConfig::load(args.config.clone()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!(
                "Failed to start aa-mirror-rs due to invalid configuration in: {}.  Error:\n{}",
                args.config.display(),
                e
            );
            std::process::exit(1);
        }
    };
    if config.debug
    {
        panic::set_hook(Box::new(|info| {
            eprintln!("panic occurred: {:?}", info);
            error!("panic occurred: {:?}", info);
        }));
    }
    let config_json = AppConfig::load_config_json().expect("Invalid embedded config.json");

    logging_init(config.debug, config.disable_console_debug, &config.logfile);
    info!(
        "🛸 <b><blue>aa-mirror-rs</> is starting, build: {}, git: {}-{}",
        env!("BUILD_DATE"),
        env!("GIT_DATE"),
        env!("GIT_HASH")
    );

    // generate system configs from template and exit
    if args.generate_system_config {
        if config.aa_mode == AAMode::PassThrough
        {
            generate_hostapd_conf(config).expect("error generating config from template");
        }
        else
        {
            generate_wpa_supplicant_conf(config).expect("error generating config from template");
        }


        generate_usb_strings(UMTPRD_CONF_IN, UMTPRD_CONF_OUT)
            .expect("error generating config from template");

        generate_usb_strings(GADGET_INIT_IN, GADGET_INIT_OUT)
            .expect("error generating config from template");
        // make a script executable
        info!(
            "{} 🚀 Making script executable: <bold><green>{}</>",
            NAME, GADGET_INIT_OUT
        );
        let mut perms = fs::metadata(GADGET_INIT_OUT)?.permissions();
        perms.set_mode(0o755); // rwxr-xr-x
        fs::set_permissions(GADGET_INIT_OUT, perms)?;

        return Ok(());
    }

    // show SBC model
    let mut led_support = false;
    if let Ok(model) = get_sbc_model() {
        info!("{} 📟 host device: <bold><blue>{}</>", NAME, model);
        if model == "AAWireless 2B" {
            led_support = true;
        }
    }

    // check and display config
    if args.config.exists() {
        info!(
            "{} ⚙️ config loaded from file: {}",
            NAME,
            args.config.display()
        );
    } else {
        warn!(
            "{} ⚙️ config file: {} doesn't exist, defaults used",
            NAME,
            args.config.display()
        );
    }
    debug!("{} ⚙️ startup configuration: {:#?}", NAME, config);

    if config.enable_ftp
    {
        // Spawn vsftpd as a background process
        let _child = Command::new("vsftpd")
            .arg("/etc/vsftpd.conf")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;                     // just spawn, don't await
        debug!("FTP server started with PID: {:?}", _child.id());
    }

    if let Some(ref wired) = config.wired {
        info!(
            "{} 🔌 enabled wired USB connection with {:04X?}",
            NAME, wired
        );
    }
    info!(
        "{} 📜 Log file path: <b><green>{}</>",
        NAME,
        config.logfile.display()
    );
    let cfg = config.clone();
    // notify for syncing threads
    let (restart_tx, _) = broadcast::channel(1);
    let config_lck = Arc::new(RwLock::new(config));
    let config_json = Arc::new(RwLock::new(config_json));
    let config_lck_cloned = config_lck.clone();

    // build and spawn main tokio runtime
    let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
    let restart_tx_cloned = restart_tx.clone();

    runtime.spawn(async move {
        tokio_main(
            config_lck_cloned,
            config_json.clone(),
            restart_tx_cloned,
            args.config.clone(),
            led_support,
        )
        .await
    });

    // start tokio_uring runtime simultaneously
    if cfg.aa_mode == AAMode::Mirror
    {
        let _ = tokio_uring::start(io_loop(
            restart_tx,
            config_lck,
        ));
    }
    else
    {
        let _ = tokio_uring::start(io_loop_pt(
            restart_tx,
            config_lck,
        ));
    }


    info!(
        "🚩 aa-mirror-rs terminated, running time: {}",
        format_duration(started.elapsed()).to_string()
    );

    Ok(())
}
