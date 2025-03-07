use pnet::packet::icmp::echo_request;
use pnet::packet::icmp::IcmpTypes;
use pnet::packet::icmpv6::{Icmpv6Types, MutableIcmpv6Packet};
use pnet::packet::Packet;
use pnet::transport::TransportSender;
use pnet::util;
use pnet_macros_support::types::*;
use rand::random;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use PingResult;

pub struct Ping {
    addr: IpAddr,
    identifier: u16,
    sequence_number: u16,
    pub seen: bool,
}

pub struct ReceivedPing {
    pub addr: IpAddr,
    pub identifier: u16,
    pub sequence_number: u16,
    pub rtt: Duration,
    pub ttl: u8,
}

impl Ping {
    pub fn new(addr: IpAddr) -> Ping {
        let mut identifier = 0;
        if addr.is_ipv4() {
            identifier = random::<u16>();
        }
        Ping {
            addr,
            identifier,
            sequence_number: 0,
            seen: false,
        }
    }

    pub fn new_with_seq(addr: IpAddr, seq: u16) -> Ping {
        let mut identifier = 0;
        if addr.is_ipv4() {
            identifier = random::<u16>();
        }
        Ping {
            addr,
            identifier,
            sequence_number: seq,
            seen: false,
        }
    }

    pub fn get_addr(&self) -> IpAddr {
        return self.addr;
    }

    pub fn get_identifier(&self) -> u16 {
        return self.identifier;
    }

    pub fn get_sequence_number(&self) -> u16 {
        return self.sequence_number;
    }

    pub fn increment_sequence_number(&mut self) -> u16 {
        self.sequence_number += 1;
        return self.sequence_number;
    }
}

fn send_echo(
    tx: &mut TransportSender,
    ping: &mut Ping,
    size: usize,
) -> Result<usize, std::io::Error> {
    // Allocate enough space for a new packet
    let mut vec: Vec<u8> = vec![0; size];

    // Use echo_request so we can set the identifier and sequence number
    let mut echo_packet = echo_request::MutableEchoRequestPacket::new(&mut vec[..]).unwrap();
    echo_packet.set_sequence_number(ping.increment_sequence_number());
    echo_packet.set_identifier(ping.get_identifier());
    echo_packet.set_icmp_type(IcmpTypes::EchoRequest);

    let csum = icmp_checksum(&echo_packet);
    echo_packet.set_checksum(csum);

    tx.send_to(echo_packet, ping.get_addr())
}

fn send_echov6(
    tx: &mut TransportSender,
    addr: IpAddr,
    size: usize,
) -> Result<usize, std::io::Error> {
    // Allocate enough space for a new packet
    let mut vec: Vec<u8> = vec![0; size];

    let mut echo_packet = MutableIcmpv6Packet::new(&mut vec[..]).unwrap();
    echo_packet.set_icmpv6_type(Icmpv6Types::EchoRequest);

    let csum = icmpv6_checksum(&echo_packet);
    echo_packet.set_checksum(csum);

    tx.send_to(echo_packet, addr)
}

pub fn send_pings(
    size: usize,
    timer: Arc<RwLock<Instant>>,
    stop: Arc<Mutex<bool>>,
    results_sender: Sender<PingResult>,
    thread_rx: Arc<Mutex<Receiver<ReceivedPing>>>,
    tx: Arc<Mutex<TransportSender>>,
    txv6: Arc<Mutex<TransportSender>>,
    targets: Arc<Mutex<BTreeMap<IpAddr, Ping>>>,
    max_rtt: Arc<Duration>,
) {
    loop {
        for (addr, ping) in targets.lock().unwrap().iter_mut() {
            match if addr.is_ipv4() {
                send_echo(&mut tx.lock().unwrap(), ping, size)
            } else if addr.is_ipv6() {
                send_echov6(&mut txv6.lock().unwrap(), *addr, size)
            } else {
                Ok(0)
            } {
                Err(e) => error!("Failed to send ping to {:?}: {}", *addr, e),
                _ => {}
            }
            ping.seen = false;
        }
        {
            // start the timer
            let mut timer = timer.write().unwrap();
            *timer = Instant::now();
        }
        loop {
            // use recv_timeout so we don't cause a CPU to needlessly spin
            match thread_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_millis(100))
            {
                Ok(ping_result) => {
                    match ping_result {
                        ReceivedPing {
                            addr,
                            identifier,
                            sequence_number,
                            rtt,
                            ttl,
                        } => {
                            // Update the address to the ping response being received
                            if let Some(ping) = targets.lock().unwrap().get_mut(&addr) {
                                if ping.get_identifier() == identifier
                                    && ping.get_sequence_number() == sequence_number
                                {
                                    ping.seen = true;
                                    // Send the ping result over the client channel
                                    match results_sender.send(PingResult::Receive {
                                        addr: ping_result.addr,
                                        rtt,
                                        seq: sequence_number,
                                        ttl,
                                    }) {
                                        Ok(_) => {}
                                        Err(e) => {
                                            if !*stop.lock().unwrap() {
                                                error!(
                                                    "Error sending ping result on channel: {}",
                                                    e
                                                )
                                            }
                                        }
                                    }
                                } else {
                                    debug!("Received echo reply from target {}, but sequence_number (expected {} but got {}) and identifier (expected {} but got {}) don't match", addr, ping.get_sequence_number(), sequence_number, ping.get_identifier(), identifier);
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    // Check we haven't exceeded the max rtt
                    let start_time = timer.read().unwrap();
                    if Instant::now().duration_since(*start_time) > *max_rtt {
                        break;
                    }
                }
            }
        }
        // check for addresses which haven't replied
        for (addr, ping) in targets.lock().unwrap().iter() {
            if ping.seen == false {
                // Send the ping Idle over the client channel
                match results_sender.send(PingResult::Idle { addr: *addr }) {
                    Ok(_) => {}
                    Err(e) => {
                        if !*stop.lock().unwrap() {
                            error!("Error sending ping Idle result on channel: {}", e)
                        }
                    }
                }
            }
        }
        // check if we've received the stop signal
        if *stop.lock().unwrap() {
            return;
        }
    }
}

fn icmp_checksum(packet: &echo_request::MutableEchoRequestPacket) -> u16be {
    util::checksum(packet.packet(), 1)
}

fn icmpv6_checksum(packet: &MutableIcmpv6Packet) -> u16be {
    util::checksum(packet.packet(), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ping() {
        let mut p = Ping::new("127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(p.get_sequence_number(), 0);
        assert!(p.get_identifier() > 0);

        p.increment_sequence_number();
        assert_eq!(p.get_sequence_number(), 1);
    }
}
