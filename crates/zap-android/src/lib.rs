//! JNI bridge for the Android app.
//!
//! The Android app is a thin Kotlin shell; the actual file-transfer server is
//! [`zap_core::web`], the same code the desktop CLI runs. On Android the phone
//! *is* the server, so a foreground service calls these functions to start the
//! server (bound to all interfaces so devices on the home Wi-Fi can reach it),
//! query its URL for display, and stop it.
//!
//! The exported symbol names must match a Kotlin class exactly. They map to:
//!
//! ```kotlin
//! package com.zap.transfer
//! object NativeBridge {
//!     external fun nativeStart(dir: String, port: Int): Long  // 0 on failure
//!     external fun nativeUrl(handle: Long): String?
//!     external fun nativeStop(handle: Long)
//! }
//! ```
//!
//! `nativeStart` returns an opaque handle (a raw pointer as a `jlong`) that the
//! Kotlin side stores and passes back to `nativeUrl` / `nativeStop`. It owns the
//! running server; `nativeStop` frees it and shuts the server down.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

use jni::objects::{JClass, JString};
use jni::sys::{jint, jlong, jstring};
use jni::JNIEnv;

use zap_core::web::{self, Credentials, ServeConfig, ServerHandle, ServerInfo};

/// Owns a running server plus its connection details, boxed and handed to Kotlin
/// as an opaque `jlong` handle.
struct Running {
    info: ServerInfo,
    // Kept alive for the server's lifetime; dropping it stops the server.
    _handle: ServerHandle,
}

/// Read a Java string that may be null or empty, returning `None` in those cases.
fn read_opt(env: &mut JNIEnv, s: JString) -> Option<String> {
    if s.is_null() {
        return None;
    }
    match env.get_string(&s) {
        Ok(js) => {
            let v: String = js.into();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        }
        Err(_) => None,
    }
}

/// Start the server sharing `dir` on `port`, bound to all interfaces.
/// `user`/`pass` may be null/empty for no authentication; if both are present,
/// the server requires HTTP Basic auth. Returns an opaque handle, or 0 on error.
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeStart<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    dir: JString<'local>,
    port: jint,
    user: JString<'local>,
    pass: JString<'local>,
) -> jlong {
    let dir: String = match env.get_string(&dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };

    let auth = match (read_opt(&mut env, user), read_opt(&mut env, pass)) {
        (Some(user), Some(pass)) => Some(Credentials { user, pass }),
        _ => None,
    };

    let config = ServeConfig {
        dir: PathBuf::from(dir),
        port: port as u16,
        bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED), // 0.0.0.0 — reachable on the LAN
        auth,
    };

    match web::spawn(config) {
        Ok((info, handle)) => {
            let running = Box::new(Running {
                info,
                _handle: handle,
            });
            Box::into_raw(running) as jlong
        }
        Err(_) => 0,
    }
}

/// Return the URL another device should open, or null for an invalid handle.
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeUrl<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    if handle == 0 {
        return std::ptr::null_mut();
    }
    // Safety: `handle` is a pointer produced by `nativeStart` and not yet freed.
    let running = unsafe { &*(handle as *const Running) };
    match env.new_string(running.info.url()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Stop the server and free the handle. Safe to call with 0 (no-op).
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeStop(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // Safety: `handle` came from `nativeStart` and is freed exactly once here.
    // Dropping the box drops the `ServerHandle`, which stops the server.
    unsafe {
        drop(Box::from_raw(handle as *mut Running));
    }
}
