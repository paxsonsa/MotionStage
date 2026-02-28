use std::net::TcpListener;

/// Returns a random available port.
pub fn get_available_port() -> Option<u16> {
    let Ok(addr) = TcpListener::bind("0.0.0.0:0") else {
        return None;
    };

    let Ok(local_addr) = addr.local_addr() else {
        return None;
    };

    Some(local_addr.port())
}

pub fn get_new_localhost_address() -> std::net::SocketAddr {
    std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        get_available_port().unwrap(),
    )
}

pub fn get_new_host_address() -> std::net::SocketAddr {
    std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
        get_available_port().unwrap(),
    )
}
