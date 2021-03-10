// Copyright (c) 2019 Ant Financial
//
// SPDX-License-Identifier: Apache-2.0
//

use crate::AGENT_CONFIG;
use crate::device::online_device;
use crate::linux_abi::*;
use crate::sandbox::Sandbox;
use slog::Logger;

use anyhow::{anyhow, Result};
use netlink_sys::{protocols, SocketAddr, TokioSocket};
use nix::errno::Errno;
use std::fmt::Debug;
use std::os::unix::io::FromRawFd;
use std::sync::Arc;
use tokio::sync::Mutex;

// Convenience macro to obtain the scope logger
macro_rules! sl {
    () => {
        slog_scope::logger().new(o!("subsystem" => "uevent"))
    };
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Uevent {
    pub action: String,
    pub devpath: String,
    pub devname: String,
    pub subsystem: String,
    seqnum: String,
    pub interface: String,
}

pub trait UeventMatcher: Sync+Send+Debug+'static {
    fn is_match(&self, uev: &Uevent) -> bool;
}

impl Uevent {
    fn new(message: &str) -> Self {
        let mut msg_iter = message.split('\0');
        let mut event = Uevent::default();

        msg_iter.next(); // skip the first value
        for arg in msg_iter {
            let key_val: Vec<&str> = arg.splitn(2, '=').collect();
            if key_val.len() == 2 {
                match key_val[0] {
                    U_EVENT_ACTION => event.action = String::from(key_val[1]),
                    U_EVENT_DEV_NAME => event.devname = String::from(key_val[1]),
                    U_EVENT_SUB_SYSTEM => event.subsystem = String::from(key_val[1]),
                    U_EVENT_DEV_PATH => event.devpath = String::from(key_val[1]),
                    U_EVENT_SEQ_NUM => event.seqnum = String::from(key_val[1]),
                    U_EVENT_INTERFACE => event.interface = String::from(key_val[1]),
                    _ => (),
                }
            }
        }

        event
    }

    async fn process_add(&self, logger: &Logger, sandbox: &Arc<Mutex<Sandbox>>) {
        // Special case for memory hot-adds first
        let online_path = format!("{}/{}/online", SYSFS_DIR, &self.devpath);
        if online_path.starts_with(SYSFS_MEMORY_ONLINE_PATH) {
            let _ = online_device(online_path.as_ref()).map_err(|e| {
                error!(
                    *logger,
                    "failed to online device";
                    "device" => &self.devpath,
                    "error" => format!("{}", e),
                )
            });
            return;
        }

        let mut sb = sandbox.lock().await;

        // Record the event by sysfs path
        sb.uevent_map
            .insert(self.devpath.clone(), self.clone());

        // Notify watchers that are interested in the udev event.
        for watch in &mut sb.uevent_watchers {
            if let Some((matcher, _)) = watch {
                if matcher.is_match(&self) {

                    let (_, sender) = watch.take().unwrap();
                    let _ = sender.send(self.clone());
                }
            }
        }
    }

    async fn process(&self, logger: &Logger, sandbox: &Arc<Mutex<Sandbox>>) {
        if self.action == U_EVENT_ACTION_ADD {
            return self.process_add(logger, sandbox).await;
        }
        debug!(*logger, "ignoring event"; "uevent" => format!("{:?}", self));
    }
}

