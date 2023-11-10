use crate::internal::{numprobe_from_id, time_from_id, Message, Payload};
use crate::net::{create_sock, id_from_payload, reverse_dns_lookup, ICMP_HDR_LEN, IP_HDR_LEN};
use anyhow::{bail, Result};
use pnet::packet::{
    icmp::{IcmpPacket, IcmpTypes},
    ipv4::Ipv4Packet,
    Packet,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{
        mpsc::{Receiver, Sender},
        Semaphore,
    },
    time::Instant,
};
use tracing::{error, info};

pub async fn receive(
    semaphore: Arc<Semaphore>,
    timetable: Arc<Mutex<HashMap<u16, Instant>>>,
    id_table: Arc<Mutex<HashMap<u16, (u8, usize)>>>,
    tx1: Sender<Message>,
    mut rx2: Receiver<Message>,
) -> Result<()> {
    info!("receiver: inside");
    let recv_sock = create_sock()?;
    let mut recv_buf = [0u8; 576];
    let mut recvd = HashSet::new();
    let mut dns_cache = HashMap::new();

    loop {
        if let Ok(Message::BreakReceiver) = rx2.try_recv() {
            info!("receiver: got BreakReceiver, closing the semaphore and breaking");
            break;
        }

        let (_bytes_received, ip_addr) = recv_sock.recv_from(&mut recv_buf).await?;

        let icmp_packet = match IcmpPacket::new(&recv_buf[IP_HDR_LEN..]) {
            Some(packet) => packet,
            None => bail!("couldn't make icmp packet"),
        };

        let id = if icmp_packet.get_icmp_type() == IcmpTypes::EchoReply {
            id_from_payload(icmp_packet.payload())
        } else {
            /* A part of the original IPv4 packet (header + at least first 8 bytes)
             * is contained in an ICMP error message. We use the identification fi-
             * eld to map responses back to correct hops. */
            let original_ipv4_packet = match Ipv4Packet::new(&recv_buf[IP_HDR_LEN + ICMP_HDR_LEN..])
            {
                Some(packet) => packet,
                None => bail!("couldn't make ivp4 packet"),
            };

            original_ipv4_packet.get_identification()
        };

        let rtt = time_from_id(&timetable, id).await?;

        if !recvd.contains(&id) {
            recvd.insert(id);
        } else {
            println!("receiving duplicates");
            continue;
        }

        let hostname = dns_cache
            .entry(ip_addr)
            .or_insert(match reverse_dns_lookup(ip_addr).await {
                Ok(host) => host,
                Err(e) => {
                    error!(
                        "receiver: error looking up ip addr: {} -- breaking printer and exiting",
                        e
                    );
                    tx1.send(Message::BreakPrinter).await?;
                    break;
                }
            })
            .clone();

        match icmp_packet.get_icmp_type() {
            IcmpTypes::TimeExceeded => {
                let numprobe = numprobe_from_id(id_table.clone(), id)?;

                if tx1
                    .send(Message::TimeExceeded(Payload {
                        id,
                        numprobe,
                        hostname: Some(hostname),
                        ip_addr: Some(ip_addr),
                        rtt: Some(rtt),
                    }))
                    .await
                    .is_err()
                {
                    break;
                }

                semaphore.add_permits(1);
                info!("receiver: added one more permit");
            }
            IcmpTypes::EchoReply => {
                let numprobe = numprobe_from_id(id_table.clone(), id)?;

                info!("receiver: sending EchoReply for hop {}", id);
                if tx1
                    .send(Message::EchoReply(Payload {
                        id,
                        numprobe,
                        hostname: Some(hostname),
                        ip_addr: Some(ip_addr),
                        rtt: Some(rtt),
                    }))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            IcmpTypes::DestinationUnreachable => {
                let numprobe = numprobe_from_id(id_table.clone(), id)?;

                info!("receiver: sending DestinationUnreachable for hop {}", id);
                if tx1
                    .send(Message::DestinationUnreachable(Payload {
                        id,
                        numprobe,
                        hostname: Some(hostname),
                        ip_addr: Some(ip_addr),
                        rtt: Some(rtt),
                    }))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            _ => {}
        }
    }
    info!("receiver: exiting");

    Ok(())
}
