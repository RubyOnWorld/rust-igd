use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::fmt;
use std;
use rand::distributions::IndependentSample;

use hyper;
use xmltree;
use futures::{Future};
use tokio_core::reactor::{Core,Handle};
use rand;
use soap;

use errors::{RequestError,GetExternalIpError,AddPortError,AddAnyPortError,RemovePortError};

/// Represents the protocols available for port mapping.
#[derive(Debug,Clone,Copy,PartialEq)]
pub enum PortMappingProtocol {
    /// TCP protocol
    TCP,
    /// UDP protocol
    UDP,
}

impl fmt::Display for PortMappingProtocol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", match *self {
            PortMappingProtocol::TCP => "TCP",
            PortMappingProtocol::UDP => "UDP",
        })
    }
}

/// This structure represents a gateway found by the search functions.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Gateway {
    /// Socket address of the gateway
    pub addr: SocketAddrV4,
    /// Control url of the device
    pub control_url: String,
}

impl Gateway {
    fn perform_request(&self, header: &str, body: &str, ok: &str) -> Result<(String, xmltree::Element), RequestError> {
        let url = format!("{}", self);
        let text = try!(soap::send(&url, soap::Action::new(header), body));
        parse_response(text, ok)
    }

    fn perform_request_async(&self, header: &str, body: &str, ok: &str, handle: &Handle) -> Box<Future<Item=(String, xmltree::Element), Error=RequestError>> {
        let url = format!("{}", self);
        let ok = ok.to_owned();
        let future = soap::send_async(&url, soap::Action::new(header), body, handle)
            .map_err(|err| RequestError::from(err) )
            .and_then(move |text| parse_response(text, &ok) );
        Box::new(future)
    }

    /// Get the external IP address of the gateway.
    pub fn get_external_ip(&self) -> Result<Ipv4Addr, GetExternalIpError> {
        let mut core = Core::new()?;
        let handle = core.handle();
        self.get_external_ip_async(&handle).wait()
    }

    /// Get the external IP address of the gateway in a tokio compatible way
    pub fn get_external_ip_async(&self, handle: &Handle) -> Box<Future<Item=Ipv4Addr, Error=GetExternalIpError>> {
        let header = "\"urn:schemas-upnp-org:service:WANIPConnection:1#GetExternalIPAddress\"";
        let body = "<?xml version=\"1.0\"?>
        <SOAP-ENV:Envelope SOAP-ENV:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\" xmlns:SOAP-ENV=\"http://schemas.xmlsoap.org/soap/envelope/\">
            <SOAP-ENV:Body>
                <m:GetExternalIPAddress xmlns:m=\"urn:schemas-upnp-org:service:WANIPConnection:1\">
                </m:GetExternalIPAddress>
            </SOAP-ENV:Body>
        </SOAP-ENV:Envelope>";
        let future = self.perform_request_async(header, body, "GetExternalIPAddressResponse", handle)
            .then(|result| {
                match result {
                    Ok((text, response)) => {
                        match response.get_child("NewExternalIPAddress")
                            .and_then(|e| e.text.as_ref())
                            .and_then(|t| t.parse::<Ipv4Addr>().ok())
                        {
                            Some(ipv4_addr) => Ok(ipv4_addr),
                            None => Err(GetExternalIpError::RequestError(RequestError::InvalidResponse(text))),
                        }
                    },
                    Err(RequestError::ErrorCode(606, _)) => Err(GetExternalIpError::ActionNotAuthorized),
                    Err(e) => Err(GetExternalIpError::RequestError(e)),
                }
            });
        Box::new(future)
    }

    /// Get an external socket address with our external ip and any port. This is a convenience
    /// function that calls `get_external_ip` followed by `add_any_port`
    ///
    /// The local_addr is the address where the traffic is sent to.
    /// The lease_duration parameter is in seconds. A value of 0 is infinite.
    ///
    /// # Returns
    ///
    /// The external address that was mapped on success. Otherwise an error.
    pub fn get_any_address(&self,
                           protocol: PortMappingProtocol,
                           local_addr: SocketAddrV4,
                           lease_duration: u32,
                           description: &str)
            -> Result<SocketAddrV4, AddAnyPortError>
    {
        let external_ip = match self.get_external_ip() {
            Ok(ip) => ip,
            Err(GetExternalIpError::ActionNotAuthorized)
                => return Err(AddAnyPortError::ActionNotAuthorized),
            Err(GetExternalIpError::RequestError(e))
                => return Err(AddAnyPortError::RequestError(e)),
        };
        let external_port = try!(self.add_any_port(protocol,
                                                   local_addr,
                                                   lease_duration,
                                                   description));
        Ok(SocketAddrV4::new(external_ip, external_port))
    }
                        

