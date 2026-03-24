use crate::codec::OMTFrameCodec;
use crate::constants;
use crate::frame::OMTFrame;
use futures::{SinkExt, StreamExt};
use std::io;
use std::os::fd::AsRawFd;
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

pub struct OMTChannel {
    stream: Framed<TcpStream, OMTFrameCodec>,
}

impl OMTChannel {
    pub fn new(stream: TcpStream) -> Self {
        let _ = stream.set_nodelay(true);
        let _ = apply_socket_options(&stream);
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

    pub fn into_split(
        self,
    ) -> (
        futures::stream::SplitSink<Framed<TcpStream, OMTFrameCodec>, OMTFrame>,
        futures::stream::SplitStream<Framed<TcpStream, OMTFrameCodec>>,
    ) {
        self.stream.split()
    }
}

fn apply_socket_options(stream: &TcpStream) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    set_sockopt_int(
        fd,
        libc::SOL_SOCKET,
        libc::SO_SNDBUF,
        constants::NETWORK_SEND_BUFFER as libc::c_int,
    )?;
    set_sockopt_int(
        fd,
        libc::SOL_SOCKET,
        libc::SO_RCVBUF,
        constants::NETWORK_RECEIVE_BUFFER as libc::c_int,
    )?;
    // No TCP keepalive — dead connections are detected by send/recv failures.
    // Keepalive probes were causing periodic audio glitches.

    Ok(())
}

fn set_sockopt_int(
    fd: std::os::fd::RawFd,
    level: libc::c_int,
    optname: libc::c_int,
    value: libc::c_int,
) -> io::Result<()> {
    let ret = unsafe {
        libc::setsockopt(
            fd,
            level,
            optname,
            &value as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
