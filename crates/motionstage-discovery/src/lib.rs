use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use motionstage_protocol::{Feature, PROTOCOL_MAJOR, PROTOCOL_MINOR};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;
use thiserror::Error;

pub const SERVICE_TYPE: &str = "_motionstage._udp.local.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryAdvertisement {
    pub service_name: String,
    pub bind_host: String,
    pub bind_port: u16,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub features: Vec<Feature>,
    pub security_mode: String,
}

impl DiscoveryAdvertisement {
    pub fn to_txt_records(&self) -> Vec<String> {
        vec![
            format!("name={}", self.service_name),
            format!("proto_major={}", self.protocol_major),
            format!("proto_minor={}", self.protocol_minor),
            format!("security={}", self.security_mode),
            format!(
                "features={}",
                self.features
                    .iter()
                    .map(|f| format!("{f:?}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ]
    }

    pub fn default_for(name: impl Into<String>, port: u16) -> Self {
        Self {
            service_name: name.into(),
            bind_host: "0.0.0.0".into(),
            bind_port: port,
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            features: vec![
                Feature::Motion,
                Feature::Mapping,
                Feature::Recording,
                Feature::Video,
                Feature::Hdr10,
                Feature::SdrFallback,
            ],
            security_mode: "trusted_lan".into(),
        }
    }

    fn instance_name(&self) -> String {
        format!("{}.{}", self.service_name, SERVICE_TYPE)
    }
}

pub struct DiscoveryPublisher {
    daemon: ServiceDaemon,
    full_name: String,
}

impl DiscoveryPublisher {
    pub fn advertise(advertisement: &DiscoveryAdvertisement) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new().map_err(|err| DiscoveryError::Mdns(err.to_string()))?;

        let txt_records = advertisement.to_txt_records();
        let mut txt_map: HashMap<String, String> = HashMap::new();
        for entry in txt_records {
            if let Some((k, v)) = entry.split_once('=') {
                txt_map.insert(k.to_string(), v.to_string());
            }
        }
        let full_name = advertisement.instance_name();
        let host_name = local_hostname_for_service(&advertisement.service_name);

        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &advertisement.service_name,
            &host_name,
            advertisement.bind_host.as_str(),
            advertisement.bind_port,
            Some(txt_map),
        )
        .map_err(|err| DiscoveryError::Mdns(err.to_string()))?;

        daemon
            .register(service)
            .map_err(|err| DiscoveryError::Mdns(err.to_string()))?;

        Ok(Self { daemon, full_name })
    }

    pub fn stop(self) -> Result<(), DiscoveryError> {
        self.daemon
            .unregister(&self.full_name)
            .map_err(|err| DiscoveryError::Mdns(err.to_string()))?;
        let _ = self
            .daemon
            .shutdown()
            .map_err(|err| DiscoveryError::Mdns(err.to_string()))?;
        Ok(())
    }
}

fn local_hostname_for_service(service_name: &str) -> String {
    let normalized = service_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();

    let label = if normalized.is_empty() {
        "motionstage"
    } else {
        normalized.as_str()
    };
    format!("{label}.local.")
}

pub struct DiscoveryBrowser {
    daemon: ServiceDaemon,
    receiver: mdns_sd::Receiver<ServiceEvent>,
}

impl DiscoveryBrowser {
    pub fn start() -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new().map_err(|err| DiscoveryError::Mdns(err.to_string()))?;
        let receiver = daemon
            .browse(SERVICE_TYPE)
            .map_err(|err| DiscoveryError::Mdns(err.to_string()))?;
        Ok(Self { daemon, receiver })
    }

    pub fn recv(&self) -> Result<ServiceEvent, DiscoveryError> {
        self.receiver
            .recv()
            .map_err(|err| DiscoveryError::Mdns(err.to_string()))
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<ServiceEvent>, DiscoveryError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(flume::RecvTimeoutError::Timeout) => Ok(None),
            Err(flume::RecvTimeoutError::Disconnected) => {
                Err(DiscoveryError::Mdns("browse channel disconnected".into()))
            }
        }
    }

    pub fn recv_service_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Option<DiscoveredService>, DiscoveryError> {
        let Some(event) = self.recv_timeout(timeout)? else {
            return Ok(None);
        };
        match event {
            ServiceEvent::ServiceResolved(info) => Ok(Some(DiscoveredService::from_info(&info))),
            _ => Ok(None),
        }
    }

    pub fn stop(self) -> Result<(), DiscoveryError> {
        let _ = self
            .daemon
            .shutdown()
            .map_err(|err| DiscoveryError::Mdns(err.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("mdns error: {0}")]
    Mdns(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredService {
    pub service_name: String,
    pub fullname: String,
    pub host_name: String,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    pub protocol_major: Option<u16>,
    pub protocol_minor: Option<u16>,
}

impl DiscoveredService {
    fn from_info(info: &ServiceInfo) -> Self {
        let mut addresses: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
        addresses.sort_by_key(|ip| ip.to_string());

        let service_name = info
            .get_property_val_str("name")
            .map(str::to_owned)
            .unwrap_or_else(|| service_name_from_fullname(info.get_fullname()));

        let protocol_major = info
            .get_property_val_str("proto_major")
            .and_then(|v| v.parse::<u16>().ok());
        let protocol_minor = info
            .get_property_val_str("proto_minor")
            .and_then(|v| v.parse::<u16>().ok());

        Self {
            service_name,
            fullname: info.get_fullname().to_owned(),
            host_name: info.get_hostname().to_owned(),
            addresses,
            port: info.get_port(),
            protocol_major,
            protocol_minor,
        }
    }
}

fn service_name_from_fullname(fullname: &str) -> String {
    fullname
        .strip_suffix(SERVICE_TYPE)
        .unwrap_or(fullname)
        .trim_end_matches('.')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        service_name_from_fullname, DiscoveredService, DiscoveryAdvertisement, SERVICE_TYPE,
    };
    use mdns_sd::ServiceInfo;
    use std::collections::HashMap;

    #[test]
    fn txt_records_include_version_and_security() {
        let adv = DiscoveryAdvertisement::default_for("cine", 7788);
        let txt = adv.to_txt_records();
        assert!(txt.iter().any(|s| s.contains("proto_major=1")));
        assert!(txt.iter().any(|s| s.contains("security=trusted_lan")));
    }

    #[test]
    fn service_name_is_derived_from_fullname_when_txt_name_missing() {
        assert_eq!(
            service_name_from_fullname("motionstage-blender._motionstage._udp.local."),
            "motionstage-blender"
        );
    }

    #[test]
    fn discovered_service_reads_txt_metadata() {
        let mut txt = HashMap::new();
        txt.insert("name".to_owned(), "motionstage-blender".to_owned());
        txt.insert("proto_major".to_owned(), "1".to_owned());
        txt.insert("proto_minor".to_owned(), "2".to_owned());
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "motionstage-blender",
            "motionstage-blender.local.",
            "127.0.0.1",
            7788,
            Some(txt),
        )
        .unwrap();

        let discovered = DiscoveredService::from_info(&info);
        assert_eq!(discovered.service_name, "motionstage-blender");
        assert_eq!(discovered.port, 7788);
        assert_eq!(discovered.protocol_major, Some(1));
        assert_eq!(discovered.protocol_minor, Some(2));
    }
}
