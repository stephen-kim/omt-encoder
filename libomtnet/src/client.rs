use crate::channel::OMTChannel;
use crate::codec::OMTFrameCodec;
use crate::frame::OMTFrame;
use futures::{SinkExt, StreamExt};
use std::io;
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

pub struct OMTClient {
    sink: futures::stream::SplitSink<Framed<TcpStream, OMTFrameCodec>, OMTFrame>,
    stream: futures::stream::SplitStream<Framed<TcpStream, OMTFrameCodec>>,
}

impl OMTClient {
    pub async fn connect(addr: &str) -> Result<Self, io::Error> {
        let socket = TcpStream::connect(addr).await?;
        let channel = OMTChannel::new(socket);
        let (sink, stream) = channel.into_split();
        Ok(OMTClient { sink, stream })
    }

    pub async fn send(&mut self, frame: OMTFrame) -> Result<(), io::Error> {
        self.sink.send(frame).await
    }

    pub async fn receive(&mut self) -> Option<Result<OMTFrame, io::Error>> {
        self.stream.next().await
    }
}
