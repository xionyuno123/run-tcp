use run_dpdk;
use arrayvec;
use run_packet;

use run_packet::udp::*;
use run_packet::ipv4::*;
use run_packet::ether::*;
use run_packet::Buf;

fn init_port(port_id: u16,
        nb_qs: u32,
        mp_name: &'static str,
        mpconf: &mut run_dpdk::MempoolConf,
        rxq_conf: &mut run_dpdk::RxQueueConf,
        txq_conf: &mut run_dpdk::TxQueueConf) {
  let port_infos = run_dpdk::service().port_infos().unwrap();
  let port_info = &port_infos[port_id as usize];
  let socket_id = port_info.socket_id;

  mpconf.socket_id = socket_id;
  run_dpdk::service().mempool_create(mp_name, mpconf).unwrap();
  
  let pconf = run_dpdk::PortConf::from_port_info(port_info).unwrap();

  rxq_conf.mp_name = mp_name.to_string();
  rxq_conf.socket_id = socket_id;
  txq_conf.socket_id = socket_id;

  let mut rxq_confs = Vec::new();
  let mut txq_confs = Vec::new();

  for _ in 0..nb_qs {
    rxq_confs.push(rxq_conf.clone());
    txq_confs.push(txq_conf.clone());
  }

  run_dpdk::service()
              .port_configure(port_id, &pconf, &rxq_confs, &txq_confs)
              .unwrap();
  
  println!("finish configuration p{}", port_id);
}


fn main() {
  run_dpdk::DpdkOption::new().init().unwrap();
    
  let port_id = 0;
  let nb_qs = 14;
  let mp_name = "mp";
  let mut mconf = run_dpdk::MempoolConf::default();
  mconf.nb_mbufs = 8192 * 4;
  mconf.per_core_caches = 256;
  mconf.socket_id = 0;

  let mut rxq_conf = run_dpdk::RxQueueConf::default();
  rxq_conf.mp_name = "mp".to_string();
  rxq_conf.nb_rx_desc = 1024;
  rxq_conf.socket_id = 0;
    
  let mut txq_conf = run_dpdk::TxQueueConf::default();
  txq_conf.nb_tx_desc = 1024;
  txq_conf.socket_id = 0;

  init_port(port_id, nb_qs, mp_name, &mut mconf, &mut rxq_conf, &mut txq_conf);

  let start_core = 1;
  let socket_id = run_dpdk::service()
                                    .port_infos()
                                    .unwrap()[port_id as usize].socket_id;
  
  run_dpdk::service()
              .lcores()
              .iter()
              .find(|lcore| 
                lcore.lcore_id >= start_core && 
                lcore.lcore_id < start_core + nb_qs)
              .map(|lcore| {
                assert!(lcore.socket_id == socket_id, "core with invalid socket id");
              });
  
  let run = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
  let run_curr = run.clone();
  let run_clone = run.clone();

  ctrlc::set_handler(move || {
    run_clone.store(false, std::sync::atomic::Ordering::Release);
  }).unwrap();

  let total_header_len = ETHER_HEADER_LEN + IPV4_HEADER_LEN +UDP_HEADER_LEN;
  let payload_len = 18;

  println!("Packet Size: {}",payload_len + total_header_len);

  let mut adder = 0;
  let total_ips = 200;

  let mut jhs = Vec::new();

  for i in 0..nb_qs {
    let run = run.clone();
    let jh = std::thread::spawn(move || {
      run_dpdk::service().lcore_bind(i + 1).unwrap();
      let mut txq = run_dpdk::service().tx_queue(port_id, i as u16).unwrap();
      let mp = run_dpdk::service().mempool(mp_name).unwrap();
      let mut batch = arrayvec::ArrayVec::<_,64>::new();

      while run.load(std::sync::atomic::Ordering::Acquire) {
        mp.fill_batch(&mut batch);
        for mbuf in batch.iter_mut() {
          unsafe { mbuf.extend(total_header_len + payload_len) };

          let mut pkt = run_packet::CursorMut::new(mbuf.data_mut());
          pkt.advance(total_header_len);

          let mut udppkt = UdpPacket::prepend_header(pkt,&UDP_HEADER_TEMPLATE);

          udppkt.set_source_port(60376);
          udppkt.set_dest_port(161);

          let mut ippkt = Ipv4Packet::prepend_header(udppkt.release(), &IPV4_HEADER_TEMPLATE);

          ippkt.set_ident(0x5c65);
          ippkt.clear_flags();
          ippkt.set_time_to_live(128);
          ippkt.set_source_ip(Ipv4Addr([192, 168, 57, 10 + (adder % total_ips)]));
          adder = adder.wrapping_add(1);
          ippkt.set_dest_ip(Ipv4Addr([192, 168, 23, 1]));
          ippkt.set_protocol(IpProtocol::UDP);
          ippkt.adjust_checksum();

          let mut ethpkt = EtherPacket::prepend_header(ippkt.release(),
                                                                   &ETHER_HEADER_TEMPLATE);
          ethpkt.set_dest_mac(MacAddr([0x10, 0x70, 0xfd, 0x15, 0x77, 0xc1]));
          ethpkt.set_source_mac(MacAddr([0x00, 0x50, 0x56, 0xae, 0x76, 0xf5]));
          ethpkt.set_ethertype(EtherType::IPV4);
        }

        while batch.len() > 0 {
          let _sent = txq.tx(&mut batch);
        }
      }
    });

    jhs.push(jh);
  }

  let mut old_stats = run_dpdk::service().port_stats(port_id).unwrap();
  while run_curr.load(std::sync::atomic::Ordering::Acquire) {
    let sleep_second = 60;
    std::thread::sleep(std::time::Duration::from_secs(sleep_second));
    let curr_stats = run_dpdk::service().port_stats(port_id).unwrap();
    println!(
      "pkts per sec: {}, throughput per sec: {} Gbps/s, errors per sec: {}",
      (curr_stats.opackets() - old_stats.opackets()) / sleep_second,
      ((curr_stats.obytes() - old_stats.obytes()) as f64 * 8.0 / 1000000000.0) / sleep_second as f64,
      curr_stats.oerrors() - old_stats.oerrors(),
    );

    old_stats = curr_stats;

  }

  for jh in jhs {
    jh.join().unwrap();
  }

  run_dpdk::service().port_close(port_id).unwrap();
  println!("port closed");

  run_dpdk::service().mempool_free(mp_name).unwrap();
  println!("mempool freed");

  run_dpdk::service().service_close().unwrap();
  println!("dpdk service shutdown gracefully");
}