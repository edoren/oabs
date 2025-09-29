use std::{net::Ipv4Addr, time::Duration};

use futures::TryStreamExt;
use rupnp::{
    Device, Service,
    ssdp::{SearchTarget, URN},
};

const WANIP_CONNECTION_VER_1: URN = URN::service("schemas-upnp-org", "WANIPConnection", 1);
const WANIP_CONNECTION_VER_2: URN = URN::service("schemas-upnp-org", "WANIPConnection", 2);

#[allow(dead_code)]
pub enum PortMappingProtocol {
    TCP,
    UDP,
    BOTH,
}

impl PortMappingProtocol {
    fn get_protocols(&self) -> Vec<&str> {
        match self {
            PortMappingProtocol::TCP => vec!["TCP"],
            PortMappingProtocol::UDP => vec!["UDP"],
            PortMappingProtocol::BOTH => vec!["TCP", "UDP"],
        }
    }
}

pub struct AddPortConfig {
    pub internal_client: Ipv4Addr,
    pub internal_port: u16,
    pub external_port: u16,
    pub protocol: PortMappingProtocol,
    pub enabled: bool,
    pub description: String,
    pub lease_duration: u32,
}

pub struct DeletePortConfig {
    pub external_port: u16,
    pub protocol: PortMappingProtocol,
}

pub enum UPnPError {
    DEFAULT,
}

impl std::fmt::Debug for UPnPError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "UPnPError")
    }
}

impl std::fmt::Display for UPnPError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "UPnPError")
    }
}

impl std::error::Error for UPnPError {
    fn description(&self) -> &str {
        "WOW"
    }
}

pub struct UPnP {
    devices: Vec<Device>,
}

impl UPnP {
    pub async fn new() -> Result<Self, UPnPError> {
        let mut devices = Vec::new();
        for schema in vec![WANIP_CONNECTION_VER_1, WANIP_CONNECTION_VER_2] {
            let search_target = SearchTarget::URN(schema.clone());
            let mut found_devices = rupnp::discover(&search_target, Duration::from_secs(5))
                .await
                .map_err(|_e| UPnPError::DEFAULT)?
                .try_collect::<Vec<Device>>()
                .await
                .map_err(|_e| UPnPError::DEFAULT)?;
            devices.append(&mut found_devices);
        }
        return Ok(UPnP { devices });
    }

    pub async fn add_port(&self, config: &AddPortConfig) -> Result<(), UPnPError> {
        for device in &self.devices {
            let service = self.get_device_service(device)?;

            for protocol in config.protocol.get_protocols() {
                let mappings: Vec<(&str, String)> = Vec::from([
                    ("NewRemoteHost", String::new()),
                    ("NewExternalPort", format!("{}", config.external_port)),
                    ("NewProtocol", format!("{}", protocol)),
                    ("NewInternalPort", format!("{}", config.internal_port)),
                    ("NewInternalClient", format!("{}", config.internal_client)),
                    ("NewEnabled", format!("{}", config.enabled as i8)),
                    (
                        "NewPortMappingDescription",
                        format!("{}", config.description),
                    ),
                    ("NewLeaseDuration", format!("{}", config.lease_duration)),
                ]);

                let mut args = String::new();
                for (variable, value) in mappings {
                    let arg = format!("<{variable}>{value}</{variable}>\n");
                    args += &arg;
                }

                let _ = service.action(device.url(), "AddPortMapping", &args).await;
            }
        }

        Ok(())
    }

    pub async fn delete_port(&self, config: &DeletePortConfig) -> Result<(), UPnPError> {
        for device in &self.devices {
            let service = self.get_device_service(device)?;

            for protocol in config.protocol.get_protocols() {
                let mappings: Vec<(&str, String)> = Vec::from([
                    ("NewRemoteHost", String::new()),
                    ("NewExternalPort", format!("{}", config.external_port)),
                    ("NewProtocol", format!("{}", protocol)),
                ]);

                let mut args = String::new();
                for (variable, value) in mappings {
                    let arg = format!("<{variable}>{value}</{variable}>\n");
                    args += &arg;
                }

                let _ = service
                    .action(device.url(), "DeletePortMapping", &args)
                    .await;
            }
        }

        Ok(())
    }

    pub async fn get_external_ip_address(&self) -> Result<String, UPnPError> {
        for device in &self.devices {
            let service = self.get_device_service(device)?;

            if let Ok(response) = service
                .action(device.url(), "GetExternalIPAddress", "")
                .await
            {
                return Ok(response
                    .get("NewExternalIPAddress")
                    .ok_or(UPnPError::DEFAULT)?
                    .to_string());
            }
        }

        Err(UPnPError::DEFAULT)
    }

    fn get_device_service<'a>(&'a self, device: &'a Device) -> Result<&'a Service, UPnPError> {
        for schema in vec![WANIP_CONNECTION_VER_1, WANIP_CONNECTION_VER_2] {
            if let Some(service) = device.find_service(&schema) {
                return Ok(service);
            }
        }
        Err(UPnPError::DEFAULT)
    }
}
