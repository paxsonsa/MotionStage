use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use motionstage_protocol::{Feature, PROTOCOL_MAJOR, PROTOCOL_MINOR};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &advertisement.service_name,
            &advertisement.bind_host,
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

#[cfg(test)]
mod tests {
    use super::DiscoveryAdvertisement;

    #[test]
    fn txt_records_include_version_and_security() {
        let adv = DiscoveryAdvertisement::default_for("cine", 7788);
        let txt = adv.to_txt_records();
        assert!(txt.iter().any(|s| s.contains("proto_major=1")));
        assert!(txt.iter().any(|s| s.contains("security=trusted_lan")));
    }
}
