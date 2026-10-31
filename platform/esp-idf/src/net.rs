//! The Wi-Fi, over the driver's interfaces.
//!
//! Wraps the blocking Wi-Fi so the firmware has one thing to start,
//! connect and read the station's address and hostname from; a joined
//! access point is remembered in the [`NvsStore`] so the next boot goes
//! straight to it.
//!
//! Written for the Volumio ESP companion (MIT) and carried here as it
//! was, less that project's status-page trait.

use crate::nvs::NvsStore;
use core::time::Duration;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::ipv4::{
    ClientConfiguration as IpClientConfiguration,
    Configuration as IpConfiguration, DHCPClientSettings,
};
use esp_idf_svc::netif::{EspNetif, NetifConfiguration, NetifStack};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::{self, EspError, ESP_ERR_INVALID_ARG};
use esp_idf_svc::wifi::{
    AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi,
    ScanMethod, WifiDriver,
};
use std::time::Instant;

/// The Wi-Fi, started, as the application's view of the network.
pub struct WifiNet<'d> {
    wifi: BlockingWifi<EspWifi<'d>>,
}

/// Where a station last found its access point. Offered back to
/// [`WifiNet::connect`], it lets the driver go straight to that channel and
/// BSSID instead of searching both bands, which is most of a cold boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApHint {
    /// The primary channel.
    pub channel: u8,
    /// The access point's MAC.
    pub bssid: [u8; 6],
}

/// The bands the station searches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band {
    /// 2.4 GHz only.
    TwoPointFour,
    /// 5 GHz only.
    Five,
    /// Both, the driver's default.
    Both,
}

impl Band {
    /// The band a channel number is on.
    pub fn of_channel(channel: u8) -> Band {
        if channel > 14 {
            Band::Five
        } else {
            Band::TwoPointFour
        }
    }

    /// Whether a station searching `self` may join on `other`.
    pub fn allows(self, other: Band) -> bool {
        self == Band::Both || self == other
    }

    fn mode(self) -> sys::wifi_band_mode_t {
        match self {
            Band::TwoPointFour => sys::wifi_band_mode_t_WIFI_BAND_MODE_2G_ONLY,
            Band::Five => sys::wifi_band_mode_t_WIFI_BAND_MODE_5G_ONLY,
            Band::Both => sys::wifi_band_mode_t_WIFI_BAND_MODE_AUTO,
        }
    }
}

/// How long a full search may take before `connect` gives up. The
/// driver's own figure for both bands is around thirteen seconds.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a hinted join may take: the AP is either there or it has
/// moved, and the caller falls back to the search.
const HINTED_TIMEOUT: Duration = Duration::from_secs(10);

/// Store keys for the remembered access point: where the station last
/// found its network, so the next boot goes straight there.
const HINT_SSID: &str = "hint_ssid";
const HINT_CHANNEL: &str = "hint_channel";
const HINT_BSSID: &str = "hint_bssid";

impl<'d> WifiNet<'d> {
    /// Take the modem, start the driver in station mode with no network
    /// yet. Scanning works from here; `connect` joins one. `hostname` is
    /// what the station tells DHCP, so the router's device list shows it;
    /// `None` keeps ESP-IDF's default (`espressif`). Nothing answers for
    /// it as `.local` until an mDNS responder is built in. Longer than the
    /// netif accepts (30) is `ESP_ERR_INVALID_ARG`.
    pub fn start(
        modem: Modem<'d>,
        sys_loop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
        hostname: Option<&str>,
    ) -> Result<Self, EspError> {
        let driver = WifiDriver::new(modem, sys_loop.clone(), Some(nvs))?;
        let sta = match hostname {
            Some(name) => {
                let name = name.try_into().map_err(|_| {
                    EspError::from_infallible::<ESP_ERR_INVALID_ARG>()
                })?;
                EspNetif::new_with_conf(&NetifConfiguration {
                    ip_configuration: Some(IpConfiguration::Client(
                        IpClientConfiguration::DHCP(DHCPClientSettings {
                            hostname: Some(name),
                        }),
                    )),
                    ..NetifConfiguration::wifi_default_client()
                })?
            }
            None => EspNetif::new(NetifStack::Sta)?,
        };
        let wifi =
            EspWifi::wrap_all(driver, sta, EspNetif::new(NetifStack::Ap)?)?;
        let mut wifi = BlockingWifi::wrap(wifi, sys_loop)?;
        wifi.set_configuration(&Configuration::Client(
            ClientConfiguration::default(),
        ))?;
        wifi.start()?;
        Ok(Self { wifi })
    }

