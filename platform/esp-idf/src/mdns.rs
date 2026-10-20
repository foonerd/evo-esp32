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

use esp_idf_svc::mdns::EspMdns;

/// The responder, alive while this is.
pub struct Advertiser(EspMdns);

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
}
