use crate::channel::OMTChannel;
use crate::enums::OMTFrameType;
use crate::frame::OMTFrame;
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch, Mutex};

use crate::constants::{NETWORK_RECEIVE_BUFFER, NETWORK_SEND_BUFFER, NETWORK_SEND_RECEIVE_BUFFER};

#[derive(Clone)]
pub struct ServerSenders {
    pub video: broadcast::Sender<OMTFrame>,
    pub video_lq: broadcast::Sender<OMTFrame>,
    pub video_sq: broadcast::Sender<OMTFrame>,
    pub video_hq: broadcast::Sender<OMTFrame>,
    pub video_h264: broadcast::Sender<OMTFrame>,
    pub video_h265: broadcast::Sender<OMTFrame>,
    pub audio: broadcast::Sender<OMTFrame>,
    pub metadata: broadcast::Sender<OMTFrame>,
}

pub struct OMTServer {
    listener: TcpListener,
    tx: ServerSenders,
    metadata_state: Arc<Mutex<ServerMetadataState>>,
    suggested_quality_hint: Arc<AtomicU8>,
    active_quality_mask: Arc<AtomicU8>,
    active_codec_mask: Arc<AtomicU8>,
    /// Bitmask of codecs the encoder can actually produce: bit0=VMX1, bit1=H264, bit2=H265
    supported_codec_mask: Arc<AtomicU8>,
}

impl OMTServer {
    pub async fn new(port: u16) -> Result<Self, io::Error> {
        let addr = format!("0.0.0.0:{}", port);
        let listener = TcpListener::bind(addr).await?;

        // Keep sender-side buffering small to avoid multi-second latency.
        // IMPORTANT: video/audio are sent on separate TCP connections by the receiver, so
        // separating channels lets us tune buffering independently.
        let (tx_video, _) = broadcast::channel(8);
        let (tx_video_lq, _) = broadcast::channel(8);
        let (tx_video_sq, _) = broadcast::channel(8);
        let (tx_video_hq, _) = broadcast::channel(8);
        let (tx_video_h264, _) = broadcast::channel(8);
        let (tx_video_h265, _) = broadcast::channel(8);
        let (tx_audio, _) = broadcast::channel(256);
        let (tx_meta, _) = broadcast::channel(64);

        Ok(OMTServer {
            listener,
            tx: ServerSenders {
                video: tx_video,
                video_lq: tx_video_lq,
                video_sq: tx_video_sq,
                video_hq: tx_video_hq,
                video_h264: tx_video_h264,
                video_h265: tx_video_h265,
                audio: tx_audio,
                metadata: tx_meta,
            },
            metadata_state: Arc::new(Mutex::new(ServerMetadataState::default())),
            suggested_quality_hint: Arc::new(AtomicU8::new(0)),
            active_quality_mask: Arc::new(AtomicU8::new(0)),
            active_codec_mask: Arc::new(AtomicU8::new(1)),
            supported_codec_mask: Arc::new(AtomicU8::new(1)), // VMX1 always; updated at startup
        })
    }

    pub fn get_senders(&self) -> ServerSenders {
        self.tx.clone()
    }

    pub fn suggested_quality_hint(&self) -> Arc<AtomicU8> {
        self.suggested_quality_hint.clone()
    }

    /// Bitmask of quality levels with active receivers: bit0=LQ, bit1=SQ, bit2=HQ.
    /// SQ (bit1) is always set when any video receiver is connected.
    pub fn active_quality_mask(&self) -> Arc<AtomicU8> {
        self.active_quality_mask.clone()
    }

    /// Bitmask of codecs with active receivers: bit0=VMX1, bit1=H264, bit2=H265
    pub fn active_codec_mask(&self) -> Arc<AtomicU8> {
        self.active_codec_mask.clone()
    }

    pub fn supported_codec_mask(&self) -> Arc<AtomicU8> {
        self.supported_codec_mask.clone()
    }

    /// Set which codecs the encoder can produce. Called at startup after detecting HW encoders.
    pub fn set_supported_codecs(&self, mask: u8) {
        self.supported_codec_mask.store(mask, Ordering::Relaxed);
    }

    pub async fn get_conn_info(&self) -> Vec<ConnInfo> {
        let st = self.metadata_state.lock().await;
        st.conn_info.values().cloned().collect()
    }

    pub fn metadata_state(&self) -> Arc<Mutex<ServerMetadataState>> {
        Arc::clone(&self.metadata_state)
    }

