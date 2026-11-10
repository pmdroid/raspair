use airplay_core::discovery::device::merge_device;
use airplay_core::discovery::discover::merge_sighting;
use airplay_core::discovery::find_device;
use airplay_core::protocol::{parse_txt_record, AuthRequirement};
use airplay_core::Device;
use std::net::Ipv4Addr;

fn device(name: &str, id: &str, ip: [u8; 4], port: u16, txt: &[&str]) -> Device {
    let mut record: Vec<String> = txt.iter().map(|s| s.to_string()).collect();
    if !id.is_empty() {
        record.push(format!("deviceid={id}"));
    }
    Device {
        name: name.into(),
        host: format!("{name}.local"),
        port,
        ipv4: Some(Ipv4Addr::from(ip)),
        ipv6: None,
        record: parse_txt_record(&record),
        last_seen: 0,
    }
}

const AP2: &[&str] = &["features=0x405C4A00,0x1C340", "flags=0x4"];

#[test]
fn raop_and_airplay_sightings_merge_by_receiver_id() {
    for raop_first in [false, true] {
        let airplay = device("Kitchen", "AA:BB:CC:DD:EE:01", [10, 0, 0, 5], 7000, AP2);
        let raop = device("Kitchen", "aa:bb:cc:dd:ee:01", [10, 0, 0, 6], 7001, &["ft=0x405C4A00,0x1C340", "sf=0x4", "pw=true"]);
        let mut devices = Vec::new();
        let order = if raop_first { [(raop, true), (airplay, false)] } else { [(airplay, false), (raop, true)] };
        for (d, is_raop) in order {
            merge_sighting(&mut devices, d, is_raop);
        }
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].port, 7000);
        assert_eq!(devices[0].route().auth, AuthRequirement::HapTransientPassword);
        let expected = if raop_first { [10, 0, 0, 5] } else { [10, 0, 0, 6] };
        assert_eq!(devices[0].ipv4, Some(Ipv4Addr::from(expected)));
    }
}

#[test]
fn identical_names_stay_distinct_and_ambiguous_names_select_nothing() {
    let mut devices = Vec::new();
    merge_sighting(&mut devices, device("Speaker", "aa:00:00:00:00:01", [10, 0, 0, 1], 7000, AP2), false);
    merge_sighting(&mut devices, device("Speaker", "aa:00:00:00:00:02", [10, 0, 0, 2], 7000, AP2), false);
    merge_sighting(&mut devices, device("Speaker Two", "aa:00:00:00:00:03", [10, 0, 0, 3], 7000, AP2), false);
    assert_eq!(devices.len(), 3);
    assert!(find_device(&devices, "speaker").is_none());
    assert!(find_device(&devices, "spea").is_none());
    assert_eq!(find_device(&devices, "AA:00:00:00:00:02").unwrap().ipv4, Some(Ipv4Addr::new(10, 0, 0, 2)));
    assert_eq!(find_device(&devices, "speaker two").unwrap().record.device_id, "aa:00:00:00:00:03");
}

#[test]
fn changed_ip_updates_address_without_reaching_another_identity() {
    let mut devices = vec![
        device("Kitchen", "aa:00:00:00:00:01", [10, 0, 0, 5], 7000, AP2),
        device("Den", "aa:00:00:00:00:02", [10, 0, 0, 6], 7000, AP2),
    ];
    merge_device(&mut devices, device("Den", "aa:00:00:00:00:02", [10, 0, 0, 5], 7000, AP2));
    assert_eq!(devices.len(), 2);
    let kitchen = find_device(&devices, "kitchen").unwrap();
    assert_eq!(kitchen.record.device_id, "aa:00:00:00:00:01");
    assert_eq!(kitchen.ip(), None);
    let den = find_device(&devices, "den").unwrap();
    assert_eq!(den.ip().as_deref(), Some("10.0.0.5"));

    merge_device(&mut devices, device("Kitchen", "", [10, 0, 0, 5], 7000, AP2));
    assert_eq!(devices.len(), 3);
    assert_eq!(find_device(&devices, "aa:00:00:00:00:02").unwrap().ip().as_deref(), Some("10.0.0.5"));
    assert!(find_device(&devices, "kitchen").is_none());

    merge_device(&mut devices, device("Spare", "aa:00:00:00:00:04", [10, 0, 0, 5], 7001, AP2));
    assert_eq!(find_device(&devices, "den").unwrap().ip().as_deref(), Some("10.0.0.5"));
}
