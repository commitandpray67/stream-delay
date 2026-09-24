//! What the OS still holds of what was written to a TCP connection: data not
//! sent yet, and data sent but not yet acknowledged by the other end.
//!
//! A dump uses it to know whether media from before it may still leave this
//! computer (then the connection is reset, which drops it), and which media the
//! destination surely has.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// See the module documentation. In bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SendQueue {
    pub unsent: u64,
    pub unacked: u64,
}

/// A stream that counts the bytes the OS accepted for sending: `written` for
/// [`query`]. Below TLS, so it counts what the connection carries.
pub(crate) struct Counted<S> {
    inner: S,
    written: Arc<AtomicU64>,
}

impl<S> Counted<S> {
    pub fn new(inner: S, written: Arc<AtomicU64>) -> Self {
        Self { inner, written }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = r {
            self.written.fetch_add(n as u64, Ordering::Relaxed);
        }
        r
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// What the OS holds for `sock`, of which `written` bytes were accepted so far.
/// `None` where it cannot tell.
pub(crate) fn query(sock: &socket2::Socket, written: u64) -> Option<SendQueue> {
    os::query(sock, written)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(unsafe_code)]
mod os {
    use std::os::fd::AsRawFd;

    use super::SendQueue;

    /// Unsent bytes in the send queue (linux/sockios.h).
    const SIOCOUTQNSD: libc::Ioctl = 0x894B;

    pub fn query(sock: &socket2::Socket, _written: u64) -> Option<SendQueue> {
        let fd = sock.as_raw_fd();
        let mut queued: libc::c_int = 0;
        let mut unsent: libc::c_int = 0;
        // SAFETY: both requests store one int through the pointer, which is
        // valid for writing one.
        let ok = unsafe {
            libc::ioctl(fd, libc::TIOCOUTQ, &mut queued as *mut libc::c_int) == 0
                && libc::ioctl(fd, SIOCOUTQNSD, &mut unsent as *mut libc::c_int) == 0
        };
        ok.then(|| SendQueue {
            unsent: unsent.max(0) as u64,
            unacked: (queued - unsent).max(0) as u64,
        })
    }
}

#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
mod os {
    use std::os::fd::AsRawFd;

    use super::SendQueue;

    pub fn query(sock: &socket2::Socket, written: u64) -> Option<SendQueue> {
        // SAFETY: a C struct of integers, for which all zeroes is valid.
        let mut info: libc::tcp_connection_info = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::tcp_connection_info>() as libc::socklen_t;
        // SAFETY: the pointer and length describe `info`, which the call fills.
        let r = unsafe {
            libc::getsockopt(
                sock.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_CONNECTION_INFO,
                (&mut info as *mut libc::tcp_connection_info).cast(),
                &mut len,
            )
        };
        if r != 0 {
            return None;
        }
        // Bytes sent at least once: never more than was written, unless the
        // counts are not what they seem, and then it cannot tell.
        let sent = info
            .tcpi_txbytes
            .checked_sub(info.tcpi_txretransmitbytes)
            .filter(|&sent| sent <= written)?;
        let unsent = written - sent;
        // The send buffer holds what is unsent and what is unacknowledged.
        let unacked = u64::from(info.tcpi_snd_sbbytes).saturating_sub(unsent);
        Some(SendQueue { unsent, unacked })
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod os {
    use std::os::windows::io::AsRawSocket;

    use windows_sys::Win32::Networking::WinSock::{SIO_TCP_INFO, SOCKET, TCP_INFO_v0, WSAIoctl};

    use super::SendQueue;

    pub fn query(sock: &socket2::Socket, written: u64) -> Option<SendQueue> {
        let version: u32 = 0;
        // SAFETY: a C struct of integers, for which all zeroes is valid.
        let mut info: TCP_INFO_v0 = unsafe { std::mem::zeroed() };
        let mut returned: u32 = 0;
        // SAFETY: the input and output pointers and lengths describe `version`
        // and `info`; the call is synchronous (no overlapped structure).
        let r = unsafe {
            WSAIoctl(
                sock.as_raw_socket() as SOCKET,
                SIO_TCP_INFO,
                (&version as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
                (&mut info as *mut TCP_INFO_v0).cast(),
                std::mem::size_of::<TCP_INFO_v0>() as u32,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if r != 0 {
            return None;
        }
        // Bytes sent at least once. The count of those sent again is only 32
        // bits: past 4 GiB of retransmissions it wraps, which shows as more
        // sent than was written, and then it cannot tell.
        let sent = info
            .BytesOut
            .checked_sub(u64::from(info.BytesRetrans))
            .filter(|&sent| sent <= written)?;
        Some(SendQueue {
            unsent: written - sent,
            unacked: u64::from(info.BytesInFlight),
        })
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
mod os {
    use super::SendQueue;

    pub fn query(_sock: &socket2::Socket, _written: u64) -> Option<SendQueue> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    use super::*;

    /// Checks the OS's numbers against a connection whose other end first
    /// stops reading, then reads everything.
    #[test]
    fn the_os_says_what_it_still_holds() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let sock = socket2::Socket::from(client.try_clone().unwrap());
        client.set_nonblocking(true).unwrap();
        // Written while the other end reads nothing, until the OS takes no more.
        let mut written = 0u64;
        let chunk = vec![7u8; 64 * 1024];
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match client.write(&chunk) {
                Ok(n) => written += n as u64,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
            assert!(Instant::now() < deadline, "the OS kept taking data");
        }
        // Let the connection settle into its stalled state.
        std::thread::sleep(Duration::from_millis(200));
        let q = query(&sock, written).expect("the OS answers");
        assert!(
            q.unsent > 0,
            "{q:?} after {written} bytes to a stalled reader"
        );

        // Once the other end has read everything, nothing is left.
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut got = 0u64;
        let mut buf = vec![0u8; 256 * 1024];
        while got < written {
            got += server.read(&mut buf).unwrap() as u64;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let q = query(&sock, written).expect("the OS answers");
            if q == (SendQueue {
                unsent: 0,
                unacked: 0,
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "still held after reading all: {q:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
