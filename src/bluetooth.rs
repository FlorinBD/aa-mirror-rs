use std::collections::BTreeSet;
use crate::btle;
use crate::config::Action;
use crate::config::WifiConfig;
use crate::config::IDENTITY_NAME;
use crate::config_types::MACAddressList;
use crate::web::AppState;
use anyhow::anyhow;
use backon::{ExponentialBuilder, Retryable};
use bluer::{rfcomm::{Profile, ProfileHandle, Role, Stream}, Adapter, Address, Device, Session, Uuid, UuidExt};
use futures::StreamExt;
use simplelog::*;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use bluer::adv::Advertisement;
use bluer::gatt::CharacteristicWriter;
use bluer::gatt::local::{characteristic_control, Application, Characteristic, CharacteristicControlEvent, CharacteristicNotify, CharacteristicNotifyMethod, CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod, Descriptor, DescriptorRead, Service};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;
use tokio::sync::broadcast::Sender as BroadcastSender;
use tokio::sync::{Mutex, Notify};
use tokio::time::timeout;

include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use protobuf::Message;
use WifiInfoResponse::AccessPointType;
use WifiInfoResponse::SecurityMode;
const HEADER_LEN: usize = 4;
const STAGES: u8 = 5;
const ATTEMPTS: usize = 3;

// module name for logging engine
const NAME: &str = "<i><bright-black> bluetooth: </>";

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub const AAWG_PROFILE_UUID: Uuid = Uuid::from_u128(0x4de17a0052cb11e6bdf40800200c9a66);
pub const BTLE_PROFILE_UUID: Uuid = Uuid::from_u128(0x9b3f6c10a4d2418ea2b90700300de8f4);
const HSP_HS_UUID: Uuid = Uuid::from_u128(0x0000110800001000800000805f9b34fb);
const HSP_AG_UUID: Uuid = Uuid::from_u128(0x0000111200001000800000805f9b34fb);
const AV_REMOTE_CONTROL_TARGET_UUID: Uuid = Uuid::from_u128(0x0000110c00001000800000805f9b34fb);
const AV_REMOTE_CONTROL_UUID: Uuid = Uuid::from_u128(0x00110e00001000800000805f9b34fb);
const AIS_PRIMARY_UUID: Uuid = Uuid::from_u128(0xe73e0001ef1b4e7482912e4f3164f3b5);

//used by HID controller
const HID_SERVICE: Uuid = Uuid::from_u128(0x0000181200001000800000805f9b34fb);
const HID_INFO_CHAR: Uuid = Uuid::from_u128(0x00002a4a00001000800000805f9b34fb);
const REPORT_MAP_CHAR: Uuid = Uuid::from_u128(0x00002a4b00001000800000805f9b34fb);
const REPORT_CHAR: Uuid = Uuid::from_u128(0x00002a4d00001000800000805f9b34fb);
const HID_CONTROL_POINT_CHAR: Uuid = Uuid::from_u128(0x00002a4c00001000800000805f9b34fb);
const PROTOCOL_MODE_CHAR: Uuid = Uuid::from_u128(0x00002a4e00001000800000805f9b34fb);

#[derive(Debug, Clone, PartialEq)]
#[repr(u16)]
#[allow(unused)]
enum MessageId {
    WifiStartRequest = 1,
    WifiInfoRequest = 2,
    WifiInfoResponse = 3,
    WifiVersionRequest = 4,
    WifiVersionResponse = 5,
    WifiConnectStatus = 6,
    WifiStartResponse = 7,
}

pub struct Bluetooth {
    adapter: Adapter,
    handle_aa: ProfileHandle,
    btle_handle: Option<bluer::gatt::local::ApplicationHandle>,
    adv_handle: Option<bluer::adv::AdvertisementHandle>,
    current_index: usize,
}

