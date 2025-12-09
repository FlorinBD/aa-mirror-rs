//! This crate provides service implementation for  [Android Open Accessory Protocol 1.0](https://source.android.com/devices/accessories/aoa)
use log::log_enabled;
use simplelog::*;
use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use nusb::DeviceInfo;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::timeout;
use tokio_uring::buf::BoundedBuf;

// protobuf stuff:
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use crate::aa_services::protos::navigation_maneuver::NavigationType::*;
use crate::aa_services::protos::auth_response::Status::*;
use crate::aa_services::protos::Config as AudioConfig;
use crate::aa_services::protos::*;
use crate::aa_services::sensor_source_service::Sensor;
use crate::aa_services::AudioStreamType::*;
use crate::aa_services::ByeByeReason::USER_SELECTION;
use crate::aa_services::MediaMessageId::*;
use crate::aa_services::InputMessageId::*;
use crate::aa_services::SensorMessageId::*;
use crate::aa_services::SensorType::*;
use protobuf::text_format::print_to_string_pretty;
use protobuf::{Enum, EnumOrUnknown, Message, MessageDyn};
use protos::ControlMessageType::{self, *};
use crate::aoa::AccessoryDeviceInfo;
use crate::channel_manager::Packet;
use crate::io_uring::Endpoint;
use crate::io_uring::IoDevice;

#[derive(Copy, Clone, Debug)]
pub enum ServiceType
{
    InputSource,
    MediaSink,
    MediaSource,
    SensorSource,
    VendorExtension,
}
impl fmt::Display for ServiceType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
        // or, alternatively:
        // fmt::Debug::fmt(self, f)
    }
}


pub trait IService{
    fn handle_hu_msg(&self, pkt: &Packet)->();
    fn get_service_type(&self)->ServiceType;
}
///MediaSink implementation
pub struct MediaSinkService {
    sid: ServiceType,
    ch_id: i32,
    tx_srv: Sender<Packet>,
}
impl Clone for MediaSinkService {
    fn clone(&self) -> Self {
        MediaSinkService {
            sid: self.sid.clone(),
            ch_id: self.ch_id.clone(),
            tx_srv: self.tx_srv.clone()
        }
    }
}
impl MediaSinkService {
    pub fn new(pch:i32, tx: Sender<Packet>) -> Self {
        Self{
            sid:ServiceType::MediaSink,
            ch_id:pch,
            tx_srv: tx,
        }
    }
}
impl IService for MediaSinkService {
    fn handle_hu_msg(&self, pkt: &Packet)
    {
        if let Ok(id)=pkt.payload[0..=1].try_into()//FIXME catch the error when not enough data is inside
        {
            let message_id: i32 = u16::from_be_bytes(id).into();

            let control = protos::MediaMessageId::from_i32(message_id);
            match control.unwrap() {
                MEDIA_MESSAGE_VIDEO_FOCUS_NOTIFICATION => {
                    info!("{} Received {} message", self.sid.to_string(), message_id);
                }
                _ =>{ error!( "{} Unhandled message ID: {} received",self.sid.to_string(), message_id);}
            }
        }

    }

    fn get_service_type(&self)->ServiceType
    {
        return self.sid;
    }
}

///MediaSource implementation
pub struct MediaSourceService {
    sid: ServiceType,
    ch_id: i32,
}
impl Clone for MediaSourceService {
    fn clone(&self) -> Self {
        MediaSourceService {
            sid: self.sid.clone(),
            ch_id: self.ch_id.clone()
        }
    }
}
impl MediaSourceService {
    pub fn new(pch:i32) -> Self {
        Self{
            sid:ServiceType::MediaSource,
            ch_id:pch,
        }
    }
}
impl IService for MediaSourceService {
    fn handle_hu_msg(&self, pkt: &Packet)
    {
        if let Ok(id)=pkt.payload[0..=1].try_into()//FIXME catch the error when not enough data is inside
        {
            let message_id: i32 = u16::from_be_bytes(id).into();

            let control = protos::MediaMessageId::from_i32(message_id);
            match control.unwrap() {
                MEDIA_MESSAGE_VIDEO_FOCUS_NOTIFICATION => {
                    info!("{} Received {} message", self.sid.to_string(), message_id);
                }
                _ =>{ error!( "{} Unhandled message ID: {} received",self.sid.to_string(), message_id);}
            }
        }
    }