pub async fn wait_for_uevent(sandbox: &Arc<Mutex<Sandbox>>, matcher: impl UeventMatcher) -> Result<Uevent> {
    let mut sb = sandbox.lock().await;
    for uev in sb.uevent_map.values() {
        if matcher.is_match(uev) {
            info!(sl!(), "Device {:?} found in pci device map", uev);
            return Ok(uev.clone());
        }
    }

    // If device is not found in the device map, hotplug event has not
    // been received yet, create and add channel to the watchers map.
    // The key of the watchers map is the device we are interested in.
    // Note this is done inside the lock, not to miss any events from the
    // global udev listener.
    let (tx, rx) = tokio::sync::oneshot::channel::<Uevent>();
    let idx = sb.uevent_watchers.len();
    sb.uevent_watchers.push(Some((Box::new(matcher), tx)));
    drop(sb); // unlock

    info!(sl!(), "Waiting on channel for uevent notification\n");
    let hotplug_timeout = AGENT_CONFIG.read().await.hotplug_timeout;

    let uev = match tokio::time::timeout(hotplug_timeout, rx).await {
        Ok(v) => v?,
        Err(_) => {
            let mut sb = sandbox.lock().await;
            sb.uevent_watchers[idx].take();

            return Err(anyhow!("Timeout after {:?} waiting for uevent",
                               hotplug_timeout));
        }
    };

    Ok(uev)
}

pub async fn watch_uevents(sandbox: Arc<Mutex<Sandbox>>) {
    let sref = sandbox.clone();
    let s = sref.lock().await;
    let logger = s.logger.new(o!("subsystem" => "uevent"));

    tokio::spawn(async move {
        let mut socket;
        unsafe {
            let fd = libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                protocols::NETLINK_KOBJECT_UEVENT as libc::c_int,
            );
            socket = TokioSocket::from_raw_fd(fd);
        }
        socket.bind(&SocketAddr::new(0, 1)).unwrap();

        loop {
            match socket.recv_from_full().await {
                Err(e) => {
                    error!(logger, "receive uevent message failed"; "error" => format!("{}", e))
                }
                Ok((buf, addr)) => {
                    if addr.port_number() != 0 {
                        // not our netlink message
                        let err_msg = format!("{:?}", nix::Error::Sys(Errno::EBADMSG));
                        error!(logger, "receive uevent message failed"; "error" => err_msg);
                        return;
                    }

                    let text = String::from_utf8(buf);
                    match text {
                        Err(e) => {
                            error!(logger, "failed to convert bytes to text"; "error" => format!("{}", e))
                        }
                        Ok(text) => {
                            let event = Uevent::new(&text);
                            info!(logger, "got uevent message"; "event" => format!("{:?}", event));
                            event.process(&logger, &sandbox).await;
                        }
                    }
                }
            }
        }
    });
}

// Used from the device module to test specific matchers
#[cfg(test)]
pub(crate) async fn test_wait_for_uevent_helper(uev: Uevent, matcher: impl UeventMatcher+Clone) {
    let devpath = uev.devpath.clone();
    let logger = slog::Logger::root(slog::Discard, o!());
    let sandbox = Arc::new(Mutex::new(Sandbox::new(&logger).unwrap()));

    let mut sb = sandbox.lock().await;
    sb.uevent_map.insert(devpath.clone(), uev.clone());
    drop(sb); // unlock

    let uev1 = wait_for_uevent(&sandbox, matcher.clone()).await;
    assert!(uev1.is_ok(), "{}", uev1.unwrap_err());
    assert_eq!(uev1.unwrap(), uev);

    let mut sb = sandbox.lock().await;
    sb.uevent_map.remove(&devpath).unwrap();
    drop(sb); // unlock

    let watcher_sandbox = Arc::clone(&sandbox);
    let watcher_uev = uev.clone();
    tokio::spawn(async move {
        loop {
            let mut sb = watcher_sandbox.lock().await;
            for w in &mut sb.uevent_watchers {
                if let Some((matcher, _)) = w {
                    if matcher.is_match(&watcher_uev) {
                        let (_, sender) = w.take().unwrap();
                        let _ = sender.send(watcher_uev);
                        return;
                    }
                }
            }
            drop(sb); // unlock
        }
    });

    let uev2 = wait_for_uevent(&sandbox, matcher).await;
    assert!(uev2.is_ok(), "{}", uev2.unwrap_err());
    assert_eq!(uev2.unwrap(), uev);
}