    /// Join `ssid` and wait for an address. An empty password is an open
    /// network. Without `hint` the driver scans every channel on both bands
    /// and joins the strongest match, which takes it about thirteen seconds
    /// and is worth it once. With `hint` it scans from that channel for
    /// that BSSID and stops at the first sight of it, in about a second;
    /// after [`HINTED_TIMEOUT`] it has moved and the caller falls back to
    /// the search. A failed hinted join leaves the driver ready for one.
    pub fn connect(
        &mut self,
        ssid: &str,
        password: &str,
        hint: Option<ApHint>,
    ) -> Result<(), EspError> {
        let auth = if password.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        };
        let mut conf = ClientConfiguration {
            auth_method: auth,
            ..Default::default()
        };
        // heapless strings: a name too long for the driver is not a name.
        if conf.ssid.push_str(ssid).is_err()
            || conf.password.push_str(password).is_err()
        {
            return Err(EspError::from_infallible::<{ ESP_ERR_INVALID_ARG }>());
        }
        let timeout = match hint {
            Some(h) => {
                conf.channel = Some(h.channel);
                conf.bssid = Some(h.bssid);
                // The default is a complete scan of both bands, the thirteen
                // seconds the hint exists to save.
                conf.scan_method = ScanMethod::FastScan;
                HINTED_TIMEOUT
            }
            None => SEARCH_TIMEOUT,
        };
        // A previous attempt may still be pending in the driver.
        let _ = self.wifi.wifi_mut().disconnect();
        self.wifi.set_configuration(&Configuration::Client(conf))?;
        self.wifi.wifi_mut().connect()?;
        self.wifi.wifi_wait_while(
            || self.wifi.wifi().is_connected().map(|c| !c),
            Some(timeout),
        )?;
        self.wifi.wait_netif_up()
    }

    /// Join `ssid` the quick way when the store remembers where it was
    /// (one band, one channel, one BSSID: about a second), else by the
    /// full search; then remember where it landed for next time. Only
    /// `band` is searched, and a hint on another band is not followed:
    /// a network on both bands is otherwise joined on whichever the
    /// first search found strongest and remembered from then on. The
    /// band is left on `band` afterwards.
    pub fn join(
        &mut self,
        store: &mut NvsStore,
        ssid: &str,
        password: &str,
        band: Band,
    ) -> Result<(), EspError> {
        let remembered = read_hint(store, ssid);
        let hinted = remembered
            .filter(|h| band.allows(Band::of_channel(h.channel)))
            .and_then(|h| {
                self.set_band(Band::of_channel(h.channel));
                let started = Instant::now();
                let outcome = self.connect(ssid, password, Some(h));
                self.set_band(band);
                match outcome {
                    Ok(()) => {
                        log::info!(
                            "wifi: joined by hint {h:?} in {} ms",
                            started.elapsed().as_millis()
                        );
                        Some(())
                    }
                    Err(e) => {
                        log::info!(
                            "wifi: hint {h:?} did not answer ({e}); searching"
                        );
                        None
                    }
                }
            });
        if hinted.is_none() {
            self.set_band(band);
            let started = Instant::now();
            self.connect(ssid, password, None)?;
            log::info!(
                "wifi: joined by search in {} ms",
                started.elapsed().as_millis()
            );
        }
        if let Some(now) = self.joined_ap() {
            if remembered != Some(now) {
                write_hint(store, ssid, now);
            }
        }
        Ok(())
    }

    /// Restrict the search to `band`, or open it to both. Takes effect
    /// on the next connect; a failure is logged, not fatal.
    #[allow(unsafe_code)]
    pub fn set_band(&mut self, band: Band) {
        let mode = band.mode();
        // SAFETY: a plain setter on the started driver; `mode` is one of
        // the enum's values.
        let rc = unsafe { sys::esp_wifi_set_band_mode(mode) };
        if rc != sys::ESP_OK {
            log::warn!("wifi: esp_wifi_set_band_mode({mode}) returned {rc}");
        }
    }

    /// The access point the station is joined to, as a hint for next time.
    pub fn joined_ap(&self) -> Option<ApHint> {
        self.wifi.wifi().get_ap_info().ok().map(|ap| ApHint {
            channel: ap.channel,
            bssid: ap.bssid,
        })
    }

    /// The station's address, when it has one.
    pub fn address(&self) -> Option<String> {
        if !self.wifi.is_up().unwrap_or(false) {
            return None;
        }
        let info = self.wifi.wifi().sta_netif().get_ip_info().ok()?;
        (!info.ip.is_unspecified()).then(|| info.ip.to_string())
    }

    /// The hostname the station told DHCP.
    pub fn hostname(&self) -> String {
        self.wifi
            .wifi()
            .sta_netif()
            .get_hostname()
            .map(|h| h.as_str().to_string())
            .unwrap_or_default()
    }

    /// The driver, for what the application does not need: scans, band
    /// mode, the raw interfaces.
    pub fn wifi(&self) -> &BlockingWifi<EspWifi<'d>> {
        &self.wifi
    }

    /// The driver, mutably.
    pub fn wifi_mut(&mut self) -> &mut BlockingWifi<EspWifi<'d>> {
        &mut self.wifi
    }
}

fn read_hint(store: &NvsStore, ssid: &str) -> Option<ApHint> {
    let get = |k: &str| store.get(k).ok().flatten();
    if get(HINT_SSID)?.as_str() != ssid {
        return None;
    }
    let channel = get(HINT_CHANNEL)?.parse().ok()?;
    let bssid_text = get(HINT_BSSID)?;
    let mut bssid = [0u8; 6];
    let mut parts = bssid_text.split(':');
    for b in &mut bssid {
        *b = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(ApHint { channel, bssid })
}

fn write_hint(store: &mut NvsStore, ssid: &str, hint: ApHint) {
    let b = hint.bssid;
    let bssid = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5]
    );
    let result = store
        .set(HINT_SSID, ssid)
        .and_then(|()| store.set(HINT_CHANNEL, &hint.channel.to_string()))
        .and_then(|()| store.set(HINT_BSSID, &bssid));
    match result {
        Ok(()) => log::info!(
            "wifi: remembered ch {} {bssid} for '{ssid}'",
            hint.channel
        ),
        Err(e) => {
            log::warn!("wifi: could not remember the access point: {e:?}")
        }
    }
}
