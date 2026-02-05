use tokio::net::TcpStream;
use tokio_util::codec::Framed;
use futures::{SinkExt, StreamExt};
use crate::codec::OMTFrameCodec;
use crate::frame::OMTFrame;
use std::io;

pub struct OMTChannel {
    stream: Framed<TcpStream, OMTFrameCodec>,
}

impl OMTChannel {
    pub fn new(stream: TcpStream) -> Self {
        OMTChannel {
            stream: Framed::new(stream, OMTFrameCodec),
        }
    }

    pub async fn send(&mut self, frame: OMTFrame) -> Result<(), io::Error> {
        self.stream.send(frame).await
    }

    pub async fn receive(&mut self) -> Option<Result<OMTFrame, io::Error>> {
        self.stream.next().await
    }

    pub fn into_split(self) -> (futures::stream::SplitSink<Framed<TcpStream, OMTFrameCodec>, OMTFrame>, futures::stream::SplitStream<Framed<TcpStream, OMTFrameCodec>>) {
        self.stream.split()
    }
}