    pub async fn set_sender_info_xml(&self, xml: Option<String>) {
        self.metadata_state.lock().await.sender_info_xml = xml.clone();
        // Match C# OMTSend.SetSenderInformation: push updates immediately to metadata subscribers.
        if let Some(xml) = xml {
            if !xml.trim().is_empty() {
                let _ = self.tx.metadata.send(metadata_frame_from_xml(&xml));
            }
        }
    }

    pub async fn set_redirect_address(&self, address: Option<String>) {
        self.metadata_state.lock().await.redirect_address = address.clone();
        // Match C# redirect metadata format: always announce the latest redirect state.
        let xml = redirect_xml(address.as_deref().unwrap_or_default());
        let _ = self.tx.metadata.send(metadata_frame_from_xml(&xml));
    }

    pub async fn set_tally(&self, preview: bool, program: bool) {
        let mut st = self.metadata_state.lock().await;
        st.tally_preview = preview;
        st.tally_program = program;
        let xml = tally_xml(preview, program);
        let _ = self.tx.metadata.send(metadata_frame_from_xml(xml));
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
            configure_sender_socket(&socket);
            println!("Accepted connection from: {}", addr);

            let tx = self.tx.clone();
            let metadata_state = Arc::clone(&self.metadata_state);
            let quality_hint = Arc::clone(&self.suggested_quality_hint);
            let quality_mask = Arc::clone(&self.active_quality_mask);
            let codec_mask = Arc::clone(&self.active_codec_mask);
            let supported_codecs = Arc::clone(&self.supported_codec_mask);
            let this_conn_id = conn_id;
            let peer_addr = addr.ip().to_string();
            conn_id = conn_id.wrapping_add(1);

            tokio::spawn(async move {
                if let Err(e) =
                    handle_connection(socket, tx, metadata_state, quality_hint, quality_mask, codec_mask, supported_codecs, this_conn_id, peer_addr).await
                {
                    eprintln!("Connection error: {}", e);
                }
            });
        }
    }
}

fn configure_sender_socket(socket: &TcpStream) {
    // Nagle algorithm enabled (TCP_NODELAY off) to reduce small packet count.
    // Audio frames (~3.9KB) span multiple MSS segments, so Nagle adds < 1ms delay.
    // Disabling NODELAY was causing excessive retransmits on Raspberry Pi.
    let _ = socket.set_nodelay(false);

    #[cfg(unix)]
    {
        use socket2::SockRef;

        let sock = SockRef::from(socket);
        let _ = sock.set_send_buffer_size(NETWORK_SEND_BUFFER);

        // Receivers use two connections: one for video/metadata, one for audio.
        // We don't know the subscription yet, so pick the larger receive buffer.
        let _ = sock.set_recv_buffer_size(NETWORK_RECEIVE_BUFFER.max(NETWORK_SEND_RECEIVE_BUFFER));

        // No keepalive — dead connections detected by send/recv failures.
    }
}