/// Builds the HID Report Descriptor with the touchscreen's logical X/Y max set to
/// `width - 1` / `height - 1`. Coordinates reported via `send_touch` must stay
/// within these bounds for the host to interpret them correctly.
fn build_report_descriptor(width: u16, height: u16) -> Vec<u8> {
    let x_max = width.saturating_sub(1);
    let y_max = height.saturating_sub(1);

    let mut d = vec![
        0x05, 0x0D,             // Usage Page (Digitizer)
        0x09, 0x04,             // Usage (Touch Screen)
        0xA1, 0x01,             // Collection (Application)

        0x85, 0x01,             //   Report ID (1)
        0x09, 0x22,             //   Usage (Finger)
        0xA1, 0x00,             //   Collection (Physical)

        // Tip Switch + In Range (2 bits) + 6 bits padding
        0x09, 0x42,             //     Usage (Tip Switch)
        0x09, 0x32,             //     Usage (In Range)
        0x15, 0x00,             //     Logical Min 0
        0x25, 0x01,             //     Logical Max 1
        0x75, 0x01,             //     Report Size 1
        0x95, 0x02,             //     Report Count 2
        0x81, 0x02,             //     Input (Data,Var,Abs)
        0x75, 0x06,             //     Report Size 6 (padding)
        0x95, 0x01,             //     Report Count 1
        0x81, 0x03,             //     Input (Const,Var,Abs)

        // Contact Identifier
        0x75, 0x08,             //     Report Size 8
        0x95, 0x01,             //     Report Count 1
        0x09, 0x51,             //     Usage (Contact Identifier)
        0x81, 0x02,             //     Input (Data,Var,Abs)

        // X
        0x05, 0x01,             //     Usage Page (Generic Desktop)
        0x09, 0x30,             //     Usage (X)
        0x75, 0x10,             //     Report Size 16
        0x95, 0x01,             //     Report Count 1
        0x15, 0x00,             //     Logical Min 0
        0x26,                   //     Logical Max (X)
    ];
    d.extend_from_slice(&x_max.to_le_bytes());
    d.extend_from_slice(&[
        0x81, 0x02,             //     Input (Data,Var,Abs)

        // Y
        0x09, 0x31,             //     Usage (Y)
        0x15, 0x00,             //     Logical Min 0
        0x26,                   //     Logical Max (Y)
    ]);
    d.extend_from_slice(&y_max.to_le_bytes());
    d.extend_from_slice(&[
        0x75, 0x10,             //     Report Size 16
        0x95, 0x01,             //     Report Count 1
        0x81, 0x02,             //     Input (Data,Var,Abs)

        0xC0,                   //   End Collection (Physical - Finger)

        // Contact Count (outside the finger collection, per spec)
        0x05, 0x0D,             //   Usage Page (Digitizer)
        0x09, 0x54,             //   Usage (Contact Count)
        0x15, 0x00,             //   Logical Min 0
        0x25, 0x01,             //   Logical Max 1 (single touch)
        0x75, 0x08,             //   Report Size 8
        0x95, 0x01,             //   Report Count 1
        0x81, 0x02,             //   Input (Data,Var,Abs)

        0xC0,                   // End Collection (Application)
    ]);

    d
}
fn build_report_descriptor_v2(width: u16, height: u16) -> Vec<u8> {
    let x_max = width.saturating_sub(1);
    let y_max = height.saturating_sub(1);

    let mut d = vec![
        // Digitizer / Touch Screen
        0x05, 0x0D,             // Usage Page (Digitizer)
        0x09, 0x04,             // Usage (Touch Screen)
        0xA1, 0x01,             // Collection (Application)

        0x85, 0x01,             // Report ID (1)

        // Finger
        0x09, 0x22,             // Usage (Finger)
        0xA1, 0x02,             // Collection (Logical)

        // Tip Switch
        0x09, 0x42,             // Usage (Tip Switch)
        0x15, 0x00,             // Logical Min 0
        0x25, 0x01,             // Logical Max 1
        0x75, 0x01,             // Report Size 1
        0x95, 0x01,             // Report Count 1
        0x81, 0x02,             // Input (Data,Var,Abs)

        // In Range
        0x09, 0x32,
        0x75, 0x01,
        0x95, 0x01,
        0x81, 0x02,

        // Confidence
        0x09, 0x47,
        0x75, 0x01,
        0x95, 0x01,
        0x81, 0x02,

        // Padding
        0x75, 0x05,
        0x95, 0x01,
        0x81, 0x03,

        // Contact ID
        0x09, 0x51,
        0x75, 0x08,
        0x95, 0x01,
        0x15, 0x00,
        0x25, 0xFF,
        0x81, 0x02,

        // X
        0x05, 0x01,
        0x09, 0x30,
        0x75, 0x10,
        0x95, 0x01,
        0x15, 0x00,
        0x26,
    ];

    d.extend_from_slice(&x_max.to_le_bytes());

    d.extend_from_slice(&[
        0x81, 0x02,             // Input X

        // Y
        0x09, 0x31,
        0x75, 0x10,
        0x95, 0x01,
        0x15, 0x00,
        0x26,
    ]);

    d.extend_from_slice(&y_max.to_le_bytes());

    d.extend_from_slice(&[
        0x81, 0x02,             // Input Y

        0xC0,                   // End Finger

        // Contact Count
        0x05, 0x0D,
        0x09, 0x54,             // Usage (Contact Count)
        0x15, 0x00,
        0x25, 0x01,
        0x75, 0x08,
        0x95, 0x01,
        0x81, 0x02,

        0xC0,                   // End Application

        // Keyboard, Report ID 2
        0x05, 0x01,
        0x09, 0x06,
        0xA1, 0x01,

        0x85, 0x02,

        0x05, 0x07,
        0x19, 0xE0,
        0x29, 0xE7,
        0x15, 0x00,
        0x25, 0x01,
        0x75, 0x01,
        0x95, 0x08,
        0x81, 0x02,

        0x95, 0x01,
        0x75, 0x08,
        0x81, 0x03,

        0x95, 0x06,
        0x75, 0x08,
        0x15, 0x00,
        0x25, 0x65,

        0x05, 0x07,
        0x19, 0x00,
        0x29, 0x65,
        0x81, 0x00,

        0xC0,
    ]);

    d
}
fn build_report_descriptor_old(width: u16, height: u16) -> Vec<u8> {
    let x_max = width.saturating_sub(1);
    let y_max = height.saturating_sub(1);

    let mut d = vec![
        // ---- Touchscreen (Digitizer, single touch), Report ID 1 ----
        0x05, 0x0D, 0x09, 0x04, 0xA1, 0x01,
        0x85, 0x01,
        0x09, 0x22, 0xA1, 0x00,
        0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02,
        0x95, 0x07, 0x81, 0x03,
        0x05, 0x01, 0x09, 0x30, 0x75, 0x10, 0x95, 0x01,
        0x15, 0x00, 0x26, // Logical Maximum (X) — 2-byte value follows
    ];
    d.extend_from_slice(&x_max.to_le_bytes());
    d.extend_from_slice(&[0x81, 0x02, 0x09, 0x31, 0x75, 0x10, 0x95, 0x01, 0x15, 0x00, 0x26]);
    d.extend_from_slice(&y_max.to_le_bytes());
    d.extend_from_slice(&[
        0x81, 0x02,
        0xC0, 0xC0,
        // ---- Keypad, Report ID 2 ----
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01,
        0x85, 0x02,
        0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x08, 0x81, 0x03,
        0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65,
        0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00,
        0xC0,
    ]);
    d
}

