#![allow(non_camel_case_types)]
use makepad_jni_sys as jni_sys;
use jni_sys::*;
use std::os::raw::{c_long,c_ulong};

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct AMidiDevice {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct AMidiInputPort {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct AMidiOutputPort {
    _unused: [u8; 0],
}

pub type media_status_t = std::os::raw::c_int;

/// NdkMediaError.h `AMEDIA_ERROR_UNSUPPORTED`, returned when libamidi is absent.
pub const AMEDIA_ERROR_UNSUPPORTED: media_status_t = -10002;

// Paseo: libamidi exists from API 29; resolve it at runtime (see ndk_dynamic.rs).
super::ndk_dynamic::dynamic_ndk_library! {
    "libamidi.so", AMidiApi, amidi_api;
    fn AMidiDevice_fromJava(env: *mut JNIEnv, midiDeviceObj: jobject, outDevicePtrPtr: *mut *mut AMidiDevice) -> media_status_t = AMEDIA_ERROR_UNSUPPORTED;
    fn AMidiDevice_release(midiDevice: *const AMidiDevice) -> media_status_t = AMEDIA_ERROR_UNSUPPORTED;
    fn AMidiDevice_getNumInputPorts(device: *const AMidiDevice) -> c_long = AMEDIA_ERROR_UNSUPPORTED as c_long;
    fn AMidiDevice_getNumOutputPorts(device: *const AMidiDevice) -> c_long = AMEDIA_ERROR_UNSUPPORTED as c_long;
    fn AMidiOutputPort_open(device: *const AMidiDevice, portNumber: i32, outOutputPortPtr: *mut *mut AMidiOutputPort) -> media_status_t = AMEDIA_ERROR_UNSUPPORTED;
    fn AMidiOutputPort_close(outputPort: *const AMidiOutputPort) = ();
    fn AMidiInputPort_open(device: *const AMidiDevice, portNumber: i32, outInputPortPtr: *mut *mut AMidiInputPort) -> media_status_t = AMEDIA_ERROR_UNSUPPORTED;
    fn AMidiInputPort_send(inputPort: *const AMidiInputPort, buffer: *const u8, numBytes: c_ulong) -> c_long = AMEDIA_ERROR_UNSUPPORTED as c_long;
    fn AMidiInputPort_close(inputPort: *const AMidiInputPort) = ();
    fn AMidiOutputPort_receive(
        outputPort: *const AMidiOutputPort,
        opcodePtr: *mut i32,
        buffer: *mut u8,
        maxBytes: c_ulong,
        numBytesReceivedPtr: *mut c_ulong,
        outTimestampPtr: *mut i64,
    ) -> c_long = AMEDIA_ERROR_UNSUPPORTED as c_long;
}