#[derive(Debug, Clone, Default, Serialize)]
struct ServerMetadataState {
    sender_info_xml: Option<String>,
    connection_metadata: Vec<String>,
    redirect_address: Option<String>,
    tally_preview: bool,
    tally_program: bool,
    per_connection_suggested_quality: HashMap<u64, u8>,
    #[serde(rename = "connInfo")]
    conn_info: HashMap<u64, ConnInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnInfo {
    pub addr: String,
    pub video: bool,
    pub audio: bool,
    pub quality: String,
    pub codec: String,
    #[serde(rename = "connectedSince")]
    pub connected_since: u64, // seconds since connection
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
    preferred_codecs: Vec<String>,
    sender_info_xml: Option<String>,
    redirect_address: Option<String>,
}

impl Default for SubscriptionState {
    fn default() -> Self {
        SubscriptionState {
            wants_video: false,
            wants_audio: false,
            wants_metadata: false,
            preview: false,
            tally_preview: false,
            tally_program: false,
            suggested_quality: None,
            preferred_codecs: vec!["vmx1".to_string()],
            sender_info_xml: None,
            redirect_address: None,
        }
    }
}

async fn handle_connection(
    socket: TcpStream,
    tx: ServerSenders,
    metadata_state: Arc<Mutex<ServerMetadataState>>,
    suggested_quality_hint: Arc<AtomicU8>,
    active_quality_mask: Arc<AtomicU8>,
    active_codec_mask: Arc<AtomicU8>,
    supported_codec_mask: Arc<AtomicU8>,
    conn_id: u64,
    peer_addr: String,
) -> Result<(), io::Error> {
    let _ = socket.set_nodelay(false);
    {
        use socket2::SockRef;
        let sock = SockRef::from(&socket);
        let _ = sock.set_send_buffer_size(NETWORK_SEND_BUFFER);
        let _ = sock.set_recv_buffer_size(NETWORK_RECEIVE_BUFFER);
    }

    let channel = OMTChannel::new(socket);
    let mut rx_video = tx.video.subscribe();
    let mut rx_video_lq = tx.video_lq.subscribe();
    let mut rx_video_sq = tx.video_sq.subscribe();
    let mut rx_video_hq = tx.video_hq.subscribe();
    let mut rx_video_h264 = tx.video_h264.subscribe();
    let mut rx_video_h265 = tx.video_h265.subscribe();
    let mut rx_audio = tx.audio.subscribe();
    let mut rx_meta = tx.metadata.subscribe();

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
        xmls.push(tally_xml(st.tally_preview, st.tally_program).to_string());
        if let Some(addr) = st.redirect_address {
            xmls.push(redirect_xml(&addr));
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
    let (tx_wants_video, mut rx_wants_video) = watch::channel(false);

    {
        let mut st = metadata_state.lock().await;
        st.per_connection_suggested_quality.insert(conn_id, 0);
        st.conn_info.insert(conn_id, ConnInfo {
            addr: peer_addr.clone(),
            video: false,
            audio: false,
            quality: "Default".to_string(),
            codec: "VMX1".to_string(),
            connected_since: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        });
    }
    update_global_suggested_quality(&metadata_state, &suggested_quality_hint, &active_quality_mask).await;

    // Low-latency send model (matches C# sender behavior):
    // - Video is "latest-wins" (no queueing): if the connection can't keep up, we prefer the newest
    //   frame over building latency.
    // - Audio/metadata are queued with a small bound; if full, we drop to avoid runaway latency.
    let (tx_video_latest, mut rx_video_latest) = watch::channel::<Option<OMTFrame>>(None);
    let (tx_other, mut rx_other) = mpsc::channel::<OMTFrame>(512);
    let (tx_quality_changed, mut rx_quality_changed) = watch::channel(0u8);
    let (tx_codec_changed, mut rx_codec_changed) = watch::channel(0u8); // 0=vmx1,1=h264,2=h265

    // Producer: Broadcast -> per-connection channels (non-blocking).
    let prod_state = Arc::clone(&state);
    let connection_closed = Arc::new(AtomicBool::new(false));
    let producer_closed = Arc::clone(&connection_closed);
    let prod_conn_id = conn_id;
    let producer = tokio::spawn(async move {
        let mut current_quality: u8 = 0;
        let mut current_codec: u8 = 0; // 0=vmx1, 1=h264, 2=h265

        while !producer_closed.load(Ordering::Relaxed) {
            // Pick video channel based on codec + quality
            let frame = tokio::select! {
                // VMX1 quality channels
                v = rx_video_sq.recv(), if current_codec == 0 && (current_quality == 0 || current_quality == 2) => v,
                v = rx_video_lq.recv(), if current_codec == 0 && current_quality == 1 => v,
                v = rx_video_hq.recv(), if current_codec == 0 && current_quality >= 3 => v,
                // H264/H265 channels
                v = rx_video_h264.recv(), if current_codec == 1 => v,
                v = rx_video_h265.recv(), if current_codec == 2 => v,
                a = rx_audio.recv() => a,
                m = rx_meta.recv() => m,
                changed = rx_quality_changed.changed() => {
                    if changed.is_ok() {
                        current_quality = *rx_quality_changed.borrow();
                    }
                    continue;
                }
                changed = rx_codec_changed.changed() => {
                    if changed.is_ok() {
                        current_codec = *rx_codec_changed.borrow();
                    }
                    continue;
                }
            };
            let frame = match frame {
                Ok(frame) => frame,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };

            let allowed = {
                let st = prod_state.lock().await;
                match frame.header.frame_type {
                    OMTFrameType::Metadata => true,
                    OMTFrameType::Video => st.wants_video,
                    OMTFrameType::Audio => st.wants_audio,
                    _ => false,
                }
            };
            if !allowed {
                continue;
            }

            match frame.header.frame_type {
                OMTFrameType::Video => {
                    let _ = tx_video_latest.send_replace(Some(frame));
                }
                OMTFrameType::Audio | OMTFrameType::Metadata => {
                    let _ = tx_other.try_send(frame);
                }
                _ => {}
            }
        }
    });

    // Writer: per-connection channels -> TCP.
    let write_state = Arc::clone(&state);
    let write_conn_id = conn_id;
    let mut send_task = tokio::spawn(async move {
        let mut wants_video = *rx_wants_video.borrow();
        let mut slow_write_count: u64 = 0;
        let mut audio_frames_this_sec: u64 = 0;
        let mut unflushed_audio: u32 = 0;
        let mut last_write_diag = tokio::time::Instant::now();
        let mut last_fps_log = tokio::time::Instant::now();
        loop {
            if last_fps_log.elapsed().as_secs() >= 1 {
                if audio_frames_this_sec > 0 {
                    eprintln!(
                        "CONN {} WRITER: {} audio frames/sec",
                        write_conn_id, audio_frames_this_sec
                    );
                }
                audio_frames_this_sec = 0;
                last_fps_log = tokio::time::Instant::now();
            }
            if last_write_diag.elapsed().as_secs() >= 10 {
                if slow_write_count > 0 {
                    eprintln!(
                        "CONN {} WRITER DIAG 10s: slow_writes={}",
                        write_conn_id, slow_write_count
                    );
                    slow_write_count = 0;
                }
                last_write_diag = tokio::time::Instant::now();
            }
            if wants_video {
                // Biased select: prioritize video over audio/metadata to minimize video latency.
                // When both are ready, always send the latest video frame first.
                tokio::select! {
                    biased;
                    changed = rx_video_latest.changed() => {
                        if changed.is_err() {
                            break;
                        }

                        let frame = match rx_video_latest.borrow().as_ref() {
                            Some(f) => f.clone(),
                            None => continue,
                        };
                        let frame = {
                            let st = write_state.lock().await;
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
                        let t = tokio::time::Instant::now();
                        sink.send(frame).await?;
                        let e = t.elapsed();
                        if e.as_millis() > 10 {
                            slow_write_count += 1;
                            eprintln!(
                                "CONN {} WRITER: video sink.send took {}ms",
                                write_conn_id, e.as_millis()
                            );
                        }
                    }
                    maybe = rx_other.recv() => {
                        let Some(frame) = maybe else { break; };
                        let ft = frame.header.frame_type;
                        let t = tokio::time::Instant::now();
                        sink.send(frame).await?;
                        let e = t.elapsed();
                        if ft == OMTFrameType::Audio {
                            audio_frames_this_sec += 1;
                        }
                        if e.as_millis() > 10 {
                            slow_write_count += 1;
                            eprintln!(
                                "CONN {} WRITER: {:?} sink.send took {}ms",
                                write_conn_id, ft, e.as_millis()
                            );
                        }
                    }
                    changed = rx_wants_video.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        wants_video = *rx_wants_video.borrow();
                    }
                }
            } else {
                tokio::select! {
                    changed = rx_wants_video.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        wants_video = *rx_wants_video.borrow();
                    }
                    maybe = rx_other.recv() => {
                        let Some(frame) = maybe else { break; };
                        if frame.header.frame_type == OMTFrameType::Audio {
                            audio_frames_this_sec += 1;
                        }
                        sink.send(frame).await?;
                    }
                }
            }
        }
        Ok::<(), io::Error>(())
    });

    let mut terminal_error: Option<io::Error> = None;
    loop {
        tokio::select! {
            send_res = &mut send_task => {
                match send_res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => terminal_error = Some(e),
                    Err(e) => {
                        terminal_error = Some(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            format!("send task join error: {e}"),
                        ));
                    }
                }
                break;
            }
            recv_res = stream.next() => {
                let Some(result) = recv_res else { break; };
                match result {
                    Ok(frame) => {
                        handle_subscription_frame(conn_id, &frame, &state).await;
                        let (wants_video, wants_audio, quality, quality_name, codec_id, codec_name) = {
                            let st = state.lock().await;
                            let q = st.suggested_quality.as_deref()
                                .map(quality_level_from_name).unwrap_or(0);
                            let qn = st.suggested_quality.clone().unwrap_or_else(|| "Default".to_string());
                            // Negotiate codec: pick first client-preferred that we actually support
                            let sup = supported_codec_mask.load(Ordering::Relaxed);
                            let neg = negotiate_codec(&st.preferred_codecs, sup);
                            let cid: u8 = match neg.as_str() { "h264" => 1, "h265" => 2, _ => 0 };
                            let cn = neg.to_uppercase();
                            (st.wants_video, st.wants_audio, q, qn, cid, cn)
                        };
                        let _ = tx_wants_video.send(wants_video);
                        let _ = tx_quality_changed.send(quality);
                        let _ = tx_codec_changed.send(codec_id);
                        // Update conn_info
                        {
                            let mut ms = metadata_state.lock().await;
                            if let Some(ci) = ms.conn_info.get_mut(&conn_id) {
                                ci.video = wants_video;
                                ci.audio = wants_audio;
                                ci.quality = quality_name;
                                ci.codec = codec_name;
                            }
                        }
                        sync_connection_suggested_quality(
                            conn_id,
                            &state,
                            &metadata_state,
                            &suggested_quality_hint,
                            &active_quality_mask,
                        )
                        .await;
                        // Update codec mask
                        {
                            let st = metadata_state.lock().await;
                            let mut cm: u8 = 0;
                            for ci in st.conn_info.values() {
                                if ci.video {
                                    cm |= codec_name_to_bit(&ci.codec.to_lowercase());
                                }
                            }
                            if cm != 0 { cm |= 1; }
                            active_codec_mask.store(cm, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        terminal_error = Some(e);
                        break;
                    }
                }
            }
        }
    }

    connection_closed.store(true, Ordering::Relaxed);
    producer.abort();
    send_task.abort();
    let _ = producer.await;
    let _ = send_task.await;

    remove_connection_suggested_quality(conn_id, &metadata_state, &suggested_quality_hint, &active_quality_mask).await;
    { metadata_state.lock().await.conn_info.remove(&conn_id); }
    if let Some(e) = terminal_error {
        return Err(e);
    }
    Ok(())
}

async fn handle_subscription_frame(
    conn_id: u64,
    frame: &OMTFrame,
    state: &Arc<Mutex<SubscriptionState>>,
) {
    if let Some(xml) = extract_metadata_xml(frame) {
        // Fast path for exact protocol constants used by C# sender/receiver.
        // These are compared by exact string in the original implementation.
        match xml.as_str() {
            "<OMTSubscribe Video=\"true\" />" => {
                println!("[conn {conn_id}] Client subscribed: video");
                state.lock().await.wants_video = true;
                return;
            }
            "<OMTSubscribe Audio=\"true\" />" => {
                println!("[conn {conn_id}] Client subscribed: audio");
                state.lock().await.wants_audio = true;
                return;
            }
            "<OMTSubscribe Metadata=\"true\" />" => {
                println!("[conn {conn_id}] Client subscribed: metadata");
                state.lock().await.wants_metadata = true;
                return;
            }
            "<OMTSettings Preview=\"true\" />" => {
                println!("[conn {conn_id}] Client settings: preview=true");
                state.lock().await.preview = true;
                return;
            }
            "<OMTSettings Preview=\"false\" />" => {
                println!("[conn {conn_id}] Client settings: preview=false");
                state.lock().await.preview = false;
                return;
            }
            "<OMTTally Preview=\"true\" Program=\"true\" />"
            | "<OMTTally Preview=\"true\" Program==\"true\" />" => {
                let mut st = state.lock().await;
                st.tally_preview = true;
                st.tally_program = true;
                return;
            }
            "<OMTTally Preview=\"false\" Program=\"true\" />"
            | "<OMTTally Preview=\"false\" Program==\"true\" />" => {
                let mut st = state.lock().await;
                st.tally_preview = false;
                st.tally_program = true;
                return;
            }
            "<OMTTally Preview=\"true\" Program=\"false\" />"
            | "<OMTTally Preview=\"true\" Program==\"false\" />" => {
                let mut st = state.lock().await;
                st.tally_preview = true;
                st.tally_program = false;
                return;
            }
            "<OMTTally Preview=\"false\" Program=\"false\" />"
            | "<OMTTally Preview=\"false\" Program==\"false\" />" => {
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
                        println!("[conn {conn_id}] Client subscribed: video");
                        st.wants_video = true;
                    }
                    if attr_true(&attrs, "Audio") {
                        println!("[conn {conn_id}] Client subscribed: audio");
                        st.wants_audio = true;
                    }
                    if attr_true(&attrs, "Metadata") {
                        println!("[conn {conn_id}] Client subscribed: metadata");
                        st.wants_metadata = true;
                    }
                    return;
                }
                "omtsettings" => {
                    let mut st = state.lock().await;
                    if let Some(v) = attr_get(&attrs, "Preview") {
                        println!("[conn {conn_id}] Client settings: preview={}", v);
                        st.preview = is_true(v);
                        return;
                    }
                    if let Some(v) = attr_get(&attrs, "Quality") {
                        println!("[conn {conn_id}] Client settings: quality={}", v);
                        st.suggested_quality = Some(v.clone());
                    }
                    if let Some(v) = attr_get(&attrs, "Codec") {
                        println!("[conn {conn_id}] Client settings: codec={}", v);
                        st.preferred_codecs = parse_codec_preference(v);
                    }
                    return;
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
                    st.redirect_address = attr_get(&attrs, "NewAddress")
                        .or_else(|| attr_get(&attrs, "Address"))
                        .cloned();
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
            let redirect = parse_attr(&xml, "NewAddress").or_else(|| parse_attr(&xml, "Address"));
            let mut st = state.lock().await;
            st.redirect_address = redirect;
            return;
        }
    }
}

fn tally_xml(preview: bool, program: bool) -> &'static str {
    // Keep wire-compatibility with C# constants (including historical Program== formatting).
    match (preview, program) {
        (true, true) => "<OMTTally Preview=\"true\" Program==\"true\" />",
        (false, true) => "<OMTTally Preview=\"false\" Program==\"true\" />",
        (true, false) => "<OMTTally Preview=\"true\" Program==\"false\" />",
        (false, false) => "<OMTTally Preview=\"false\" Program==\"false\" />",
    }
}

