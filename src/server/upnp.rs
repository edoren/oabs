use std::{net::Ipv4Addr, time::Duration};

use anyhow::{anyhow, Result};
use futures::{pin_mut, TryStreamExt};
use rupnp::{
    ssdp::{SearchTarget, URN},
    Device,
};

const WANIP_CONNECTION: URN = URN::service("schemas-upnp-org", "WANIPConnection", 1);

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

pub struct UPnP {
    devices: Vec<Device>,
}

impl UPnP {
    pub async fn new() -> Result<Self> {
        let search_target = SearchTarget::URN(WANIP_CONNECTION);
        let devices = rupnp::discover(&search_target, Duration::from_secs(3)).await?;
        pin_mut!(devices);
        return Ok(UPnP {
            devices: devices.try_collect::<Vec<Device>>().await?,
        });
    }

    pub async fn add_port(&self, config: &AddPortConfig) -> Result<()> {
        for device in &self.devices {
            let service = device
                .find_service(&WANIP_CONNECTION)
                .expect("searched for RenderingControl, got something else");

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

    pub async fn delete_port(&self, config: &DeletePortConfig) -> Result<()> {
        for device in &self.devices {
            let service = device
                .find_service(&WANIP_CONNECTION)
                .expect("searched for RenderingControl, got something else");

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

    pub async fn get_external_ip_address(&self) -> Result<String> {
        for device in &self.devices {
            let service = device
                .find_service(&WANIP_CONNECTION)
                .expect("searched for RenderingControl, got something else");

            if let Ok(response) = service
                .action(device.url(), "GetExternalIPAddress", "")
                .await
            {
                return Ok(response
                    .get("NewExternalIPAddress")
                    .ok_or(anyhow!("Could not get external IP"))?
                    .to_string());
            }
        }

        Err(anyhow!("Could not get an external ip"))
    }
}
