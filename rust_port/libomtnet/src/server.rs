use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use crate::frame::OMTFrame;
use crate::channel::OMTChannel;
use std::io;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use crate::enums::OMTFrameType;

pub struct OMTServer {
    listener: TcpListener,
    tx: broadcast::Sender<OMTFrame>,
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
        })
    }

    pub fn get_sender(&self) -> broadcast::Sender<OMTFrame> {
        self.tx.clone()
    }

    pub async fn run(&self) -> Result<(), io::Error> {
        loop {
            let (socket, addr) = self.listener.accept().await?;
            println!("Accepted connection from: {}", addr);

            let tx = self.tx.clone();
            
            tokio::spawn(async move {
                if let Err(e) = handle_connection(socket, tx).await {
                    eprintln!("Connection error: {}", e);
                }
            });
        }
    }
}

#[derive(Debug, Clone)]
struct SubscriptionState {
    wants_video: bool,
    wants_audio: bool,
    wants_metadata: bool,
    preview: bool,
}

impl Default for SubscriptionState {
    fn default() -> Self {
        SubscriptionState {
            wants_video: true,
            wants_audio: true,
            wants_metadata: false,
            preview: false,
        }
    }
}

async fn handle_connection(socket: TcpStream, tx: broadcast::Sender<OMTFrame>) -> Result<(), io::Error> {
    // Configure socket buffers
    // Note: OMT logic sets specific buffer sizes
    // We can rely on OS defaults or set them if critical via socket2 crate or implicit behavior.
    
    let channel = OMTChannel::new(socket);
    let mut rx = tx.subscribe();

    let (mut sink, mut stream) = channel.into_split();
    let state = Arc::new(Mutex::new(SubscriptionState::default()));

    // Spawn send task (Broadcast -> TCP)
    let send_state = Arc::clone(&state);
    let send_task = tokio::spawn(async move {
        while let Ok(frame) = rx.recv().await {
            let allowed = {
                let state = send_state.lock().await;
                match frame.header.frame_type {
                    OMTFrameType::Video => state.wants_video,
                    OMTFrameType::Audio => state.wants_audio,
                    OMTFrameType::Metadata => state.wants_metadata,
                    _ => false,
                }
            };
            if !allowed {
                continue;
            }
            if let Err(e) = sink.send(frame).await {
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
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    let _ = send_task.await;
    Ok(())
}

async fn handle_subscription_frame(frame: &OMTFrame, state: &Arc<Mutex<SubscriptionState>>) {
    if let Some(xml) = extract_metadata_xml(frame) {
        if xml.contains("<OMTSubscribe") {
            let mut updated = false;
            let mut state = state.lock().await;
            if let Some(v) = parse_bool_attr(&xml, "Video") {
                state.wants_video = v;
                updated = true;
            }
            if let Some(v) = parse_bool_attr(&xml, "Audio") {
                state.wants_audio = v;
                updated = true;
            }
            if let Some(v) = parse_bool_attr(&xml, "Metadata") {
                state.wants_metadata = v;
                updated = true;
            }
            if updated {
                println!(
                    "Client subscriptions updated: video={}, audio={}, metadata={}",
                    state.wants_video, state.wants_audio, state.wants_metadata
                );
            }
        }
        if xml.contains("<OMTSettings") {
            if let Some(v) = parse_bool_attr(&xml, "Preview") {
                let mut state = state.lock().await;
                state.preview = v;
                println!("Client preview setting: {}", state.preview);
            }
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
    let s = String::from_utf8_lossy(&bytes[..nul_pos]).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn parse_bool_attr(xml: &str, name: &str) -> Option<bool> {
    let needle = format!(\"{}=\", name);
    let idx = xml.find(&needle)?;
    let rest = &xml[idx + needle.len()..];
    let quote = rest.chars().next()?;
    if quote != '\"' && quote != '\\'' {
        return None;
    }
    let rest = &rest[1..];
    let end = rest.find(quote)?;
    let value = rest[..end].trim().to_lowercase();
    match value.as_str() {
        \"true\" => Some(true),
        \"false\" => Some(false),
        _ => None,
    }
}