    /// Add a port mapping.with any external port.
    ///
    /// The local_addr is the address where the traffic is sent to.
    /// The lease_duration parameter is in seconds. A value of 0 is infinite.
    ///
    /// # Returns
    ///
    /// The external port that was mapped on success. Otherwise an error.
    pub fn add_any_port(&self, protocol: PortMappingProtocol,
                        local_addr: SocketAddrV4,
                        lease_duration: u32, description: &str)
            -> Result<u16, AddAnyPortError>
    {
        // This function first attempts to call AddAnyPortMapping on the IGD with a random port
        // number. If that fails due to the method being unknown it attempts to call AddPortMapping
        // instead with a random port number. If that fails due to ConflictInMappingEntry it retrys
        // with another port up to a maximum of 20 times. If it fails due to SamePortValuesRequired
        // it retrys once with the same port values.

        if local_addr.port() == 0 {
            return Err(AddAnyPortError::InternalPortZeroInvalid)
        }

        let port_range = rand::distributions::Range::new(32768u16, 65535u16);
        let mut rng = rand::thread_rng();
        let external_port = port_range.ind_sample(&mut rng);

        let header = "\"urn:schemas-upnp-org:service:WANIPConnection:1#AddAnyPortMapping\"";
        let body = format!("<?xml version=\"1.0\"?>
        <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">
        <s:Body>
            <u:AddAnyPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">
                <NewProtocol>{}</NewProtocol>
                <NewExternalPort>{}</NewExternalPort>
                <NewInternalClient>{}</NewInternalClient>
                <NewInternalPort>{}</NewInternalPort>
                <NewLeaseDuration>{}</NewLeaseDuration>
                <NewPortMappingDescription>{}</NewPortMappingDescription>
                <NewEnabled>1</NewEnabled>
                <NewRemoteHost></NewRemoteHost>
            </u:AddAnyPortMapping>
        </s:Body>
        </s:Envelope>
        ", protocol, external_port, local_addr.ip(),
           local_addr.port(), lease_duration, description);
        // First, attempt to call the AddAnyPortMapping method.
        match self.perform_request(header, &*body, "AddAnyPortMappingResponse") {
            Ok((text, response)) => {
                match response.get_child("NewReservedPort")
                              .and_then(|e| e.text.as_ref())
                              .and_then(|t| t.parse::<u16>().ok())
                {
                    Some(port) => Ok(port),
                    None => Err(AddAnyPortError::RequestError(RequestError::InvalidResponse(text))),
                }
            }
            // The router doesn't know the AddAnyPortMapping method. Try using AddPortMapping
            // instead.
            Err(RequestError::ErrorCode(401, _)) => {
                // Try a bunch of random ports.
                for _attempt in 0..20 {
                    let external_port = port_range.ind_sample(&mut rng);
                    match self.add_port_mapping(protocol, external_port, local_addr, lease_duration, description) {
                        Ok(()) => return Ok(external_port),
                        Err(RequestError::ErrorCode(605, _)) => return Err(AddAnyPortError::DescriptionTooLong),
                        Err(RequestError::ErrorCode(606, _)) => return Err(AddAnyPortError::ActionNotAuthorized),
                        // That port is in use. Try another.
                        Err(RequestError::ErrorCode(718, _)) => continue,
                        // The router requires that internal and external ports are the same.
                        Err(RequestError::ErrorCode(724, _)) => {
                            return match self.add_port_mapping(protocol, local_addr.port(), local_addr, lease_duration, description) {
                                Ok(()) => Ok(local_addr.port()),
                                Err(RequestError::ErrorCode(606, _)) => Err(AddAnyPortError::ActionNotAuthorized),
                                Err(RequestError::ErrorCode(718, _)) => Err(AddAnyPortError::ExternalPortInUse),
                                Err(RequestError::ErrorCode(725, _)) => Err(AddAnyPortError::OnlyPermanentLeasesSupported),
                                Err(e) => Err(AddAnyPortError::RequestError(e)),
                            }
                        },
                        Err(RequestError::ErrorCode(725, _)) => return Err(AddAnyPortError::OnlyPermanentLeasesSupported),
                        Err(e) => return Err(AddAnyPortError::RequestError(e)),
                    }
                }
                // The only way we can get here is if the router kept returning 718 (port in use)
                // for all the ports we tried.
                Err(AddAnyPortError::NoPortsAvailable)
            },
            Err(RequestError::ErrorCode(605, _)) => Err(AddAnyPortError::DescriptionTooLong),
            Err(RequestError::ErrorCode(606, _)) => Err(AddAnyPortError::ActionNotAuthorized),
            Err(RequestError::ErrorCode(728, _)) => Err(AddAnyPortError::NoPortsAvailable),
            Err(e) => Err(AddAnyPortError::RequestError(e)),
        }
    }

