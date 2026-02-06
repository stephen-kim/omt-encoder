use std::fs;
use std::process::{Child, Command, Stdio};

pub struct MdnsPublisher {
    child: Child,
}

impl MdnsPublisher {
    pub fn start(source_name: &str, port: u16) -> Option<Self> {
        let hostname = read_hostname().unwrap_or_else(|| "omtcapture".to_string());
        let instance = format!("{} ({})", hostname, source_name);

        let mut cmd = Command::new("avahi-publish-service");
        cmd.arg(&instance)
            .arg("_omt._tcp")
            .arg(port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        match cmd.spawn() {
            Ok(child) => {
                println!("mDNS published: {} on port {}", instance, port);
                Some(MdnsPublisher { child })
            }
            Err(e) => {
                eprintln!("mDNS publish failed (avahi-publish-service): {}", e);
                None
            }
        }
    }
}

impl Drop for MdnsPublisher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_hostname() -> Option<String> {
    if let Ok(value) = std::env::var("HOSTNAME") {
        let trimmed = value.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    if let Ok(raw) = fs::read_to_string("/etc/hostname") {
        let trimmed = raw.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    None
}
