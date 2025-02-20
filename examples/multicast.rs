mod utils;

use log::debug;
use std::os::unix::io::AsRawFd;

use smoltcp::iface::{InterfaceBuilder, NeighborCache, SocketSet};
use smoltcp::phy::wait as phy_wait;
use smoltcp::socket::{raw, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, IgmpPacket, IgmpRepr, IpAddress, IpCidr, IpProtocol, IpVersion, Ipv4Address,
    Ipv4Packet,
};

const MDNS_PORT: u16 = 5353;
const MDNS_GROUP: [u8; 4] = [224, 0, 0, 251];

fn main() {
    utils::setup_logging("warn");

    let (mut opts, mut free) = utils::create_options();
    utils::add_tuntap_options(&mut opts, &mut free);
    utils::add_middleware_options(&mut opts, &mut free);

    let mut matches = utils::parse_options(&opts, free);
    let device = utils::parse_tuntap_options(&mut matches);
    let fd = device.as_raw_fd();
    let mut device =
        utils::parse_middleware_options(&mut matches, device, /*loopback=*/ false);
    let neighbor_cache = NeighborCache::new();

    let local_addr = Ipv4Address::new(192, 168, 69, 2);

    let ethernet_addr = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
    let ip_addr = IpCidr::new(IpAddress::from(local_addr), 24);
    let mut ip_addrs = heapless::Vec::<IpCidr, 5>::new();
    ip_addrs.push(ip_addr).unwrap();
    let mut iface = InterfaceBuilder::new()
        .hardware_addr(ethernet_addr.into())
        .neighbor_cache(neighbor_cache)
        .ip_addrs(ip_addrs)
        .finalize(&mut device);

    let now = Instant::now();
    // Join a multicast group to receive mDNS traffic
    iface
        .join_multicast_group(&mut device, Ipv4Address::from_bytes(&MDNS_GROUP), now)
        .unwrap();

    let mut sockets = SocketSet::new(vec![]);

    // Must fit at least one IGMP packet
    let raw_rx_buffer = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; 2], vec![0; 512]);
    // Will not send IGMP
    let raw_tx_buffer = raw::PacketBuffer::new(vec![], vec![]);
    let raw_socket = raw::Socket::new(
        IpVersion::Ipv4,
        IpProtocol::Igmp,
        raw_rx_buffer,
        raw_tx_buffer,
    );
    let raw_handle = sockets.add(raw_socket);

    // Must fit mDNS payload of at least one packet
    let udp_rx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 1024]);
    // Will not send mDNS
    let udp_tx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 0]);
    let udp_socket = udp::Socket::new(udp_rx_buffer, udp_tx_buffer);
    let udp_handle = sockets.add(udp_socket);

    loop {
        let timestamp = Instant::now();
        match iface.poll(timestamp, &mut device, &mut sockets) {
            Ok(_) => {}
            Err(e) => {
                debug!("poll error: {}", e);
            }
        }

        let socket = sockets.get_mut::<raw::Socket>(raw_handle);

        if socket.can_recv() {
            // For display purposes only - normally we wouldn't process incoming IGMP packets
            // in the application layer
            match socket.recv() {
                Err(e) => println!("Recv IGMP error: {:?}", e),
                Ok(buf) => {
                    Ipv4Packet::new_checked(buf)
                        .and_then(|ipv4_packet| IgmpPacket::new_checked(ipv4_packet.payload()))
                        .and_then(|igmp_packet| IgmpRepr::parse(&igmp_packet))
                        .map(|igmp_repr| println!("IGMP packet: {:?}", igmp_repr))
                        .unwrap_or_else(|e| println!("parse IGMP error: {:?}", e));
                }
            }
        }

        let socket = sockets.get_mut::<udp::Socket>(udp_handle);
        if !socket.is_open() {
            socket.bind(MDNS_PORT).unwrap()
        }

        if socket.can_recv() {
            socket
                .recv()
                .map(|(data, sender)| {
                    println!("mDNS traffic: {} UDP bytes from {}", data.len(), sender)
                })
                .unwrap_or_else(|e| println!("Recv UDP error: {:?}", e));
        }

        phy_wait(fd, iface.poll_delay(timestamp, &sockets)).expect("wait error");
    }
}