/// Shared handle for sending HID input reports once a client is connected & subscribed.
#[derive(Clone)]
pub struct HidPeripheral {
    keyboard_writer: Arc<Mutex<Option<CharacteristicWriter>>>,
    touchpad_writer: Arc<Mutex<Option<CharacteristicWriter>>>,
    width: u16,
    height: u16,
}

impl HidPeripheral {
    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// Send a touch event.
    ///
    /// HID Report ID 4:
    ///
    ///     04 contact_count contact_id flags Xlo Xhi Ylo Yhi
    ///
    /// flags:
    ///     bit 0 = Tip Switch
    ///     bit 1 = In Range
    pub async fn send_touch(
        &self,
        down: bool,
        x: u16,
        y: u16,
    ) -> std::io::Result<()> {
        let x = x.min(self.width.saturating_sub(1));
        let y = y.min(self.height.saturating_sub(1));

        let contact_count = if down { 1 } else { 0 };
        let flags = if down { 0x03 } else { 0x00 };

        let report = [
            0x04, // Report ID = TOUCHPAD_ID
            contact_count,
            0x00, // Contact Identifier
            flags, // Tip Switch + In Range
            (x & 0xFF) as u8,
            (x >> 8) as u8,
            (y & 0xFF) as u8,
            (y >> 8) as u8,
        ];

        debug!("BT HID touch report: {:02X?}", report);

        self.write_touchpad_report(&report).await
    }

    /// Send a keyboard report.
    ///
    /// HID Report ID 1:
    ///
    ///     01 modifier reserved key1 key2 key3 key4 key5 key6
    ///
    /// `modifier`:
    ///     bit 0 = Left Ctrl
    ///     bit 1 = Left Shift
    ///     bit 2 = Left Alt
    ///     bit 3 = Left GUI
    ///     bit 4 = Right Ctrl
    ///     bit 5 = Right Shift
    ///     bit 6 = Right Alt
    ///     bit 7 = Right GUI
    ///
    /// `keys` contains up to six simultaneously pressed HID usage IDs.
    ///
    /// To release all keys:
    ///
    ///     send_key(0, [0; 6])
    pub async fn send_key(
        &self,
        modifier: u8,
        keys: [u8; 6],
    ) -> std::io::Result<()> {
        let mut report = [0u8; 9];

        report[0] = 0x01; // Report ID = KEYBOARD_ID
        report[1] = modifier;
        report[2] = 0x00; // Reserved

        report[3..9].copy_from_slice(&keys);

        debug!("BT HID keyboard report: {:02X?}", report);

        self.write_keyboard_report(&report).await
    }

    async fn write_keyboard_report(
        &self,
        report: &[u8],
    ) -> std::io::Result<()> {
        let mut guard = self.keyboard_writer.lock().await;

        let Some(writer) = guard.as_ref() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no HID keyboard client subscribed for notifications",
            ));
        };

        match writer.send(report).await {
            Ok(()) => Ok(()),

            Err(e) => {
                *guard = None;

                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("HID keyboard notification failed: {}", e),
                ))
            }
        }
    }

    async fn write_touchpad_report(
        &self,
        report: &[u8],
    ) -> std::io::Result<()> {
        let mut guard = self.touchpad_writer.lock().await;

        let Some(writer) = guard.as_ref() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no HID touchpad client subscribed for notifications",
            ));
        };

        match writer.send(report).await {
            Ok(()) => Ok(()),

            Err(e) => {
                *guard = None;

                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("HID touchpad notification failed: {}", e),
                ))
            }
        }
    }
}

fn report_reference_descriptor(report_id: u8, report_type: u8, ) -> Descriptor {
    Descriptor {
        uuid: Uuid::from_u16(0x2908),

        read: Some(DescriptorRead {
            read: true,

            fun: Box::new(move |_req| {
                let value = vec![report_id, report_type];

                Box::pin(async move {
                    Ok(value)
                })
            }),

            ..Default::default()
        }),

        ..Default::default()
    }
}

