use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use crate::frame::OMTFrame;
use crate::channel::OMTChannel;
use std::io;
use futures::{SinkExt, StreamExt};

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

async fn handle_connection(socket: TcpStream, tx: broadcast::Sender<OMTFrame>) -> Result<(), io::Error> {
    // Configure socket buffers
    // Note: OMT logic sets specific buffer sizes
    // We can rely on OS defaults or set them if critical via socket2 crate or implicit behavior.
    
    let channel = OMTChannel::new(socket);
    let mut rx = tx.subscribe();

    let (mut sink, mut stream) = channel.into_split();

    // Spawn send task (Broadcast -> TCP)
    let send_task = tokio::spawn(async move {
        while let Ok(frame) = rx.recv().await {
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
                // Process incoming frame (e.g. metadata updates from client)
                // For now, assume read-only/log
                println!("Received frame type: {:?}", frame.header.frame_type);
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    let _ = send_task.await;
    Ok(())
}