    fn add_port_mapping(&self, protocol: PortMappingProtocol,
                        external_port: u16, local_addr: SocketAddrV4, lease_duration: u32,
                        description: &str) -> Result<(), RequestError> {

        let header = "\"urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping\"";
        let body = format!("<?xml version=\"1.0\"?>
        <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">
        <s:Body>
            <u:AddPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">
                <NewProtocol>{}</NewProtocol>
                <NewExternalPort>{}</NewExternalPort>
                <NewInternalClient>{}</NewInternalClient>
                <NewInternalPort>{}</NewInternalPort>
                <NewLeaseDuration>{}</NewLeaseDuration>
                <NewPortMappingDescription>{}</NewPortMappingDescription>
                <NewEnabled>1</NewEnabled>
                <NewRemoteHost></NewRemoteHost>
            </u:AddPortMapping>
        </s:Body>
        </s:Envelope>
        ", protocol, external_port, local_addr.ip(),
           local_addr.port(), lease_duration, description);
        try!(self.perform_request(header, &*body, "AddPortMappingResponse"));
        Ok(())
    }

    /// Add a port mapping.
    ///
    /// The local_addr is the address where the traffic is sent to.
    /// The lease_duration parameter is in seconds. A value of 0 is infinite.
    pub fn add_port(&self, protocol: PortMappingProtocol,
                    external_port: u16, local_addr: SocketAddrV4, lease_duration: u32,
                    description: &str) -> Result<(), AddPortError> {
        if external_port == 0 {
            return Err(AddPortError::ExternalPortZeroInvalid);
        }
        if local_addr.port() == 0 {
            return Err(AddPortError::InternalPortZeroInvalid);
        }
        match self.add_port_mapping(protocol, external_port, local_addr, lease_duration, description) {
            Ok(()) => Ok(()),
            Err(RequestError::ErrorCode(605, _)) => Err(AddPortError::DescriptionTooLong),
            Err(RequestError::ErrorCode(606, _)) => Err(AddPortError::ActionNotAuthorized),
            Err(RequestError::ErrorCode(718, _)) => Err(AddPortError::PortInUse),
            Err(RequestError::ErrorCode(724, _)) => Err(AddPortError::SamePortValuesRequired),
            Err(RequestError::ErrorCode(725, _)) => Err(AddPortError::OnlyPermanentLeasesSupported),
            Err(e) => Err(AddPortError::RequestError(e)),
        }
    }

    /// Remove a port mapping.
    pub fn remove_port(&self, protocol: PortMappingProtocol,
                       external_port: u16) -> Result<(), RemovePortError> {
        let header = "\"urn:schemas-upnp-org:service:WANIPConnection:1#DeletePortMapping\"";
        let body = format!("<?xml version=\"1.0\"?>
        <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">
        <s:Body>
            <u:DeletePortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">
                <NewProtocol>{}</NewProtocol>
                <NewExternalPort>{}</NewExternalPort>
                <NewRemoteHost></NewRemoteHost>
            </u:DeletePortMapping>
        </s:Body>
        </s:Envelope>
        ", protocol, external_port);

        match self.perform_request(header, &*body, "DeletePortMappingResponse") {
            Ok(..) => Ok(()),
            Err(RequestError::ErrorCode(606, _)) => Err(RemovePortError::ActionNotAuthorized),
            Err(RequestError::ErrorCode(714, _)) => Err(RemovePortError::NoSuchPortMapping),
            Err(e) => Err(RemovePortError::RequestError(e)),
        }
    }
}

impl fmt::Display for Gateway {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "http://{}{}", self.addr, self.control_url)
    }
}

fn parse_response(text: String, ok: &str) -> Result<(String, xmltree::Element), RequestError> {
    let mut xml = match xmltree::Element::parse(text.as_bytes()) {
        Ok(xml) => xml,
        Err(..) => return Err(RequestError::InvalidResponse(text)),
    };
    let mut body = match xml.get_mut_child("Body")
    {
        Some(body) => body,
        None => return Err(RequestError::InvalidResponse(text)),
    };
    if let Some(ok) = body.take_child(ok) {
        return Ok((text, ok))
    }
    let upnp_error = match body.get_child("Fault")
        .and_then(|e| e.get_child("detail"))
        .and_then(|e| e.get_child("UPnPError"))
    {
        Some(upnp_error) => upnp_error,
        None => return Err(RequestError::InvalidResponse(text)),
    };
    match (upnp_error.get_child("errorCode"), upnp_error.get_child("errorDescription")) {
        (Some(e), Some(d)) => match (e.text.as_ref(), d.text.as_ref()) {
            (Some(et), Some(dt)) => {
                match et.parse::<u16>() {
                    Ok(en)  => Err(RequestError::ErrorCode(en, From::from(&dt[..]))),
                    Err(..) => Err(RequestError::InvalidResponse(text)),
                }
            },
            _ => Err(RequestError::InvalidResponse(text)),
        },
        _ => Err(RequestError::InvalidResponse(text)),
    }
}
