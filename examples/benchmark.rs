#![allow(clippy::collapsible_if)]

mod utils;

use log::debug;
use std::cmp;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use smoltcp::iface::{InterfaceBuilder, NeighborCache, SocketSet};
use smoltcp::phy::{wait as phy_wait, Device, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr};

const AMOUNT: usize = 1_000_000_000;

enum Client {
    Reader,
    Writer,
}

fn client(kind: Client) {
    let port = match kind {
        Client::Reader => 1234,
        Client::Writer => 1235,
    };
    let mut stream = TcpStream::connect(("192.168.69.1", port)).unwrap();
    let mut buffer = vec![0; 1_000_000];

    let start = Instant::now();

    let mut processed = 0;
    while processed < AMOUNT {
        let length = cmp::min(buffer.len(), AMOUNT - processed);
        let result = match kind {
            Client::Reader => stream.read(&mut buffer[..length]),
            Client::Writer => stream.write(&buffer[..length]),
        };
        match result {
            Ok(0) => break,
            Ok(result) => {
                // print!("(P:{})", result);
                processed += result
            }
            Err(err) => panic!("cannot process: {}", err),
        }
    }

    let end = Instant::now();

    let elapsed = (end - start).total_millis() as f64 / 1000.0;

    println!("throughput: {:.3} Gbps", AMOUNT as f64 / elapsed / 0.125e9);

    CLIENT_DONE.store(true, Ordering::SeqCst);
}

static CLIENT_DONE: AtomicBool = AtomicBool::new(false);

fn main() {
    #[cfg(feature = "log")]
    utils::setup_logging("info");

    let (mut opts, mut free) = utils::create_options();
    utils::add_tuntap_options(&mut opts, &mut free);
    utils::add_middleware_options(&mut opts, &mut free);
    free.push("MODE");

    let mut matches = utils::parse_options(&opts, free);
    let device = utils::parse_tuntap_options(&mut matches);
    let fd = device.as_raw_fd();
    let mut device =
        utils::parse_middleware_options(&mut matches, device, /*loopback=*/ false);
    let mode = match matches.free[0].as_ref() {
        "reader" => Client::Reader,
        "writer" => Client::Writer,
        _ => panic!("invalid mode"),
    };

    let neighbor_cache = NeighborCache::new();

    let tcp1_rx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp1_tx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp1_socket = tcp::Socket::new(tcp1_rx_buffer, tcp1_tx_buffer);

    let tcp2_rx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp2_tx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp2_socket = tcp::Socket::new(tcp2_rx_buffer, tcp2_tx_buffer);

    let ethernet_addr = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let mut ip_addrs = heapless::Vec::<IpCidr, 5>::new();
    ip_addrs
        .push(IpCidr::new(IpAddress::v4(192, 168, 69, 1), 24))
        .unwrap();
    let medium = device.capabilities().medium;
    let mut builder = InterfaceBuilder::new().ip_addrs(ip_addrs);
    if medium == Medium::Ethernet {
        builder = builder
            .hardware_addr(ethernet_addr.into())
            .neighbor_cache(neighbor_cache);
    }
    let mut iface = builder.finalize(&mut device);

    let mut sockets = SocketSet::new(vec![]);
    let tcp1_handle = sockets.add(tcp1_socket);
    let tcp2_handle = sockets.add(tcp2_socket);
    let default_timeout = Some(Duration::from_millis(1000));

    thread::spawn(move || client(mode));
    let mut processed = 0;
    while !CLIENT_DONE.load(Ordering::SeqCst) {
        let timestamp = Instant::now();
        match iface.poll(timestamp, &mut device, &mut sockets) {
            Ok(_) => {}
            Err(e) => {
                debug!("poll error: {}", e);
            }
        }

        // tcp:1234: emit data
        let socket = sockets.get_mut::<tcp::Socket>(tcp1_handle);
        if !socket.is_open() {
            socket.listen(1234).unwrap();
        }

        if socket.can_send() {
            if processed < AMOUNT {
                let length = socket
                    .send(|buffer| {
                        let length = cmp::min(buffer.len(), AMOUNT - processed);
                        (length, length)
                    })
                    .unwrap();
                processed += length;
            }
        }

        // tcp:1235: sink data
        let socket = sockets.get_mut::<tcp::Socket>(tcp2_handle);
        if !socket.is_open() {
            socket.listen(1235).unwrap();
        }

        if socket.can_recv() {
            if processed < AMOUNT {
                let length = socket
                    .recv(|buffer| {
                        let length = cmp::min(buffer.len(), AMOUNT - processed);
                        (length, length)
                    })
                    .unwrap();
                processed += length;
            }
        }

        match iface.poll_at(timestamp, &sockets) {
            Some(poll_at) if timestamp < poll_at => {
                phy_wait(fd, Some(poll_at - timestamp)).expect("wait error");
            }
            Some(_) => (),
            None => {
                phy_wait(fd, default_timeout).expect("wait error");
            }
        }
    }
}
