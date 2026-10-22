//! Attaching a terminal to the login console of a VM in the background.
//!
//! The `sendit run` process of a VM in the background listens on a Unix
//! socket in the VM's run directory, and `sendit attach` connects to it.
//! The server's first byte says whether the client is let in: one client
//! at a time. After that, both send messages: the client keystrokes and the
//! terminal's size, the server the console's output, and that the console's
//! user logged out. There is no key to detach: every key goes to the guest.
//! Closing the terminal detaches it, and so does logging out, after which
//! the guest logs in on the console again for the next client.

use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

use super::console::{
    RawMode, ScreenTracker, pipe, poll, pollfd, read, winch_pipe, window_size, write_all,
};
use super::{CatchSignals, clear_screen, signals};
use crate::util;

/// How long the attached client may take to accept output before it is
/// detached: the console's output, and with it the log, waits meanwhile.
pub(super) const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// The server's first byte: the client is attached.
const ACCEPTED: u8 = b'=';
/// The server's first byte: another client is attached already.
const BUSY: u8 = b'-';

/// Tags of the client's messages.
const INPUT: u8 = b'i';
const SIZE: u8 = b's';
/// Tags of the server's messages.
const OUTPUT: u8 = b'o';
const LOGGED_OUT: u8 = b'l';

/// A tag and a big-endian `u32` length precede each message's payload.
const HEADER: usize = 5;

/// The longest payload a message may have.
const MAX_FRAME: u32 = 64 * 1024;

/// What the client and the server send each other.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Message {
    /// To the server: typed into the terminal.
    Input(Vec<u8>),
    /// To the server: the terminal's size.
    Size { rows: u16, cols: u16 },
    /// To the client: the console's output.
    Output(Vec<u8>),
    /// To the client, last: the console's user logged out.
    LoggedOut,
}

impl Message {
    pub(super) fn encode(&self) -> Vec<u8> {
        let (tag, payload) = match self {
            Self::Input(data) => (INPUT, data.clone()),
            Self::Size { rows, cols } => (SIZE, [rows.to_be_bytes(), cols.to_be_bytes()].concat()),
            Self::Output(data) => (OUTPUT, data.clone()),
            Self::LoggedOut => (LOGGED_OUT, Vec::new()),
        };
        let mut bytes = vec![tag];
        bytes.extend((payload.len() as u32).to_be_bytes());
        bytes.extend(payload);
        bytes
    }
}

/// Reassembles messages from what arrives of them.
#[derive(Default)]
pub(super) struct Frames {
    buf: Vec<u8>,
}

impl Frames {
    pub(super) fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The next complete message, if there is one.
    pub(super) fn next(&mut self) -> Result<Option<Message>> {
        if self.buf.len() < HEADER {
            return Ok(None);
        }
        let tag = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]);
        ensure!(
            matches!(tag, INPUT | SIZE | OUTPUT | LOGGED_OUT),
            "unknown message {tag:#x}"
        );
        ensure!(len <= MAX_FRAME, "message of {len} bytes");
        ensure!(tag != SIZE || len == 4, "size of {len} bytes");
        ensure!(tag != LOGGED_OUT || len == 0, "logout of {len} bytes");
        let end = HEADER + len as usize;
        if self.buf.len() < end {
            return Ok(None);
        }
        let payload = self.buf[HEADER..end].to_vec();
        self.buf.drain(..end);
        Ok(Some(match tag {
            INPUT => Message::Input(payload),
            SIZE => Message::Size {
                rows: u16::from_be_bytes([payload[0], payload[1]]),
                cols: u16::from_be_bytes([payload[2], payload[3]]),
            },
            OUTPUT => Message::Output(payload),
            _ => Message::LoggedOut,
        }))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The attached client, as far as the console's output is concerned.
#[derive(Default)]
pub(super) struct ClientSlot(Mutex<Option<UnixStream>>);

impl ClientSlot {
    pub(super) fn set(&self, stream: Option<UnixStream>) {
        *lock(&self.0) = stream;
    }

