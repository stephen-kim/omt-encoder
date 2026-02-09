use crate::channel::OMTChannel;
use crate::enums::OMTFrameType;
use crate::frame::OMTFrame;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};

pub struct OMTServer {
    listener: TcpListener,
    tx: broadcast::Sender<OMTFrame>,
    metadata_state: Arc<Mutex<ServerMetadataState>>,
    suggested_quality_hint: Arc<AtomicU8>,
}

impl OMTServer {
    pub async fn new(port: u16) -> Result<Self, io::Error> {
        let addr = format!("0.0.0.0:{}", port);
        let listener = TcpListener::bind(addr).await?;

        // Channel capacity: how many frames to buffer before lagging receivers miss out
        let (tx, _) = broadcast::channel(100);

        Ok(OMTServer {
            listener,
            tx,
            metadata_state: Arc::new(Mutex::new(ServerMetadataState::default())),
            suggested_quality_hint: Arc::new(AtomicU8::new(0)),
        })
    }

    pub fn get_sender(&self) -> broadcast::Sender<OMTFrame> {
        self.tx.clone()
    }

    pub fn suggested_quality_hint(&self) -> Arc<AtomicU8> {
        self.suggested_quality_hint.clone()
    }

    pub async fn set_sender_info_xml(&self, xml: Option<String>) {
        self.metadata_state.lock().await.sender_info_xml = xml;
    }

    pub async fn set_redirect_address(&self, address: Option<String>) {
        self.metadata_state.lock().await.redirect_address = address;
    }

    pub async fn set_tally(&self, preview: bool, program: bool) {
        let mut st = self.metadata_state.lock().await;
        st.tally_preview = preview;
        st.tally_program = program;
    }

    pub async fn add_connection_metadata(&self, xml: String) {
        if xml.trim().is_empty() {
            return;
        }
        self.metadata_state
            .lock()
            .await
            .connection_metadata
            .push(xml);
    }

    pub async fn clear_connection_metadata(&self) {
        self.metadata_state.lock().await.connection_metadata.clear();
    }