    fn get_service_type(&self)->ServiceType
    {
        return self.sid;
    }
}

///SensorSourceService implementation
pub struct SensorSourceService {
    sid: ServiceType,
    ch_id: i32,
}
impl Clone for SensorSourceService {
    fn clone(&self) -> Self {
        SensorSourceService {
            sid: self.sid.clone(),
            ch_id: self.ch_id.clone()
        }
    }
}
impl SensorSourceService {
    pub fn new(pch:i32) -> Self {
        //info!( "SensorSourceService init ok");
        Self{
            sid:ServiceType::SensorSource,
            ch_id:pch,
        }
    }
}
impl IService for SensorSourceService {
    fn handle_hu_msg(&self, pkt: &Packet)
    {
        if let Ok(id)=pkt.payload[0..=1].try_into()//FIXME catch the error when not enough data is inside
        {
            let message_id: i32 = u16::from_be_bytes(id).into();

            let control = protos::SensorMessageId::from_i32(message_id);
            match control.unwrap() {
                SENSOR_MESSAGE_RESPONSE => {
                    info!("{} Received {} message", self.sid.to_string(), message_id);
                }
                _ =>{ error!( "{} Unhandled message ID: {} received",self.sid.to_string(), message_id);}
            }
        }

    }

    fn get_service_type(&self)->ServiceType
    {
        return self.sid;
    }
}

///InputSourceService implementation
pub struct InputSourceService {
    sid: ServiceType,
    ch_id: i32,
}
impl Clone for InputSourceService {
    fn clone(&self) -> Self {
        InputSourceService {
            sid: self.sid.clone(),
            ch_id: self.ch_id.clone()
        }
    }
}
impl InputSourceService {
    pub fn new(pch:i32) -> Self {
        Self{
            sid:ServiceType::InputSource,
            ch_id:pch,
        }
    }
}
impl IService for InputSourceService {
    fn handle_hu_msg(&self, pkt: &Packet)
    {
        if let Ok(id)=pkt.payload[0..=1].try_into()//FIXME catch the error when not enough data is inside
        {
            let message_id: i32 = u16::from_be_bytes(id).into();

            let control = protos::InputMessageId::from_i32(message_id);
            match control.unwrap() {
                INPUT_MESSAGE_KEY_BINDING_RESPONSE => {
                    info!("{} Received {} message", self.sid.to_string(), message_id);
                }
                _ =>{ error!( "{} Unhandled message ID: {} received",self.sid.to_string(), message_id);}
            }
        }

    }

    fn get_service_type(&self)->ServiceType
    {
        return self.sid;
    }
}

///VendorExtensionService implementation
pub struct VendorExtensionService {
    sid: ServiceType,
    ch_id: i32,
}
impl Clone for VendorExtensionService {
    fn clone(&self) -> Self {
        VendorExtensionService {
            sid: self.sid.clone(),
            ch_id: self.ch_id.clone()
        }
    }
}
impl VendorExtensionService {
    pub fn new(pch:i32) -> Self {
        Self{
            sid:ServiceType::VendorExtension,
            ch_id:pch,
        }
    }
}
impl IService for VendorExtensionService {
    fn handle_hu_msg(&self, pkt: &Packet)
    {
        if let Ok(id)=pkt.payload[0..=1].try_into()//FIXME catch the error when not enough data is inside
        {
            let message_id: i32 = u16::from_be_bytes(id).into();

            let control = protos::MediaMessageId::from_i32(message_id);
            match control.unwrap() {
                MEDIA_MESSAGE_SETUP => {
                    info!("{} Received {} message", self.sid.to_string(), message_id);
                }
                _ =>{ error!( "{} Unhandled message ID: {} received",self.sid.to_string(), message_id);}
            }
        }

    }

    fn get_service_type(&self)->ServiceType
    {
        return self.sid;
    }
}