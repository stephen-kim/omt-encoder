use std::fs;
use std::process::{Child, Command, Stdio};

pub struct MdnsPublisher {
    child: Child,
}

impl MdnsPublisher {
    pub fn start(source_name: &str, port: u16) -> Option<Self> {
        let hostname = read_hostname().unwrap_or_else(|| "omtencoder".to_string());
        let instance = build_instance_name(&hostname, source_name);

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
    if let Ok(raw) = fs::read_to_string("/etc/hostname") {
        let trimmed = raw.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    if let Ok(out) = Command::new("uname").arg("-n").output() {
        if out.status.success() {
            let value = String::from_utf8_lossy(&out.stdout);
            let trimmed = value.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

fn build_instance_name(hostname: &str, source_name: &str) -> String {
    // Match libomtnet OMTAddress length limit (63 chars) for the full name:
    // "HOSTNAME (Source Name)"
    const MAX_FULLNAME_LENGTH: usize = 63;

    let mut name = source_name.to_string();
    let mut full = format!("{} ({})", hostname, name);
    let full_len = full.chars().count();
    if full_len > MAX_FULLNAME_LENGTH {
        let oversize = full_len - MAX_FULLNAME_LENGTH;
        let name_len = name.chars().count();
        if oversize < name_len {
            let new_len = name_len - oversize;
            name = name
                .chars()
                .take(new_len)
                .collect::<String>()
                .trim_end()
                .to_string();
            full = format!("{} ({})", hostname, name);
        }
    }
    full
}