    pub async fn run(&self) -> Result<(), io::Error> {
        let mut conn_id: u64 = 1;
        loop {
            let (socket, addr) = self.listener.accept().await?;
            println!("Accepted connection from: {}", addr);

            let tx = self.tx.clone();
            let metadata_state = Arc::clone(&self.metadata_state);
            let quality_hint = Arc::clone(&self.suggested_quality_hint);
            let this_conn_id = conn_id;
            conn_id = conn_id.wrapping_add(1);

            tokio::spawn(async move {
                if let Err(e) =
                    handle_connection(socket, tx, metadata_state, quality_hint, this_conn_id).await
                {
                    eprintln!("Connection error: {}", e);
                }
            });
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ServerMetadataState {
    sender_info_xml: Option<String>,
    connection_metadata: Vec<String>,
    redirect_address: Option<String>,
    tally_preview: bool,
    tally_program: bool,
    per_connection_suggested_quality: HashMap<u64, u8>,
}

#[derive(Debug, Clone)]
struct SubscriptionState {
    wants_video: bool,
    wants_audio: bool,
    wants_metadata: bool,
    preview: bool,
    tally_preview: bool,
    tally_program: bool,
    suggested_quality: Option<String>,
    sender_info_xml: Option<String>,
    redirect_address: Option<String>,
}

impl Default for SubscriptionState {
    fn default() -> Self {
        SubscriptionState {
            // Match C# OMTChannel defaults: nothing subscribed until explicit metadata command
            wants_video: false,
            wants_audio: false,
            wants_metadata: false,
            preview: false,
            tally_preview: false,
            tally_program: false,
            suggested_quality: None,
            sender_info_xml: None,
            redirect_address: None,
        }
    }
}

async fn handle_connection(
    socket: TcpStream,
    tx: broadcast::Sender<OMTFrame>,
    metadata_state: Arc<Mutex<ServerMetadataState>>,
    suggested_quality_hint: Arc<AtomicU8>,
    conn_id: u64,
) -> Result<(), io::Error> {
    // Configure socket buffers
    // Note: OMT logic sets specific buffer sizes
    // We can rely on OS defaults or set them if critical via socket2 crate or implicit behavior.

    let channel = OMTChannel::new(socket);
    let mut rx = tx.subscribe();

    let (mut sink, mut stream) = channel.into_split();

    let initial_metadata = {
        let st = metadata_state.lock().await.clone();
        let mut xmls = Vec::new();
        if let Some(xml) = st.sender_info_xml {
            xmls.push(xml);
        }
        xmls.extend(
            st.connection_metadata
                .into_iter()
                .filter(|v| !v.trim().is_empty()),
        );
        xmls.push(format!(
            "<OMTTally Preview=\"{}\" Program==\"{}\" />",
            if st.tally_preview { "true" } else { "false" },
            if st.tally_program { "true" } else { "false" }
        ));
        if let Some(addr) = st.redirect_address {
            if !addr.trim().is_empty() {
                xmls.push(format!("<OMTRedirect Address=\"{}\" />", addr));
            }
        }
        xmls
    };
    for xml in initial_metadata {
        let frame = metadata_frame_from_xml(&xml);
        if let Err(e) = sink.send(frame).await {
            return Err(e);
        }
    }

    let state = Arc::new(Mutex::new(SubscriptionState::default()));

    {
        let mut st = metadata_state.lock().await;
        st.per_connection_suggested_quality.insert(conn_id, 0);
    }
    update_global_suggested_quality(&metadata_state, &suggested_quality_hint).await;

    // Spawn send task (Broadcast -> TCP)
    let send_state = Arc::clone(&state);
    let send_task = tokio::spawn(async move {
        loop {
            let frame = match rx.recv().await {
                Ok(frame) => frame,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    eprintln!(
                        "Sender lagged on client stream (skipped {} frames). Continuing.",
                        skipped
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            };

            let allowed = {
                let state = send_state.lock().await;
                match frame.header.frame_type {
                    // Match C#: metadata always passes subscription filter
                    OMTFrameType::Metadata => true,
                    OMTFrameType::Video => state.wants_video,
                    OMTFrameType::Audio => state.wants_audio,
                    _ => false,
                }
            };
            if !allowed {
                continue;
            }
            let maybe_preview_frame = {
                let st = send_state.lock().await;
                if st.preview && frame.header.frame_type == OMTFrameType::Video {
                    let mut f = frame.clone();
                    f.preview_mode = true;
                    if f.preview_data_length.is_none() {
                        f.preview_data_length = Some(f.header.data_length);
                    }
                    f
                } else {
                    frame
                }
            };
            if let Err(e) = sink.send(maybe_preview_frame).await {
                return Err(e);
            }
        }
        Ok::<(), io::Error>(())
    });

    // Spawn receive task (TCP -> Logic? or simple keepalive/metadata handling)
    // For a broadcaster, we mostly just read control messages or ignore incoming data?
    // In C# OMTSend.cs: OnAccept -> creates Channel. Channel reads frames.
    // If it's a "metadata server", it might accept metadata.
    // For now, let's just drain the stream or handle simple frames.

    while let Some(result) = stream.next().await {
        match result {
            Ok(frame) => {
                handle_subscription_frame(&frame, &state).await;
                sync_connection_suggested_quality(
                    conn_id,
                    &state,
                    &metadata_state,
                    &suggested_quality_hint,
                )
                .await;
            }
            Err(e) => {
                remove_connection_suggested_quality(
                    conn_id,
                    &metadata_state,
                    &suggested_quality_hint,
                )
                .await;
                return Err(e);
            }
        }
    }

    remove_connection_suggested_quality(conn_id, &metadata_state, &suggested_quality_hint).await;
    let _ = send_task.await;
    Ok(())
}

async fn handle_subscription_frame(frame: &OMTFrame, state: &Arc<Mutex<SubscriptionState>>) {
    if let Some(xml) = extract_metadata_xml(frame) {
        // Fast path for exact protocol constants used by C# sender/receiver.
        // These are compared by exact string in the original implementation.
        match xml.as_str() {
            "<OMTSubscribe Video=\"true\" />" => {
                println!("Client subscribed: video");
                state.lock().await.wants_video = true;
                return;
            }
            "<OMTSubscribe Audio=\"true\" />" => {
                println!("Client subscribed: audio");
                state.lock().await.wants_audio = true;
                return;
            }
            "<OMTSubscribe Metadata=\"true\" />" => {
                println!("Client subscribed: metadata");
                state.lock().await.wants_metadata = true;
                return;
            }
            "<OMTSettings Preview=\"true\" />" => {
                println!("Client settings: preview=true");
                state.lock().await.preview = true;
                return;
            }
            "<OMTSettings Preview=\"false\" />" => {
                println!("Client settings: preview=false");
                state.lock().await.preview = false;
                return;
            }
            "<OMTTally Preview=\"true\" Program==\"true\" />" => {
                let mut st = state.lock().await;
                st.tally_preview = true;
                st.tally_program = true;
                return;
            }
            "<OMTTally Preview=\"false\" Program==\"true\" />" => {
                let mut st = state.lock().await;
                st.tally_preview = false;
                st.tally_program = true;
                return;
            }
            "<OMTTally Preview=\"true\" Program==\"false\" />" => {
                let mut st = state.lock().await;
                st.tally_preview = true;
                st.tally_program = false;
                return;
            }
            "<OMTTally Preview=\"false\" Program==\"false\" />" => {
                let mut st = state.lock().await;
                st.tally_preview = false;
                st.tally_program = false;
                return;
            }
            _ => {}
        }

        if let Some((tag, attrs)) = parse_xml_tag_and_attrs(&xml) {
            match tag.to_ascii_lowercase().as_str() {
                "omtsubscribe" => {
                    let mut st = state.lock().await;
                    if attr_true(&attrs, "Video") {
                        println!("Client subscribed: video");
                        st.wants_video = true;
                    }
                    if attr_true(&attrs, "Audio") {
                        println!("Client subscribed: audio");
                        st.wants_audio = true;
                    }
                    if attr_true(&attrs, "Metadata") {
                        println!("Client subscribed: metadata");
                        st.wants_metadata = true;
                    }
                    return;
                }
                "omtsettings" => {
                    let mut st = state.lock().await;
                    if let Some(v) = attr_get(&attrs, "Preview") {
                        println!("Client settings: preview={}", v);
                        st.preview = is_true(v);
                        return;
                    }
                    if let Some(v) = attr_get(&attrs, "Quality") {
                        println!("Client settings: quality={}", v);
                        st.suggested_quality = Some(v.clone());
                        return;
                    }
                }
                "omttally" => {
                    let mut st = state.lock().await;
                    if let Some(v) = attr_get(&attrs, "Preview") {
                        st.tally_preview = is_true(v);
                    }
                    if let Some(v) = attr_get(&attrs, "Program") {
                        st.tally_program = is_true(v);
                    }
                    return;
                }
                "omtinfo" => {
                    let mut st = state.lock().await;
                    st.sender_info_xml = Some(xml);
                    return;
                }
                "omtredirect" => {
                    let mut st = state.lock().await;
                    st.redirect_address = attr_get(&attrs, "Address").cloned();
                    return;
                }
                _ => {}
            }
        }

        // Backward-compat: keep previous prefix-based handling for non-normalized XML.
        if xml.starts_with("<OMTSettings Quality=") {
            if let Some(quality) = parse_attr(&xml, "Quality") {
                let mut st = state.lock().await;
                st.suggested_quality = Some(quality);
            }
            return;
        }
        if xml.starts_with("<OMTInfo") {
            let mut st = state.lock().await;
            st.sender_info_xml = Some(xml);
            return;
        }
        if xml.starts_with("<OMTRedirect") {
            let redirect = parse_attr(&xml, "Address");
            let mut st = state.lock().await;
            st.redirect_address = redirect;
            return;
        }
    }
}

fn extract_metadata_xml(frame: &OMTFrame) -> Option<String> {
    let bytes = if !frame.metadata.is_empty() {
        &frame.metadata
    } else if !frame.data.is_empty() && frame.header.metadata_length == 0 {
        &frame.data
    } else {
        return None;
    };
    if bytes.is_empty() {
        return None;
    }
    let nul_pos = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..nul_pos])
        .trim()
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn metadata_frame_from_xml(xml: &str) -> OMTFrame {
    let mut frame = OMTFrame::new(OMTFrameType::Metadata);
    frame.header.timestamp = 0;
    frame.data = bytes::Bytes::copy_from_slice(xml.as_bytes());
    frame.update_data_length();
    frame
}

fn parse_attr(xml: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let idx = xml.find(&needle)?;
    let mut rest = &xml[idx + needle.len()..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    rest = &rest[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].trim().to_string())
}

fn quality_level_from_name(value: &str) -> u8 {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" | "1" => 1,
        "medium" | "50" => 2,
        "high" | "100" => 3,
        _ => 0,
    }
}

async fn sync_connection_suggested_quality(
    conn_id: u64,
    state: &Arc<Mutex<SubscriptionState>>,
    metadata_state: &Arc<Mutex<ServerMetadataState>>,
    suggested_quality_hint: &Arc<AtomicU8>,
) {
    let quality = {
        let st = state.lock().await;
        if st.wants_video {
            st.suggested_quality
                .as_deref()
                .map(quality_level_from_name)
                .unwrap_or(0)
        } else {
            0
        }
    };
    {
        let mut st = metadata_state.lock().await;
        st.per_connection_suggested_quality.insert(conn_id, quality);
    }
    update_global_suggested_quality(metadata_state, suggested_quality_hint).await;
}

async fn remove_connection_suggested_quality(
    conn_id: u64,
    metadata_state: &Arc<Mutex<ServerMetadataState>>,
    suggested_quality_hint: &Arc<AtomicU8>,
) {
    {
        let mut st = metadata_state.lock().await;
        st.per_connection_suggested_quality.remove(&conn_id);
    }
    update_global_suggested_quality(metadata_state, suggested_quality_hint).await;
}

async fn update_global_suggested_quality(
    metadata_state: &Arc<Mutex<ServerMetadataState>>,
    suggested_quality_hint: &Arc<AtomicU8>,
) {
    let max_quality = {
        let st = metadata_state.lock().await;
        st.per_connection_suggested_quality
            .values()
            .copied()
            .max()
            .unwrap_or(0)
    };
    suggested_quality_hint.store(max_quality, Ordering::Relaxed);
}

fn parse_xml_tag_and_attrs(
    xml: &str,
) -> Option<(String, std::collections::HashMap<String, String>)> {
    let start = xml.find('<')?;
    let end = xml.rfind('>')?;
    if end <= start + 1 {
        return None;
    }
    let inner = xml[start + 1..end].trim().trim_end_matches('/').trim();
    if inner.is_empty() {
        return None;
    }

    let mut parts = inner.split_whitespace();
    let tag = parts.next()?.to_string();
    let mut attrs = std::collections::HashMap::new();

    let attrs_src = inner[tag.len()..].trim();
    let bytes = attrs_src.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let key_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        if i <= key_start {
            break;
        }
        let key = attrs_src[key_start..i].trim().to_string();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let quote = bytes[i];
        let value = if quote == b'"' || quote == b'\'' {
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let v = attrs_src[val_start..i.min(bytes.len())].to_string();
            if i < bytes.len() {
                i += 1;
            }
            v
        } else {
            let val_start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            attrs_src[val_start..i].to_string()
        };

        if !key.is_empty() {
            attrs.insert(key, value);
        }
    }

    Some((tag, attrs))
}

fn is_true(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn attr_true(attrs: &std::collections::HashMap<String, String>, key: &str) -> bool {
    attr_get(attrs, key).map(|v| is_true(v)).unwrap_or(false)
}

fn attr_get<'a>(
    attrs: &'a std::collections::HashMap<String, String>,
    key: &str,
) -> Option<&'a String> {
    if let Some(v) = attrs.get(key) {
        return Some(v);
    }
    let target = key.to_ascii_lowercase();
    attrs
        .iter()
        .find_map(|(k, v)| (k.to_ascii_lowercase() == target).then_some(v))
}