pub async fn start_hid_peripheral(
    adapter: &Adapter,
    width: u16,
    height: u16,
) -> bluer::Result<HidPeripheral> {
    // ------------------------------------------------------------
    // Build HID report descriptor
    // ------------------------------------------------------------

    let report_descriptor = build_report_descriptor(width, height);

    // ------------------------------------------------------------
    // Keyboard Input Report - ID 1
    // ------------------------------------------------------------

    let (keyboard_control, keyboard_handle) = characteristic_control();

    // ------------------------------------------------------------
    // Keyboard Output Report - ID 1
    // ------------------------------------------------------------

    let (keyboard_output_control, keyboard_output_handle) = characteristic_control();

    // ------------------------------------------------------------
    // Touchpad Input Report - ID 4
    // ------------------------------------------------------------

    let (touchpad_control, touchpad_handle) = characteristic_control();

    // ------------------------------------------------------------
    // GATT application
    // ------------------------------------------------------------

    let app = Application {
        services: vec![
            Service {
                uuid: HID_SERVICE,
                primary: true,

                characteristics: vec![
                    // ------------------------------------------------
                    // HID Information - 0x2A4A
                    // ------------------------------------------------
                    Characteristic {
                        uuid: HID_INFO_CHAR,

                        read: Some(CharacteristicRead {
                            read: true,

                            fun: Box::new(|_req| {
                                Box::pin(async move {
                                    Ok(vec![
                                        0x11, 0x01, // HID version 1.11
                                        0x00,       // Country code
                                        0x01,       // Flags=Normally connectable
                                    ])
                                })
                            }),

                            ..Default::default()
                        }),

                        ..Default::default()
                    },

                    // ------------------------------------------------
                    // Report Map - 0x2A4B
                    // ------------------------------------------------
                    Characteristic {
                        uuid: REPORT_MAP_CHAR,

                        read: Some(CharacteristicRead {
                            read: true,

                            fun: Box::new(move |_req| {
                                let descriptor = report_descriptor.clone();

                                Box::pin(async move {
                                    Ok(descriptor)
                                })
                            }),

                            ..Default::default()
                        }),

                        ..Default::default()
                    },

                    // ------------------------------------------------
                    // Keyboard Input Report - 0x2A4D
                    //
                    // Report Reference:
                    //     01 01
                    //
                    //     0x01 = Report ID
                    //     0x01 = Input
                    // ------------------------------------------------
                    Characteristic {
                        uuid: REPORT_CHAR,

                        notify: Some(CharacteristicNotify {
                            notify: true,
                            method: CharacteristicNotifyMethod::Io,
                            ..Default::default()
                        }),

                        descriptors: vec![
                            report_reference_descriptor(1, 0x01),
                        ],

                        control_handle: keyboard_handle,

                        ..Default::default()
                    },

                    // ------------------------------------------------
                    // Keyboard Output Report - 0x2A4D
                    //
                    // Report Reference:
                    //     01 02
                    //
                    //     0x01 = Report ID
                    //     0x02 = Output
                    //
                    // Used by Android for keyboard LEDs, etc.
                    // ------------------------------------------------
                    Characteristic {
                        uuid: REPORT_CHAR,

                        write: Some(CharacteristicWrite {
                            write: true,
                            write_without_response: true,

                            method: CharacteristicWriteMethod::Fun(
                                Box::new(|value, _req| {
                                    Box::pin(async move {
                                        log::debug!(
                                            "BT HID keyboard output: {:02X?}",
                                            value
                                        );

                                        Ok(())
                                    })
                                }),
                            ),

                            ..Default::default()
                        }),

                        descriptors: vec![
                            report_reference_descriptor(1, 0x02),
                        ],

                        control_handle: keyboard_output_handle,

                        ..Default::default()
                    },

                    // ------------------------------------------------
                    // Touchpad Input Report - 0x2A4D
                    //
                    // Report Reference:
                    //     04 01
                    //
                    //     0x04 = Report ID
                    //     0x01 = Input
                    // ------------------------------------------------
                    Characteristic {
                        uuid: REPORT_CHAR,

                        notify: Some(CharacteristicNotify {
                            notify: true,
                            method: CharacteristicNotifyMethod::Io,
                            ..Default::default()
                        }),

                        descriptors: vec![
                            report_reference_descriptor(4, 0x01),
                        ],

                        control_handle: touchpad_handle,

                        ..Default::default()
                    },

                    // ------------------------------------------------
                    // HID Control Point - 0x2A4C
                    // ------------------------------------------------
                    Characteristic {
                        uuid: HID_CONTROL_POINT_CHAR,

                        write: Some(CharacteristicWrite {
                            write_without_response: true,

                            method: CharacteristicWriteMethod::Fun(
                                Box::new(|value, _req| {
                                    Box::pin(async move {
                                        log::debug!("BT HID control point: {:02X?}",value);
                                        Ok(())
                                    })
                                }),
                            ),

                            ..Default::default()
                        }),

                        ..Default::default()
                    },

                    // ------------------------------------------------
                    // Protocol Mode - 0x2A4E
                    //
                    // 0x01 = Report Protocol
                    // ------------------------------------------------
                    Characteristic {
                        uuid: PROTOCOL_MODE_CHAR,

                        read: Some(CharacteristicRead {
                            read: true,

                            fun: Box::new(|_req| {
                                Box::pin(async move {
                                    Ok(vec![0x01])
                                })
                            }),

                            ..Default::default()
                        }),

                        write: Some(CharacteristicWrite {
                            write: true,
                            write_without_response: true,

                            method: CharacteristicWriteMethod::Fun(
                                Box::new(|value, _req| {
                                    Box::pin(async move {
                                        log::debug!("BT HID protocol mode: {:02X?}",value);
                                        Ok(())
                                    })
                                }),
                            ),

                            ..Default::default()
                        }),

                        ..Default::default()
                    },
                ],

                ..Default::default()
            },
        ],

        ..Default::default()
    };

    // ------------------------------------------------------------
    // Bluetooth adapter
    // ------------------------------------------------------------

    adapter.set_powered(true).await?;
    adapter.set_pairable(true).await?;

    // ------------------------------------------------------------
    // Register GATT application
    // ------------------------------------------------------------

    let app_handle = adapter
        .serve_gatt_application(app)
        .await?;

    // ------------------------------------------------------------
    // Advertisement
    // ------------------------------------------------------------

    let mut service_uuids = BTreeSet::new();
    service_uuids.insert(HID_SERVICE);

    let le_advertisement = Advertisement {
        advertisement_type: bluer::adv::Type::Peripheral,
        service_uuids,
        appearance: Some(0x03C0),
        local_name: Some("aa-mirror-rs HID".to_string()),
        discoverable: Some(true),
        ..Default::default()
    };

    let mut adv_handle = None;

    for attempt in 0..3 {
        match adapter
            .advertise(le_advertisement.clone())
            .await
        {
            Ok(handle) => {
                info!("{} 📣 BLE advertisement started with UUIDs (attempt {})",NAME, attempt + 1);
                adv_handle = Some(handle);
                break;
            }

            Err(e) => {
                warn!(
                    "{} 🥏 Advertising attempt {} failed: {}",
                    NAME,
                    attempt + 1,
                    e
                );

                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }

    let adv_handle = match adv_handle {
        Some(handle) => handle,

        None => {
            return Err(
                bluer::Error::from(
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Failed to register BLE advertisement \
                         after 3 attempts",
                    ),
                )
            );
        }
    };

    // ------------------------------------------------------------
    // Keyboard notification writer
    // ------------------------------------------------------------

    let keyboard_writer:
        Arc<Mutex<Option<CharacteristicWriter>>> =
        Arc::new(Mutex::new(None));

    let keyboard_writer_task =
        keyboard_writer.clone();

    tokio::spawn(async move {
        let mut control = keyboard_control;

        loop {
            match control.next().await {
                Some(CharacteristicControlEvent::Notify(writer)) => {
                    info!(
                        "BT HID keyboard subscribed, MTU={}",
                        writer.mtu()
                    );

                    *keyboard_writer_task.lock().await =
                        Some(writer);
                }

                Some(CharacteristicControlEvent::Write(_)) => {
                    // Keyboard input characteristic isn't writable.
                }

                None => {
                    warn!(
                        "BT HID keyboard control stream ended"
                    );

                    *keyboard_writer_task.lock().await =
                        None;

                    break;
                }
            }
        }
    });

    // ------------------------------------------------------------
    // Keyboard output control stream NOT NEEDED
    // ------------------------------------------------------------

    /*tokio::spawn(async move {
        let mut control = keyboard_output_control;

        loop {
            match control.next().await {
                Some(CharacteristicControlEvent::Write(_)) => {
                    // Actual keyboard LED data is handled by the
                    // CharacteristicWrite callback above.
                }

                Some(CharacteristicControlEvent::Notify(_)) => {
                    // This characteristic isn't a notify characteristic.
                }

                None => {
                    debug!(
                        "BT HID keyboard output control stream ended"
                    );

                    break;
                }
            }
        }
    });*/

    // ------------------------------------------------------------
    // Touchpad notification writer
    // ------------------------------------------------------------

    let touchpad_writer:
        Arc<Mutex<Option<CharacteristicWriter>>> =
        Arc::new(Mutex::new(None));

    let touchpad_writer_task =
        touchpad_writer.clone();

    tokio::spawn(async move {
        let mut control = touchpad_control;

        loop {
            match control.next().await {
                Some(CharacteristicControlEvent::Notify(writer)) => {
                    info!(
                        "BT HID touchpad subscribed, MTU={}",
                        writer.mtu()
                    );

                    *touchpad_writer_task.lock().await =
                        Some(writer);
                }

                Some(CharacteristicControlEvent::Write(_)) => {
                    // Touchpad input characteristic isn't writable.
                }

                None => {
                    warn!(
                        "BT HID touchpad control stream ended"
                    );

                    *touchpad_writer_task.lock().await =
                        None;

                    break;
                }
            }
        }
    });

    // ------------------------------------------------------------
    // Keep the GATT application and advertisement alive.
    // ------------------------------------------------------------

    std::mem::forget(app_handle);
    std::mem::forget(adv_handle);

    // ------------------------------------------------------------
    // Return peripheral
    // ------------------------------------------------------------

    Ok(HidPeripheral {
        keyboard_writer,
        touchpad_writer,
        width,
        height,
    })
}

// Create and configure the Bluetooth adapter
pub async fn init(
    btalias: Option<String>,
    advertise: bool,
    dongle_mode: bool,
) -> Result<Bluetooth> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;

    // setting BT alias for further use
    let alias = match btalias {
        None => match get_cpu_serial_number_suffix().await {
            Ok(suffix) => format!("{}-{}", IDENTITY_NAME, suffix),
            Err(_) => String::from(IDENTITY_NAME),
        },
        Some(btalias) => btalias,
    };
    info!("{} 🥏 Bluetooth alias: <bold><green>{}</>", NAME, alias);

    info!(
        "{} 🥏 Opened bluetooth adapter <b>{}</> with address <b>{}</b>",
        NAME,
        adapter.name(),
        adapter.address().await?
    );
    adapter.set_alias(alias.clone()).await?;
    adapter.set_powered(true).await?;
    adapter.set_pairable(true).await?;

    if advertise {
        adapter.set_discoverable(true).await?;
        adapter.set_discoverable_timeout(0).await?;
    }

    // AA Wireless profile
    let profile = Profile {
        uuid: AAWG_PROFILE_UUID,
        name: Some("AA Wireless".to_string()),
        channel: Some(8),
        role: Some(Role::Server),
        require_authentication: Some(false),
        require_authorization: Some(false),
        ..Default::default()
    };
    let handle_aa = session.register_profile(profile).await?;
    info!("{} 📱 AA Wireless Profile: registered", NAME);
    if !dongle_mode {
        // Headset profile
        let profile = Profile {
            uuid: HSP_HS_UUID,
            name: Some("HSP HS".to_string()),
            require_authentication: Some(false),
            require_authorization: Some(false),
            ..Default::default()
        };
        match session.register_profile(profile).await {
            Ok(mut handle) => {
                info!("{} 🎧 Headset Profile (HSP): registered", NAME);
                // handling connection to headset profile in own task
                // it only accepts each incoming connection
                let _ = Some(tokio::spawn(async move {
                    loop {
                        let req = handle.next().await.expect("received no connect request");
                        info!(
                            "{} 🎧 Headset Profile (HSP): connect from: <b>{}</>",
                            NAME,
                            req.device()
                        );
                        let _ = req.accept();
                    }
                }));
            }
            Err(e) => {
                warn!(
                    "{} 🎧 Headset Profile (HSP) registering error: {}, ignoring",
                    NAME, e
                );
            }
        }
    }

    Ok(Bluetooth {
        adapter,
        handle_aa,
        btle_handle: None,
        adv_handle: None,
        current_index: 0,
    })
}

