//! `status`: is the virtual camera listed by Media Foundation, and by DirectShow (what OBS uses)?

use vcam_install::{enum_directshow_names, enum_mf_devices, init_com_and_mf};

pub fn run() -> i32 {
    if let Err(e) = init_com_and_mf() {
        eprintln!("MFStartup failed: {e}");
        return 1;
    }
    let want = vcam_protocol::CAMERA_NAME.to_lowercase();
    println!(
        "registry clsid key present: {}",
        vcam_install::registry::clsid_key_present()
    );

    let devices = match enum_mf_devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("MFEnumDeviceSources failed: {e}");
            return 1;
        }
    };
    for (_, d) in &devices {
        println!("mf device: '{}' link '{}'", d.name, d.link);
    }
    match devices
        .iter()
        .find(|(_, d)| d.name.to_lowercase().contains(&want))
    {
        Some((_, d)) => {
            println!("listed: true");
            println!("friendly name: {}", d.name);
            println!("symbolic link: {}", d.link);
        }
        None => println!("listed: false"),
    }

    match enum_directshow_names() {
        Ok(names) => {
            for n in &names {
                println!("directshow device: '{n}'");
            }
            println!(
                "directshow listed: {}",
                names.iter().any(|n| n.to_lowercase().contains(&want))
            );
        }
        Err(e) => println!("directshow enumeration failed: {e}"),
    }
    0
}
