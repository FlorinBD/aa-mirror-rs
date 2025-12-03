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
use crate::aa_services::SensorMessageId::*;
use crate::aa_services::SensorType::*;
use protobuf::text_format::print_to_string_pretty;
use protobuf::{Enum, EnumOrUnknown, Message, MessageDyn};
use protos::ControlMessageType::{self, *};
use crate::aoa::AccessoryDeviceInfo;

#[derive(Copy, Clone)]
pub enum ServiceType
{
    InputSource,
    MediaSink,
    MediaSource,
    SensorSource,
    VendorExtension,
}
pub trait IService: Sized{
    fn handle_hu_msg(&self, a: i32);
    fn get_service_type(&self)->ServiceType;
}
#[derive(Copy, Clone)]
pub struct MediaSinkService {
    pub sid: ServiceType,
}

impl IService for MediaSinkService {
    fn handle_hu_msg(&self, a: &str)
    {

    }

    fn get_service_type(&self)->ServiceType
    {
        return self.sid;
    }
}
#[derive(Copy, Clone)]
pub struct MediaSourceService {
    pub(crate) sid: ServiceType,
}

impl IService for MediaSourceService {
    fn handle_hu_msg(&self, a: &str)
    {

    }

    fn get_service_type(&self)->ServiceType
    {
        return self.sid;
    }
}