pub async fn get_cpu_serial_number_suffix() -> Result<String> {
    let mut serial = String::new();
    let contents = tokio::fs::read_to_string("/sys/firmware/devicetree/base/serial-number").await?;
    let trimmed = contents.trim_end_matches(char::from(0)).trim();
    // check if we read the serial number with correct length
    if trimmed.len() >= 6 {
        serial = trimmed[trimmed.len() - 6..].to_string();
    }
    Ok(serial)
}

async fn send_message(
    stream: &mut Stream,
    stage: u8,
    id: MessageId,
    message: impl Message,
) -> Result<usize> {
    let mut packet: Vec<u8> = vec![];
    let mut data = message.write_to_bytes()?;

    // create header: 2 bytes message length + 2 bytes MessageID
    packet.write_u16(data.len() as u16).await?;
    packet.write_u16(id.clone() as u16).await?;

    // append data and send
    packet.append(&mut data);

    info!(
        "{} 📨 stage #{} of {}: Sending <yellow>{:?}</> frame to phone...",
        NAME, stage, STAGES, id
    );

    Ok(stream.write(&packet).await?)
}

async fn read_message(
    stream: &mut Stream,
    stage: u8,
    id: MessageId,
    started: Instant,
) -> Result<usize> {
    let mut buf = vec![0; HEADER_LEN];
    let n = stream.read_exact(&mut buf).await?;
    debug!("received {} bytes: {:02X?}", n, buf);
    let elapsed = started.elapsed();

    let len: usize = u16::from_be_bytes(buf[0..=1].try_into()?).into();
    let message_id = u16::from_be_bytes(buf[2..=3].try_into()?);
    debug!("MessageID = {}, len = {}", message_id, len);

    if message_id != id.clone() as u16 {
        warn!(
            "Received data has invalid MessageID: got: {:?}, expected: {:?}",
            message_id, id
        );
    }
    info!(
        "{} 📨 stage #{} of {}: Received <yellow>{:?}</> frame from phone (⏱️ {} ms)",
        NAME,
        stage,
        STAGES,
        id,
        (elapsed.as_secs() * 1_000) + (elapsed.subsec_nanos() / 1_000_000) as u64,
    );

    // read and discard the remaining bytes
    if len > 0 {
        let mut buf = vec![0; len];
        let n = stream.read_exact(&mut buf).await?;
        debug!("remaining {} bytes: {:02X?}", n, buf);

        // analyzing WifiConnectStatus
        // this is a frame where phone cannot connect to WiFi:
        // [08, FD, FF, FF, FF, FF, FF, FF, FF, FF, 01] -> which is -i64::MAX
        // and this is where all is fine:
        // [08, 00]
        if id == MessageId::WifiConnectStatus && n >= 2 {
            if buf[1] != 0 {
                return Err("phone cannot connect to our WiFi AP...".into());
            }
        }
    }

    Ok(HEADER_LEN + len)
}