    /// Sends the console's output `data` to the client, if one is
    /// attached. One that can't take it is detached.
    pub(super) fn send(&self, data: &[u8]) {
        let mut slot = lock(&self.0);
        if let Some(stream) = slot.as_mut()
            && !send_within(
                stream,
                &Message::Output(data.to_vec()).encode(),
                CLIENT_WRITE_TIMEOUT,
            )
        {
            detach(&mut slot);
        }
    }

    /// Tells the client, if one is attached, that the console's user logged
    /// out, and detaches it.
    pub(super) fn log_out(&self) {
        let mut slot = lock(&self.0);
        if let Some(stream) = slot.as_mut() {
            send_within(stream, &Message::LoggedOut.encode(), CLIENT_WRITE_TIMEOUT);
            detach(&mut slot);
        }
    }
}

/// Ends the connection to the client in `slot`, if any.
fn detach(slot: &mut Option<UnixStream>) {
    if let Some(stream) = slot.take() {
        // The server reads from its own copy, and sees it end.
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// Writes all of `data` to `stream` within `timeout`; false if it can't.
/// A write timeout alone would apply to each write, and a partial one
/// would start it over.
fn send_within(stream: &mut UnixStream, mut data: &[u8], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !data.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_write_timeout(Some(left)).is_err() {
            return false;
        }
        match stream.write(data) {
            Ok(0) => return false,
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

/// The attached client's terminal size, for the guest's console.
pub(super) struct RemoteSize {
    sizes: Mutex<Sizes>,
    /// Readable when sizes are pending; whoever polls it reads it.
    wake: OwnedFd,
    wake_write: OwnedFd,
}

#[derive(Default)]
struct Sizes {
    current: Option<(u16, u16)>,
    /// Not sent to the guest yet, oldest first.
    pending: Vec<(u16, u16)>,
}

impl RemoteSize {
    pub(super) fn new() -> io::Result<Self> {
        let (wake, wake_write) = pipe()?;
        // One pending wakeup is as good as many.
        // SAFETY: plain fcntl calls on a descriptor we own.
        unsafe {
            let flags = libc::fcntl(wake_write.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(
                wake_write.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            );
        }
        Ok(Self {
            sizes: Mutex::default(),
            wake,
            wake_write,
        })
    }

    pub(super) fn set(&self, rows: u16, cols: u16) {
        self.push(&[(rows, cols)]);
    }

    /// Sets the size so that programs on the console redraw, even if it
    /// hasn't changed: they only learn of a change (SIGWINCH), so the size
    /// is one row off for a moment first. For a terminal that attaches,
    /// whose screen starts out empty.
    pub(super) fn set_redrawing(&self, rows: u16, cols: u16) {
        let off = if rows > 1 { rows - 1 } else { rows + 1 };
        self.push(&[(off, cols), (rows, cols)]);
    }

    fn push(&self, sizes: &[(u16, u16)]) {
        let mut state = lock(&self.sizes);
        state.current = sizes.last().copied();
        state.pending.extend_from_slice(sizes);
        let _ = write_all(self.wake_write.as_raw_fd(), &[0]);
    }

    pub(super) fn current(&self) -> Option<(u16, u16)> {
        lock(&self.sizes).current
    }

    pub(super) fn take_pending(&self) -> Vec<(u16, u16)> {
        std::mem::take(&mut lock(&self.sizes).pending)
    }

    pub(super) fn wake_fd(&self) -> libc::c_int {
        self.wake.as_raw_fd()
    }
}

/// The socket clients attach through. Dropping it removes the socket.
pub(super) struct Server {
    path: PathBuf,
}

impl Server {
    /// Listens on `path`, replacing whatever was left there, and serves
    /// clients until `stop` closes: their keystrokes go to `guest_input`,
    /// their terminal's size to `size`, and the attached one is in
    /// `client`. Binding changes the working directory for a moment (see
    /// `util::in_dir`), so no other thread may have started yet.
    pub(super) fn start(
        path: &Path,
        guest_input: OwnedFd,
        client: Arc<ClientSlot>,
        size: Arc<RemoteSize>,
        stop: OwnedFd,
    ) -> Result<Self> {
        util::if_exists(fs::remove_file(path))
            .with_context(|| format!("removing {}", path.display()))?;
        let (dir, name) = (path.parent(), path.file_name());
        let (Some(dir), Some(name)) = (dir, name) else {
            anyhow::bail!("{} is no socket path", path.display());
        };
        let listener = util::in_dir(dir, || UnixListener::bind(name))
            .with_context(|| format!("listening on {}", path.display()))?;
        std::thread::spawn(move || serve(listener, guest_input, &client, &size, stop));
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Serves clients on `listener` until `stop` closes, as `Server::start`
/// says.
fn serve(
    listener: UnixListener,
    guest_input: OwnedFd,
    client: &ClientSlot,
    size: &RemoteSize,
    stop: OwnedFd,
) {
    // The client, what it sent of its next message, and whether it has
    // sent its terminal's size yet.
    let mut attached: Option<(UnixStream, Frames, bool)> = None;
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = vec![pollfd(stop.as_raw_fd()), pollfd(listener.as_raw_fd())];
        if let Some((stream, ..)) = &attached {
            fds.push(pollfd(stream.as_raw_fd()));
        }
        if poll(&mut fds, -1).is_err() || fds[0].revents != 0 {
            break;
        }
        // The attached client first: one that has gone makes room for the
        // next.
        if let Some((stream, frames, sized)) = &mut attached
            && fds[2].revents != 0
        {
            let still = match read(stream.as_raw_fd(), &mut buf) {
                Ok(n @ 1..) => {
                    frames.push(&buf[..n]);
                    forward(frames, guest_input.as_raw_fd(), size, sized)
                }
                _ => false,
            };
            if !still {
                client.set(None);
                attached = None;
            }
        }
        if fds[1].revents != 0
            && let Ok((mut stream, _)) = listener.accept()
        {
            if attached.is_some() {
                let _ = stream.write_all(&[BUSY]);
            } else if stream.write_all(&[ACCEPTED]).is_ok()
                && let Ok(copy) = stream.try_clone()
            {
                // After the greeting: output must not come before it.
                client.set(Some(copy));
                attached = Some((stream, Frames::default(), false));
            }
        }
    }
    client.set(None);
}

/// Passes on the complete messages in `frames`; false if the client sent
/// something that isn't one, or the guest's console is gone. The client's
/// first size, once it has `sized`, makes the console redraw.
fn forward(
    frames: &mut Frames,
    guest_input: libc::c_int,
    size: &RemoteSize,
    sized: &mut bool,
) -> bool {
    loop {
        match frames.next() {
            Ok(Some(Message::Input(data))) => {
                if write_all(guest_input, &data).is_err() {
                    return false;
                }
            }
            Ok(Some(Message::Size { rows, cols })) if *sized => size.set(rows, cols),
            Ok(Some(Message::Size { rows, cols })) => {
                size.set_redrawing(rows, cols);
                *sized = true;
            }
            Ok(None) => return true,
            Ok(Some(Message::Output(_) | Message::LoggedOut)) | Err(_) => return false,
        }
    }
}

/// How attaching ended.
#[derive(Debug)]
pub enum Attached {
    /// Another terminal is attached already.
    Busy,
    /// This terminal went away, or was told to (SIGHUP, SIGTERM, SIGINT).
    Detached,
    /// The console's user logged out; the VM keeps running.
    LoggedOut,
    /// The VM stopped, or its console dropped this terminal.
    Stopped,
}

/// Attaches this terminal to the console of the VM in the background whose
/// server listens on `socket`, until the VM stops, its console's user logs
/// out, or the terminal goes away.
/// Every key goes to the guest. stdin and stdout must be the terminal.
/// Once it is let in, `notice` goes on top of the cleared screen.
pub fn attach(socket: &Path, notice: &str) -> Result<Attached> {
    let (Some(dir), Some(name)) = (socket.parent(), socket.file_name()) else {
        anyhow::bail!("{} is no socket path", socket.display());
    };
    let mut stream = util::in_dir(dir, || UnixStream::connect(name))
        .with_context(|| format!("connecting to {}", socket.display()))?;
    let mut greeting = [0u8];
    stream
        .read_exact(&mut greeting)
        .context("the VM's console closed before it let this terminal in")?;
    match greeting[0] {
        BUSY => return Ok(Attached::Busy),
        ACCEPTED => {}
        other => anyhow::bail!(
            "unexpected greeting {other:#x} from the VM's console; it may have been \
             started by another version of sendit"
        ),
    }

    let resized = winch_pipe()?;
    let (first_signal, _) = signals();
    let _signals = CatchSignals::new();
    clear_screen();
    let _ = writeln!(io::stderr(), "{notice}");
    let mut raw_mode = RawMode::enable(false)?;
    let result = (|| {
        if let Some((rows, cols)) = window_size() {
            (&stream).write_all(&Message::Size { rows, cols }.encode())?;
        }
        relay(
            libc::STDIN_FILENO,
            libc::STDOUT_FILENO,
            &stream,
            resized,
            first_signal,
        )
    })();
    if let (Some(raw_mode), Ok((_, alternate))) = (&mut raw_mode, &result) {
        raw_mode.alternate_screen = *alternate;
    }
    result.map(|(ended, _)| ended)
}

/// Relays `input` (stdin) to the VM's console on `socket` and its output to
/// `output` (stdout), and the terminal's size whenever `resized` is
/// signalled, until either side ends, the console's user logs out, or a
/// signal comes after the first `first_signal`. Returns how it ended, and
/// whether the output left the terminal on the alternate screen.
fn relay(
    input: libc::c_int,
    output: libc::c_int,
    socket: &UnixStream,
    resized: libc::c_int,
    first_signal: usize,
) -> Result<(Attached, bool)> {
    let mut to_vm = socket;
    let mut screen = ScreenTracker::default();
    let mut frames = Frames::default();
    let mut fds = [pollfd(input), pollfd(socket.as_raw_fd()), pollfd(resized)];
    let mut buf = [0u8; 4096];
    let ended = loop {
        // Signals don't interrupt the wait, so it has a timeout.
        if signals().0 > first_signal {
            break Attached::Detached;
        }
        if poll(&mut fds, 200).context("waiting for the terminal")? == 0 {
            continue;
        }
        // The VM's output first, up to its end.
        if fds[1].revents != 0 {
            let Ok(n @ 1..) = read(socket.as_raw_fd(), &mut buf) else {
                break Attached::Stopped;
            };
            frames.push(&buf[..n]);
            if let Some(ended) = show(&mut frames, output, &mut screen)? {
                break ended;
            }
        }
        if fds[0].revents != 0 {
            match read(input, &mut buf) {
                Ok(n @ 1..) => {
                    let message = Message::Input(buf[..n].to_vec());
                    if to_vm.write_all(&message.encode()).is_err() {
                        break Attached::Stopped;
                    }
                }
                _ => break Attached::Detached,
            }
        }
        if fds[2].revents != 0 {
            let _ = read(resized, &mut buf);
            if let Some((rows, cols)) = window_size()
                && to_vm
                    .write_all(&Message::Size { rows, cols }.encode())
                    .is_err()
            {
                break Attached::Stopped;
            }
        }
    };
    Ok((ended, screen.alternate))
}

/// Writes the console's output in `frames` to `output`, as far as it has
/// arrived. Returns how attaching ended, if it has.
fn show(
    frames: &mut Frames,
    output: libc::c_int,
    screen: &mut ScreenTracker,
) -> Result<Option<Attached>> {
    while let Some(message) = frames.next().context("reading from the VM's console")? {
        match message {
            Message::Output(data) => {
                screen.feed(&data);
                if write_all(output, &data).is_err() {
                    return Ok(Some(Attached::Detached));
                }
            }
            Message::LoggedOut => return Ok(Some(Attached::LoggedOut)),
            Message::Input(_) | Message::Size { .. } => {
                anyhow::bail!("unexpected message from the VM's console")
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempDir;
    use std::fs::File;
    use std::io::Read;

    #[test]
    fn frames_survive_split_reads() {
        let mut bytes = Message::Input(b"ls\r\x03\x1d".to_vec()).encode();
        bytes.extend(
            Message::Size {
                rows: 30,
                cols: 100,
            }
            .encode(),
        );
        bytes.extend(Message::Output(b"\x1b[?1049h".to_vec()).encode());
        bytes.extend(Message::LoggedOut.encode());
        let mut frames = Frames::default();
        let mut got = Vec::new();
        for byte in bytes {
            frames.push(&[byte]);
            while let Some(message) = frames.next().unwrap() {
                got.push(message);
            }
        }
        assert_eq!(
            got,
            [
                Message::Input(b"ls\r\x03\x1d".to_vec()),
                Message::Size {
                    rows: 30,
                    cols: 100
                },
                Message::Output(b"\x1b[?1049h".to_vec()),
                Message::LoggedOut,
            ]
        );

        let mut unknown = Frames::default();
        unknown.push(b"x\0\0\0\0");
        assert!(unknown.next().is_err());
        let mut short_size = Frames::default();
        short_size.push(b"s\0\0\0\x02ab");
        assert!(short_size.next().is_err());
        let mut too_long = Frames::default();
        too_long.push(b"i\x7f\0\0\0");
        assert!(too_long.next().is_err());
        let mut long_logout = Frames::default();
        long_logout.push(b"l\0\0\0\x01x");
        assert!(long_logout.next().is_err());
    }

    #[test]
    fn relays_every_key_and_the_vms_output() {
        let (input, typed) = pipe().unwrap();
        let (shown, output) = pipe().unwrap();
        let (socket, mut server) = UnixStream::pair().unwrap();
        let (resized, _resize) = pipe().unwrap();
        let (input_fd, output_fd) = (input.as_raw_fd(), output.as_raw_fd());
        let thread = std::thread::spawn(move || {
            let first_signal = signals().0;
            let result = relay(
                input_fd,
                output_fd,
                &socket,
                resized.as_raw_fd(),
                first_signal,
            );
            drop((input, output));
            result.unwrap()
        });

        // No key detaches: Ctrl-C and Ctrl-] go to the guest too.
        write_all(typed.as_raw_fd(), b"ls\r\x03\x1d").unwrap();
        let mut frames = Frames::default();
        let mut forwarded = Vec::new();
        let mut buf = [0u8; 64];
        while forwarded.len() < 5 {
            let n = server.read(&mut buf).unwrap();
            frames.push(&buf[..n]);
            while let Some(message) = frames.next().unwrap() {
                let Message::Input(data) = message else {
                    panic!("{message:?}")
                };
                forwarded.extend(data);
            }
        }
        assert_eq!(forwarded, b"ls\r\x03\x1d");

        // Split across reads.
        let sent = Message::Output(b"\x1b[?1049hhi".to_vec()).encode();
        server.write_all(&sent[..3]).unwrap();
        server.write_all(&sent[3..]).unwrap();
        let mut got = [0u8; 10];
        File::from(shown).read_exact(&mut got).unwrap();
        assert_eq!(&got, b"\x1b[?1049hhi");
        // The VM stopped, on the alternate screen.
        drop(server);
        assert!(matches!(thread.join().unwrap(), (Attached::Stopped, true)));

        // The console's user logged out, after the last output.
        let (input, _typed) = pipe().unwrap();
        let (shown, output) = pipe().unwrap();
        let (socket, mut server) = UnixStream::pair().unwrap();
        let (resized, _resize) = pipe().unwrap();
        let mut sent = Message::Output(b"logout\r\n".to_vec()).encode();
        sent.extend(Message::LoggedOut.encode());
        server.write_all(&sent).unwrap();
        let ended = relay(
            input.as_raw_fd(),
            output.as_raw_fd(),
            &socket,
            resized.as_raw_fd(),
            signals().0,
        )
        .unwrap();
        assert!(matches!(ended, (Attached::LoggedOut, false)));
        drop(output);
        let mut out = Vec::new();
        File::from(shown).read_to_end(&mut out).unwrap();
        assert_eq!(out, b"logout\r\n");

        // The terminal went away.
        let (input, typed) = pipe().unwrap();
        let (_shown, output) = pipe().unwrap();
        let (socket, _server) = UnixStream::pair().unwrap();
        let (resized, _resize) = pipe().unwrap();
        drop(typed);
        let ended = relay(
            input.as_raw_fd(),
            output.as_raw_fd(),
            &socket,
            resized.as_raw_fd(),
            signals().0,
        )
        .unwrap();
        assert!(matches!(ended, (Attached::Detached, false)));
    }

    #[test]
    fn redraws_with_a_size_change() {
        let size = RemoteSize::new().unwrap();
        size.set_redrawing(30, 100);
        assert_eq!(size.take_pending(), [(29, 100), (30, 100)]);
        assert_eq!(size.current(), Some((30, 100)));
        size.set_redrawing(1, 80);
        assert_eq!(size.take_pending(), [(2, 80), (1, 80)]);
        size.set(1, 81);
        assert_eq!(size.take_pending(), [(1, 81)]);
    }

    /// Retries `f` for up to a second until it returns true.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn serves_one_client_at_a_time() {
        // Deeper than a socket's path may be.
        let temp = TempDir::new("attach");
        let dir = temp.path().join("d".repeat(60)).join("e".repeat(60));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("console.sock");
        // Left over from a process that didn't get to remove it.
        fs::write(&path, "").unwrap();
        let (guest_reads, guest_input) = pipe().unwrap();
        let (stop_read, stop) = pipe().unwrap();
        let client = Arc::new(ClientSlot::default());
        let size = Arc::new(RemoteSize::new().unwrap());
        let server =
            Server::start(&path, guest_input, client.clone(), size.clone(), stop_read).unwrap();
        let connect = || util::in_dir(&dir, || UnixStream::connect("console.sock")).unwrap();
        let greeting = |stream: &mut UnixStream| {
            let mut byte = [0u8];
            stream.read_exact(&mut byte).map(|()| byte[0]).ok()
        };

        let mut first = connect();
        assert_eq!(greeting(&mut first), Some(ACCEPTED));
        first
            .write_all(&Message::Input(b"ls\r".to_vec()).encode())
            .unwrap();
        first
            .write_all(
                &Message::Size {
                    rows: 30,
                    cols: 100,
                }
                .encode(),
            )
            .unwrap();
        let mut typed = [0u8; 3];
        let mut guest_reads = File::from(guest_reads);
        guest_reads.read_exact(&mut typed).unwrap();
        assert_eq!(&typed, b"ls\r");
        assert!(eventually(|| size.current() == Some((30, 100))));
        // What the VM writes reaches the attached client.
        client.send(b"output");
        let sent = Message::Output(b"output".to_vec()).encode();
        let mut shown = vec![0u8; sent.len()];
        first.read_exact(&mut shown).unwrap();
        assert_eq!(shown, sent);

        let mut second = connect();
        assert_eq!(greeting(&mut second), Some(BUSY));

        // Once the first one has gone, the next one is let in.
        drop(first);
        let mut third = None;
        assert!(eventually(|| {
            let mut stream = connect();
            let accepted = greeting(&mut stream) == Some(ACCEPTED);
            third = accepted.then_some(stream);
            accepted
        }));
        let mut third = third.unwrap();
        // Once its size has arrived, it is surely the client.
        third
            .write_all(
                &Message::Size {
                    rows: 31,
                    cols: 101,
                }
                .encode(),
            )
            .unwrap();
        assert!(eventually(|| size.current() == Some((31, 101))));
        // Logged out, it is told so and detached, and the next is let in.
        client.log_out();
        let mut got = Vec::new();
        third.read_to_end(&mut got).unwrap();
        assert_eq!(got, Message::LoggedOut.encode());
        assert!(eventually(|| greeting(&mut connect()) == Some(ACCEPTED)));

        drop(stop);
        drop(server);
        assert!(!path.exists());
    }
}
