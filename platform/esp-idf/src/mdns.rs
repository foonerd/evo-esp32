// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The device on the local network's service directory.
//!
//! Espressif's mDNS responder answers for `<hostname>.local` and for
//! the services added here. An Evo device advertises `_evo._tcp` with
//! its control port and a TXT record the framework's discovery reads:
//! `id` (the device's UUID), `name`, `version` and `caps`; a source-host
//! finds a group member by the `id` and dials the address and port the
//! record resolves to.
//!
//! The same responder looks for services other devices advertise: a
//! [`Search`] asks the network for the instances of one (`_snapcast`
//! over `_tcp`) and is polled for the answers, never waited on, so the
//! loop that owns it keeps its cadence.

use std::ffi::CString;
use std::net::IpAddr;
use std::ptr;
use std::time::{Duration, Instant};

use esp_idf_svc::mdns::{EspMdns, QueryResult};
use esp_idf_svc::sys;

/// The responder, alive while this is.
pub struct Advertiser(EspMdns);

/// One instance of a service another device advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The instance's name, as the device named it.
    pub instance: String,
    /// The device's host name, without `.local`.
    pub host: String,
    /// The service's port.
    pub port: u16,
    /// The device's addresses, as the answer carried them.
    pub addrs: Vec<IpAddr>,
}

impl Found {
    /// The first IPv4 address, as `a.b.c.d:port`, if the answer had one.
    pub fn socket_v4(&self) -> Option<String> {
        self.addrs
            .iter()
            .find(|a| a.is_ipv4())
            .map(|a| format!("{a}:{}", self.port))
    }
}

/// A search for a service's instances, running in the responder's task
/// for the time it was given; [`Search::poll`] says when it is done.
pub struct Search {
    handle: *mut sys::mdns_search_once_t,
    ends: Instant,
    done: bool,
}

impl Search {
    /// The instances found, once the search has run its time; `None`
    /// while it runs. Asked again after it answered, `Some` and empty.
    pub fn poll(&mut self) -> Option<Vec<Found>> {
        if self.done {
            return Some(Vec::new());
        }
        let mut results: *mut sys::mdns_result_t = ptr::null_mut();
        let mut count = 0u8;
        // Timeout 0: the semaphore is taken if the search is over, and
        // the call returns at once either way.
        let finished = unsafe {
            sys::mdns_query_async_get_results(
                self.handle,
                0,
                &mut results,
                &mut count,
            )
        };
        if !finished {
            return None;
        }
        self.done = true;
        let mut found = Vec::with_capacity(count as usize);
        let mut p = results;
        while !p.is_null() {
            let r = QueryResult::from(unsafe { *p });
            found.push(Found {
                instance: r.instance_name.unwrap_or_default(),
                host: r.hostname.unwrap_or_default(),
                port: r.port,
                addrs: r.addr,
            });
            p = unsafe { (*p).next };
        }
        if !results.is_null() {
            unsafe { sys::mdns_query_results_free(results) };
        }
        Some(found)
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        // The handle may only be deleted once the search has ended: one
        // dropped early waits out what is left of its time (bounded by
        // the timeout it was started with) and frees what it found.
        if !self.done {
            let left = self.ends.saturating_duration_since(Instant::now());
            let mut results: *mut sys::mdns_result_t = ptr::null_mut();
            let finished = unsafe {
                sys::mdns_query_async_get_results(
                    self.handle,
                    left.as_millis() as u32 + 50,
                    &mut results,
                    ptr::null_mut(),
                )
            };
            if finished && !results.is_null() {
                unsafe { sys::mdns_query_results_free(results) };
            }
        }
        unsafe { sys::mdns_query_async_delete(self.handle) };
    }
}

impl Advertiser {
    /// Start answering as `hostname` (`hostname.local`), with `instance`
    /// as the human-readable name of the device's services.
    pub fn start(hostname: &str, instance: &str) -> Result<Self, String> {
        let mut mdns = EspMdns::take().map_err(|e| format!("mdns: {e}"))?;
        mdns.set_hostname(hostname)
            .map_err(|e| format!("mdns hostname: {e}"))?;
        mdns.set_instance_name(instance)
            .map_err(|e| format!("mdns instance: {e}"))?;
        Ok(Self(mdns))
    }

    /// Advertise `service` (`_evo`) over `proto` (`_tcp`) on `port`
    /// with `txt`.
    pub fn add_service(
        &mut self,
        service: &str,
        proto: &str,
        port: u16,
        txt: &[(&str, &str)],
    ) -> Result<(), String> {
        self.0
            .add_service(None, service, proto, port, txt)
            .map_err(|e| format!("mdns {service}: {e}"))
    }

    /// Start looking for the instances of `service` (`_snapcast`) over
    /// `proto` (`_tcp`), up to `most` of them, for `timeout`; the search
    /// answers through [`Search::poll`]. One search at a time is enough
    /// for a device; several may run.
    pub fn search(
        &self,
        service: &str,
        proto: &str,
        timeout: Duration,
        most: usize,
    ) -> Result<Search, String> {
        let service_c = CString::new(service)
            .map_err(|_| format!("mdns search: {service:?}"))?;
        let proto_c = CString::new(proto)
            .map_err(|_| format!("mdns search: {proto:?}"))?;
        let handle = unsafe {
            sys::mdns_query_async_new(
                ptr::null(),
                service_c.as_ptr(),
                proto_c.as_ptr(),
                sys::MDNS_TYPE_PTR as u16,
                timeout.as_millis() as u32,
                most,
                None,
            )
        };
        if handle.is_null() {
            return Err(format!(
                "mdns search {service}.{proto}: the responder refused it"
            ));
        }
        Ok(Search {
            handle,
            ends: Instant::now() + timeout,
            done: false,
        })
    }
}