impl Bluetooth {
    pub async fn start_ble(&mut self, state: AppState, enable_btle: bool) -> Result<()> {
        // --- Start BLE GATT server first ---
        if enable_btle {
            match btle::run_btle_server(&self.adapter, state.clone()).await {
                Ok(handle) => {
                    info!("{} 🥏 BLE GATT server started successfully", NAME);
                    self.btle_handle = Some(handle);
                }
                Err(e) => {
                    error!("{} 🥏 Failed to start BLE server: {}", NAME, e);
                }
            }
        }

        // --- Prepare UUIDs ---
        let mut uuids: std::collections::BTreeSet<bluer::Uuid> = std::collections::BTreeSet::new();
        uuids.insert(BTLE_PROFILE_UUID);

        // --- BLE advertisement ---
        if !uuids.is_empty() {
            // Stop any previous advertisement first
            if let Some(handle) = self.adv_handle.take() {
                drop(handle);
            }

            let mut le_advertisement = bluer::adv::Advertisement {
                advertisement_type: bluer::adv::Type::Peripheral,
                service_uuids: uuids.clone(),
                discoverable: Some(true), // temporarily true for stable discovery
                local_name: Some(self.adapter.alias().await?),
                ..Default::default()
            };

            let mut adv_success = false;
            for attempt in 0..3 {
                match self.adapter.advertise(le_advertisement.clone()).await {
                    Ok(handle) => {
                        info!(
                            "{} 📣 BLE advertisement started with UUIDs (attempt {})",
                            NAME,
                            attempt + 1
                        );
                        self.adv_handle = Some(handle);
                        adv_success = true;
                        break;
                    }
                    Err(e) => {
                        warn!(
                            "{} 🥏 Advertising attempt {} failed: {}",
                            NAME,
                            attempt + 1,
                            e
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }

            if !adv_success {
                warn!(
                    "{} 🥏 Advertising with UUIDs failed, fallback to local name only",
                    NAME
                );

                // Retry only with local name
                if let Some(handle) = self.adv_handle.take() {
                    drop(handle);
                }

                le_advertisement.service_uuids = Default::default();

                for attempt in 0..3 {
                    match self.adapter.advertise(le_advertisement.clone()).await {
                        Ok(handle) => {
                            info!(
                                "{} 📣 BLE advertisement started with local name only (attempt {})",
                                NAME,
                                attempt + 1
                            );
                            self.adv_handle = Some(handle);
                            adv_success = true;
                            break;
                        }
                        Err(e) => {
                            warn!(
                                "{} 🥏 Local-name-only advertising attempt {} failed: {}",
                                NAME,
                                attempt + 1,
                                e
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    }
                }

                if !adv_success {
                    error!(
                        "{} 🥏 BLE advertisement completely failed after retries",
                        NAME
                    );
                }
            }
        }

        Ok(())
    }

    async fn get_aa_profile_connection(
        &mut self,
        connect: MACAddressList,
        bt_timeout: Duration,
        stopped: bool,
    ) -> Result<(Address, Stream)> {
        info!("{} ⏳ Waiting for phone to connect via bluetooth...", NAME);

        // try to connect to saved devices or provided one via command line
        if let Some(addresses_to_connect) = connect.0 {
            if !stopped {
                let adapter_cloned = self.adapter.clone();

                let addresses: Vec<Address> = if addresses_to_connect
                    .iter()
                    .any(|addr| *addr == Address::any())
                {
                    info!("{} 🥏 Enumerating known bluetooth devices...", NAME);
                    adapter_cloned.device_addresses().await?
                } else {
                    addresses_to_connect
                };
                // exit if we don't have anything to connect to
                if !addresses.is_empty() {
                    info!("{} 🧲 Attempting to start an AndroidAuto session via bluetooth with the following devices, in this order: {:?}", NAME, addresses);
                    let try_connect_bluetooth_addresses_retry = || async {
                        let next_index = Bluetooth::try_connect_bluetooth_addresses(
                            &adapter_cloned,
                            &addresses,
                            self.current_index,
                        )
                            .await?;

                        Ok(next_index)
                    };

                    let retry_policy = ExponentialBuilder::default()
                        .with_min_delay(Duration::from_secs(1))
                        .with_max_delay(Duration::from_secs(15))
                        .without_max_times();

                    self.current_index = try_connect_bluetooth_addresses_retry
                        // Retry with exponential backoff
                        .retry(retry_policy)
                        // Sleep implementation, required if no feature has been enabled
                        .sleep(tokio::time::sleep)
                        // Notify when retrying;
                        .notify(
                            |err: &Box<dyn std::error::Error + Send + Sync + 'static>,
                             dur: Duration| {
                                debug!("{} Retrying due to error: {:?} after {:?}", NAME, err, dur);
                            },
                        )
                        .await?;
                }
            }
        }

        let req = timeout(bt_timeout, self.handle_aa.next()).await?.expect("received no connect request");
        info!("{} 📱 AA Wireless Profile: connect from: <b>{}</>", NAME, req.device());
        let addr = req.device().clone();
        let stream = req.accept()?;

        Ok((addr, stream))
    }

    async fn try_connect_bluetooth_addresses(
        adapter: &Adapter,
        addresses: &Vec<Address>,
        start_index: usize,
    ) -> Result<(usize)> {
        let n = addresses.len();
        for i in 0..n {
            // Calculate the actual index, taking start_index into account
            let idx = (start_index + i) % n;
            let addr = addresses[idx];
            let device = adapter.device(addr)?;

            let dev_name = match device.name().await {
                Ok(Some(name)) => format!(" (<b><blue>{}</>)", name),
                _ => String::new(),
            };
            for j in 1..=ATTEMPTS {
                info!(
                    "{} 🧲 Trying to connect to: {}{}, attempt: {}/{}",
                    NAME, addr, dev_name, j, ATTEMPTS
                );
                if let Ok(true) = device.is_paired().await {
                    match device.connect_profile(&HSP_AG_UUID).await {
                        Ok(_) => {
                            info!(
                                "{} 🔗 Successfully connected to device: {}{}",
                                NAME, addr, dev_name
                            );
                            return Ok((idx + 1) % n);
                        }
                        Err(e) => {
                            warn!("{} 🔇 {}{}: Error connecting: {}", NAME, addr, dev_name, e)
                        }
                    }
                } else {
                    warn!(
                        "{} 🧲 Unable to connect to: {}{} device not paired",
                        NAME, addr, dev_name
                    );
                }
            }
        }
        Err(anyhow!("Unable to connect to the provided addresses").into())
    }

    async fn cleanup_failed_bluetooth_connect(device: &Device) -> Result<()> {
        let cleanup_delay = Duration::from_secs(2);
        let _ = timeout(cleanup_delay, device.disconnect()).await;
        debug!(
            "{} Cleaned up bluetooth connection for device: {:?}",
            NAME,
            device.name().await
        );
        Ok(())
    }

    async fn send_params(wifi_config: WifiConfig, stream: &mut Stream) -> Result<()> {
        use WifiInfoResponse::WifiInfoResponse;
        use WifiStartRequest::WifiStartRequest;
        let mut stage = 1;
        let mut started;

        info!("{} 📲 Sending parameters via bluetooth to phone...", NAME);
        let mut start_req = WifiStartRequest::new();
        info!(
            "{} 🛜 Sending Host IP Address: {}",
            NAME, wifi_config.ip_addr
        );
        start_req.set_ip_address(wifi_config.ip_addr);
        start_req.set_port(wifi_config.port);
        send_message(stream, stage, MessageId::WifiStartRequest, start_req).await?;
        stage += 1;
        started = Instant::now();
        read_message(stream, stage, MessageId::WifiInfoRequest, started).await?;

        let mut info = WifiInfoResponse::new();
        info!(
            "{} 🛜 Sending Host SSID and Password: {}, {}",
            NAME, wifi_config.ssid, wifi_config.wpa_key
        );
        info.set_ssid(wifi_config.ssid);
        info.set_key(wifi_config.wpa_key);
        info.set_bssid(wifi_config.bssid);
        info.set_security_mode(SecurityMode::WPA2_PERSONAL);
        info.set_access_point_type(AccessPointType::DYNAMIC);
        stage += 1;
        send_message(stream, stage, MessageId::WifiInfoResponse, info).await?;
        stage += 1;
        started = Instant::now();
        read_message(stream, stage, MessageId::WifiStartResponse, started).await?;
        stage += 1;
        started = Instant::now();
        read_message(stream, stage, MessageId::WifiConnectStatus, started).await?;

        Ok(())
    }

    pub async fn aa_handshake(
        &mut self,
        connect: MACAddressList,
        wifi_config: WifiConfig,
        bt_timeout: Duration,
        stopped: bool,
        bt_poweroff: bool,
    ) -> Result<()> {
        // Use the provided session and adapter instead of creating new ones
        let (address, mut stream) = self.get_aa_profile_connection(connect, bt_timeout, stopped).await?;
        Self::send_params(wifi_config.clone(), &mut stream).await?;
        //tcp_start.notify_one();

        // attempt graceful shutdown of the RFCOMM stream before disconnect
        let _ = stream.shutdown().await;
        // let some phones that have problems with handshake time to
        // finish all bluetooth frames before disconnect
        let _ = tokio::time::sleep(Duration::from_millis(150));
        // handshake complete, now disconnect the device so it should
        // connect to real HU for calls
        let device = self.adapter.device(bluer::Address(*address))?;
        let _ = device.disconnect().await;
        if bt_poweroff {
            let _ = self.adapter.set_powered(false).await;
        }

        info!("{} 🚀 Bluetooth launch sequence completed", NAME);

        Ok(())
    }
}