fn redirect_xml(address: &str) -> String {
    format!(
        "<OMTRedirect NewAddress=\"{}\" />",
        xml_escape_attr(address)
    )
}

fn xml_escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
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
    // libomtnet (C# reference) sends metadata XML as a UTF-8 byte blob without a trailing NUL.
    // Receivers perform exact string matches for several protocol commands; adding a NUL would
    // break those comparisons.
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
    active_quality_mask: &Arc<AtomicU8>,
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
    update_global_suggested_quality(metadata_state, suggested_quality_hint, &active_quality_mask).await;
}

async fn remove_connection_suggested_quality(
    conn_id: u64,
    metadata_state: &Arc<Mutex<ServerMetadataState>>,
    suggested_quality_hint: &Arc<AtomicU8>,
    active_quality_mask: &Arc<AtomicU8>,
) {
    {
        let mut st = metadata_state.lock().await;
        st.per_connection_suggested_quality.remove(&conn_id);
    }
    update_global_suggested_quality(metadata_state, suggested_quality_hint, &active_quality_mask).await;
}

async fn update_global_suggested_quality(
    metadata_state: &Arc<Mutex<ServerMetadataState>>,
    suggested_quality_hint: &Arc<AtomicU8>,
    active_quality_mask: &Arc<AtomicU8>,
) {
    let (max_quality, mask, cmask) = {
        let st = metadata_state.lock().await;
        let max = st.per_connection_suggested_quality
            .values()
            .copied()
            .max()
            .unwrap_or(0);
        let mut m: u8 = 0;
        for &q in st.per_connection_suggested_quality.values() {
            match q {
                1 => m |= 1,
                2 => m |= 2,
                3.. => m |= 4,
                _ => m |= 2,
            }
        }
        if m != 0 { m |= 2; }

        // Compute active codec mask from conn_info
        let mut cm: u8 = 0;
        for ci in st.conn_info.values() {
            if ci.video {
                cm |= codec_name_to_bit(&ci.codec.to_lowercase());
            }
        }
        // Always include VMX1 if any video receiver
        if cm != 0 { cm |= 1; }
        (max, m, cm)
    };
    suggested_quality_hint.store(max_quality, Ordering::Relaxed);
    active_quality_mask.store(mask, Ordering::Relaxed);
    // Store codec mask if Arc is available in scope (passed via handle_connection)
    // This is stored by the caller separately.
}

fn parse_codec_preference(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Map codec name to bit position: vmx1=0, h264=1, h265=2
fn codec_name_to_bit(name: &str) -> u8 {
    match name {
        "h264" => 2,
        "h265" => 4,
        _ => 1, // vmx1 and unknown
    }
}

/// Negotiate: pick the first client-preferred codec that the encoder supports.
/// Returns the codec name (lowercase).
pub fn negotiate_codec(preferred: &[String], supported_mask: u8) -> String {
    for c in preferred {
        let bit = codec_name_to_bit(c);
        if supported_mask & bit != 0 {
            return c.clone();
        }
    }
    "vmx1".to_string()
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
