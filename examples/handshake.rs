// Debug helper: connect over USB with adb_client's logging on.
use adb_client::{ADBDeviceExt, usb::ADBUSBDevice};
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    let key = std::env::home_dir().unwrap().join(".android/adbkey");
    match ADBUSBDevice::autodetect_with_custom_private_key(key) {
        Ok(d) => println!("connected; list: {:?}", d.list("/storage/emulated/0").map(|v| v.len())),
        Err(e) => println!("error: {e:?}"),
    }
}
