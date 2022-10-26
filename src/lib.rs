//!
//! Minimal non-blocking I/O library.
//!
//! ## Example: reading from *stdin*
//!
//! ```
//! use std::{io, io::prelude::*, process, time};
//!
//! fn main() -> io::Result<()> {
//!     // Create a registry to hold I/O sources.
//!     let mut poll = popol::Poll::with_capacity(1);
//!
//!     // Register the program's standard input as a source of "read" readiness events.
//!     poll.register((), &io::stdin(), popol::event::READ);
//!
//!     // Wait on our event sources for at most 6 seconds. If an event source is
//!     // ready before then, process its events. Otherwise, timeout.
//!     if poll.wait_timeout(popol::Timeout::from_secs(6))? {
//!         process::exit(1);
//!     }
//!
//!     // Iterate over source events. Since we only have one source
//!     // registered, this will only iterate once.
//!     for ((), event) in &poll {
//!         // The standard input has data ready to be read.
//!         if event.is_readable() || event.has_hangup() {
//!             let mut buf = [0; 1024];
//!
//!             // Read what we can from standard input and echo it.
//!             match io::stdin().read(&mut buf[..]) {
//!                 Ok(n) => io::stdout().write_all(&buf[..n])?,
//!                 Err(err) => panic!("{}", err),
//!             }
//!         }
//!     }
//!
//!     Ok(())
//! }
//! ```
#![deny(missing_docs)]
#![allow(clippy::new_without_default)]
#![allow(clippy::comparison_chain)]
use std::io;
use std::io::prelude::*;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub use event::Event;

/// Source readiness interest.
pub mod event {
    /// Poll that can be waited for.
    pub type Event = libc::c_short;

    /// The associated file is ready to be read.
    pub const READ: Event = POLLIN | POLLPRI;
    /// The associated file is ready to be written.
    pub const WRITE: Event = POLLOUT | libc::POLLWRBAND;
    /// The associated file is ready.
    pub const ALL: Event = READ | WRITE;
    /// Don't wait for any events.
    pub const NONE: Event = 0x0;

    // NOTE: POLLERR, POLLNVAL and POLLHUP are ignored as *interests*, and will
    // always be set automatically in the output events.

    /// The associated file is available for read operations.
    const POLLIN: Event = libc::POLLIN;
    /// There is urgent data available for read operations.
    const POLLPRI: Event = libc::POLLPRI;
    /// The associated file is available for write operations.
    const POLLOUT: Event = libc::POLLOUT;
}

/// Optional timeout.
///
/// Note that the maximum timeout is `i32::MAX` milliseconds (about 25 days). Longer
/// timeouts will be silently clipped to `i32::MAX` milliseconds.
#[derive(Debug, Clone)]
pub enum Timeout {
    /// Timeout after a specific duration.
    After(Duration),
    /// Never timeout.
    Never,
}

impl Timeout {
    /// Create a timeout with the specified number of seconds.
    ///
    /// See [`Timeout`] for an important note about the maximum timeout.
    pub fn from_secs(seconds: u32) -> Self {
        Self::After(Duration::from_secs(seconds as u64))
    }

    /// Create a timeout with the specified number of milliseconds.
    ///
    /// See [`Timeout`] for an important note about the maximum timeout.
    pub fn from_millis(milliseconds: u32) -> Self {
        Self::After(Duration::from_millis(milliseconds as u64))
    }
}

impl From<Duration> for Timeout {
    /// Create a timeout from a duration.
    ///
    /// See [`Timeout`] for an important note about the maximum timeout.
    fn from(duration: Duration) -> Self {
        Self::After(duration)
    }
}

impl From<Option<Duration>> for Timeout {
    /// Create a timeout from an optional duration.
    ///
    /// See [`Timeout`] for an important note about the maximum timeout.
    fn from(duration: Option<Duration>) -> Self {
        match duration {
            Some(duration) => Self::from(duration),
            None => Self::Never,
        }
    }
}

/// Poll descriptor accepted by poll syscall, containing list of subscribed
/// and read events.
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct PollFd {
    fd: RawFd,
    events: Event,
    revents: Event,
}

impl PollFd {
    fn new(fd: RawFd, events: Event) -> Self {
        Self {
            fd,
            events,
            revents: 0,
        }
    }

    /// Return the source from the underlying raw file descriptor.
    ///
    /// # Safety
    ///
    /// Calls `FromRawFd::from_raw_fd`. The returned object will cause
    /// the file to close when dropped.
    pub unsafe fn to_raw_fd<T: FromRawFd>(&self) -> T {
        T::from_raw_fd(self.fd)
    }

    /// Set events to wait for on this source.
    pub fn set(&mut self, events: Event) {
        self.events |= events;
    }

    /// Unset events to wait for on this source.
    pub fn unset(&mut self, events: Event) {
        self.events &= !events;
    }

    /// The source is writable.
    pub fn is_writable(self) -> bool {
        self.revents & event::WRITE != 0
    }

    /// The source is readable.
    pub fn is_readable(self) -> bool {
        self.revents & event::READ != 0
    }

    /// The source has be disconnected.
    pub fn has_hangup(self) -> bool {
        self.revents & libc::POLLHUP != 0
    }

    /// An error has occurred on the source.
    pub fn has_errored(self) -> bool {
        self.revents & libc::POLLERR != 0
    }

    /// The sourc is not valid.
    pub fn is_invalid(self) -> bool {
        self.revents & libc::POLLNVAL != 0
    }

    /// Check whether the event is an error. Returns true if the underlying
    /// source is invalid, or if an error occurred on it.
    pub fn is_err(&self) -> bool {
        self.has_errored() || self.is_invalid()
    }
}

/// Trait providing indication of the presence of polled events in a file descriptor
pub trait HasPoll {
    /// Returns information whether file descriptor has generated events
    fn has_events(&self) -> bool;
}

impl HasPoll for PollFd {
    fn has_events(&self) -> bool {
        self.revents != 0
    }
}

impl HasPoll for &PollFd {
    fn has_events(&self) -> bool {
        self.revents != 0
    }
}

impl AsRawFd for &PollFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

/// Keeps track of sources to poll.
#[derive(Debug, Clone)]
pub struct Poll<K> {
    /// Number of generated events.
    events_count: usize,
    /// Tracks the keys assigned to each source.
    index: Vec<K>,
    /// List of sources passed to `poll`.
    list: Vec<PollFd>,
}

impl<K> Poll<K> {
    /// Creates a new set of sources to poll.
    pub fn new() -> Self {
        Self {
            events_count: 0,
            index: vec![],
            list: vec![],
        }
    }

    /// Creates a new set of sources to poll, with the given capacity.
    /// Use this if you have a lot of sources to poll.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            events_count: 0,
            index: Vec::with_capacity(cap),
            list: Vec::with_capacity(cap),
        }
    }

    /// Return the number of registered sources.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Return whether the source registry is empty.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Return whether recent [`Self::poll`] returned any events from the descriptors.
    pub fn has_events(&self) -> bool {
        self.events_count > 0
    }

    /// Returns count of the events detected by the last [`Self::poll`] request.
    pub fn events_count(&self) -> usize {
        self.events_count
    }

    /// Returns iterator over the sources indexed by their keys
    pub fn events(&self) -> AsIter<K> {
        AsIter {
            keys: self.index.iter(),
            list: self.list.iter(),
        }
    }

    /// Register a new source, with the given key, and wait for the specified events.
    ///
    /// Care must be taken not to register the same source twice, or use the same key
    /// for two different sources.
    ///
    /// Resets the information about previously collected events.
    pub fn register(&mut self, key: K, fd: &impl AsRawFd, events: Event) {
        self.reset();
        self.insert(key, PollFd::new(fd.as_raw_fd(), events));
    }

    /// Wait for readiness events on the given list of sources. Does not timeout; i.e. may block
    /// the thread forever.
    ///
    /// Resets the information about previously collected events.
    pub fn wait(&mut self) -> Result<(), io::Error> {
        debug_assert!(self.wait_timeout(Timeout::Never)?);
        Ok(())
    }

    /// Wait for readiness events on the given list of sources. If no event
    /// is returned within the given timeout, returns an error of kind [`io::ErrorKind::TimedOut`].
    ///
    /// Returns if the request has timed out.
    ///
    /// Resets the information about previously collected events.
    pub fn wait_timeout(&mut self, timeout: Timeout) -> Result<bool, io::Error> {
        self.reset();

        let timeout = match timeout {
            Timeout::After(duration) => duration.as_millis() as libc::c_int,
            Timeout::Never => -1,
        };

        // SAFETY: required for FFI; shouldn't break rust guarantees.
        let result = unsafe {
            libc::poll(
                self.list.as_mut_ptr() as *mut libc::pollfd,
                self.list.len() as libc::nfds_t,
                timeout,
            )
        };

        if result == 0 {
            if self.is_empty() {
                Ok(false)
            } else {
                Ok(true)
            }
        } else if result > 0 {
            self.events_count = result as usize;
            Ok(false)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Resets the information about previously collected events.
    ///
    /// Returns count of previously collected events.
    pub fn reset(&mut self) -> usize {
        let count = self.events_count;
        for fd in &mut self.list {
            fd.revents = 0;
        }
        self.events_count = 0;
        count
    }

    fn insert(&mut self, key: K, source: PollFd) {
        self.index.push(key);
        self.list.push(source);
    }
}

impl<K: PartialEq> Poll<K> {
    /// Unregister a source, given its key.
    ///
    /// Resets the information about previously collected events.
    pub fn unregister(&mut self, key: &K) {
        self.reset();
        if let Some(ix) = self.find(key) {
            self.index.swap_remove(ix);
            self.list.swap_remove(ix);
        }
    }

    /// Set the events to poll for on a source identified by its key.
    ///
    /// Resets the information about previously collected events.
    pub fn set(&mut self, key: &K, events: Event) -> bool {
        self.reset();
        if let Some(ix) = self.find(key) {
            self.list[ix].set(events);
            return true;
        }
        false
    }

    /// Unset event interests on a source.
    ///
    /// Resets the information about previously collected events.
    pub fn unset(&mut self, key: &K, events: Event) -> bool {
        self.reset();
        if let Some(ix) = self.find(key) {
            self.list[ix].unset(events);
            return true;
        }
        false
    }

    /// Get a source by key.
    pub fn get(&self, key: &K) -> Option<&PollFd> {
        self.find(key).map(move |ix| &self.list[ix])
    }

    /// Get a mutable reference for a source by key.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut PollFd> {
        self.find(key).map(move |ix| &mut self.list[ix])
    }

    fn find(&self, key: &K) -> Option<usize> {
        self.index.iter().position(|k| k == key)
    }
}

/// Iterator over all events indexed by the source keys
pub struct Iter<I1: Iterator, I2: Iterator<Item = Fd>, Fd: HasPoll> {
    /// Tracks the keys assigned to each source.
    keys: I1,
    /// List of sources passed to `poll`.
    list: I2,
}

/// Iterator returned by [`Sources::iter`] and [`Poll::iter`]
pub type AsIter<'a, K> = Iter<std::slice::Iter<'a, K>, std::slice::Iter<'a, PollFd>, &'a PollFd>;
/// Iterator returned by [`Sources::into_iter`] and [`Poll::into_iter`]
pub type IntoIter<K> = Iter<std::vec::IntoIter<K>, std::vec::IntoIter<PollFd>, PollFd>;

impl<I1: Iterator, I2: Iterator<Item = Fd>, Fd: HasPoll> Iterator for Iter<I1, I2, Fd> {
    type Item = (I1::Item, I2::Item);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (k, v) = (self.keys.next(), self.list.next());
            match (k, v) {
                (None, None) => return None,
                (Some(k), Some(v)) if v.has_events() => return Some((k, v)),
                (Some(_), Some(_)) => continue,
                _ => panic!("broken iterator"),
            }
        }
    }
}

impl<K> IntoIterator for Poll<K> {
    type Item = <IntoIter<K> as Iterator>::Item;
    type IntoIter = IntoIter<K>;

    fn into_iter(self) -> Self::IntoIter {
        Iter {
            keys: self.index.into_iter(),
            list: self.list.into_iter(),
        }
    }
}

impl<'a, K> IntoIterator for &'a Poll<K> {
    type Item = <AsIter<'a, K> as Iterator>::Item;
    type IntoIter = AsIter<'a, K>;

    fn into_iter(self) -> Self::IntoIter {
        Iter {
            keys: self.index.iter(),
            list: self.list.iter(),
        }
    }
}

/// Wakers are used to wake up `wait`.
pub struct Waker {
    reader: UnixStream,
    writer: UnixStream,
}

impl Waker {
    /// Create a new `Waker`.
    ///
    /// # Examples
    ///
    /// Wake a `wait` call from another thread.
    ///
    /// ```
    /// fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     use std::thread;
    ///     use std::time::Duration;
    ///     use std::sync::Arc;
    ///
    ///     use popol::{Poll, Waker, Timeout};
    ///
    ///     const WAKER: &'static str = "waker";
    ///
    ///     let mut poller = Poll::new();
    ///
    ///     // Create a waker and keep it alive until the end of the program, so that
    ///     // the reading end doesn't get closed.
    ///     let waker = Arc::new(Waker::new(&mut poller, WAKER)?);
    ///     let _waker = waker.clone();
    ///
    ///     let handle = thread::spawn(move || {
    ///         thread::sleep(Duration::from_millis(160));
    ///
    ///         // Wake up popol on the main thread.
    ///         _waker.wake().expect("waking shouldn't fail");
    ///     });
    ///
    ///     // Wait to be woken up by the other thread. Otherwise, time out.
    ///     poller.wait_timeout(Timeout::from_secs(1))?;
    ///
    ///     assert!(!poller.is_empty(), "There should be at least one event selected");
    ///
    ///     let mut events = poller.events();
    ///     let (key, event) = events.next().unwrap();
    ///
    ///     assert!(key == &WAKER, "The event is triggered by the waker");
    ///     assert!(event.is_readable(), "The event is readable");
    ///     assert!(events.next().is_none(), "There was only one event");
    ///
    ///     handle.join().unwrap();
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn new<K: Eq + Clone>(sources: &mut Poll<K>, key: K) -> io::Result<Waker> {
        let (writer, reader) = UnixStream::pair()?;
        let fd = reader.as_raw_fd();

        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;

        sources.insert(key, PollFd::new(fd, event::READ));

        Ok(Waker { reader, writer })
    }

    /// Wake up a waker. Causes `popol::wait` to return with a readiness
    /// event for this waker.
    pub fn wake(&self) -> io::Result<()> {
        use io::ErrorKind::*;

        match (&self.writer).write_all(&[0x1]) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == WouldBlock => {
                Waker::reset(self.reader.as_raw_fd())?;
                self.wake()
            }
            Err(e) if e.kind() == Interrupted => self.wake(),
            Err(e) => Err(e),
        }
    }

    /// Reset the waker by draining the receive buffer.
    pub fn reset(fd: impl AsRawFd) -> io::Result<()> {
        let mut buf = [0u8; 4096];

        loop {
            // We use a low-level "read" here because the alternative is to create a `UnixStream`
            // from the `RawFd`, which has "drop" semantics which we want to avoid.
            match unsafe {
                libc::read(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            } {
                -1 => match io::Error::last_os_error() {
                    e if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    e => return Err(e),
                },
                0 => return Ok(()),
                _ => continue,
            }
        }
    }
}

/// Set non-blocking mode on a stream.
///
/// This is a convenience function if the source of your stream doesn't provide an
/// easy way to set it into non-blocking mode.
///
/// ## Example
///
/// ```
/// use std::process;
/// use popol::set_nonblocking;
///
/// let child = process::Command::new("ls")
///     .stdout(process::Stdio::piped())
///     .spawn()
///     .unwrap();
/// let out = child.stdout.unwrap();
///
/// set_nonblocking(&out, true).unwrap();
/// ```
///
/// ## Return
///
/// On Linux, this should always return `Ok(0)` or `Err(_)`. On other operating systems,
/// consult the `fcntl(2)` man page.
pub fn set_nonblocking(fd: &dyn AsRawFd, nonblocking: bool) -> io::Result<i32> {
    let fd = fd.as_raw_fd();

    // SAFETY: required for FFI; shouldn't break rust guarantees.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }

    let flags = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };

    // SAFETY: required for FFI; shouldn't break rust guarantees.
    match unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } {
        -1 => Err(io::Error::last_os_error()),
        result => Ok(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_readable() -> io::Result<()> {
        let (writer0, reader0) = UnixStream::pair()?;
        let (writer1, reader1) = UnixStream::pair()?;
        let (writer2, reader2) = UnixStream::pair()?;

        let mut poller = Poll::new();

        for reader in &[&reader0, &reader1, &reader2] {
            reader.set_nonblocking(true)?;
        }

        poller.register("reader0", &reader0, event::READ);
        poller.register("reader1", &reader1, event::READ);
        poller.register("reader2", &reader2, event::READ);

        {
            assert!(poller.wait_timeout(Timeout::from_millis(1)).unwrap());
            assert!(!poller.has_events());
        }

        let tests = &mut [
            (&writer0, &reader0, "reader0", 0x1u8),
            (&writer1, &reader1, "reader1", 0x2u8),
            (&writer2, &reader2, "reader2", 0x3u8),
        ];

        for (mut writer, mut reader, key, byte) in tests.iter_mut() {
            let mut buf = [0u8; 1];

            assert!(matches!(
                reader.read(&mut buf[..]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock
            ));

            writer.write_all(&[*byte])?;

            poller.wait_timeout(Timeout::from_millis(1))?;
            assert!(poller.has_events());

            let mut iter = poller.events();
            let (k, event) = iter.next().unwrap();

            assert_eq!(&k, &key);
            assert!(
                event.is_readable()
                    && !event.is_writable()
                    && !event.has_errored()
                    && !event.has_hangup()
            );
            assert!(iter.next().is_none());

            assert_eq!(reader.read(&mut buf[..])?, 1);
            assert_eq!(&buf[..], &[*byte]);
        }
        Ok(())
    }

    #[test]
    fn test_empty() -> io::Result<()> {
        let mut poller = Poll::<()>::new();

        poller
            .wait_timeout(Timeout::from_millis(1))
            .expect("no error if nothing registered");

        assert!(!poller.has_events());

        Ok(())
    }

    #[test]
    fn test_timeout() -> io::Result<()> {
        let mut poll = Poll::new();

        poll.register((), &io::stdin(), event::READ);

        assert!(poll.wait_timeout(Timeout::from_millis(1)).unwrap());

        assert_eq!(poll.len(), 1);
        assert!(!poll.has_events());

        Ok(())
    }

    #[test]
    fn test_threaded() -> io::Result<()> {
        let (writer0, reader0) = UnixStream::pair()?;
        let (writer1, reader1) = UnixStream::pair()?;
        let (writer2, reader2) = UnixStream::pair()?;

        let mut poll = Poll::new();
        let readers = &[&reader0, &reader1, &reader2];

        for reader in readers {
            reader.set_nonblocking(true)?;
        }

        poll.register("reader0", &reader0, event::READ);
        poll.register("reader1", &reader1, event::READ);
        poll.register("reader2", &reader2, event::READ);

        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(8));

            for writer in &mut [&writer1, &writer2, &writer0] {
                writer.write_all(&[1]).unwrap();
                writer.write_all(&[2]).unwrap();
            }
        });

        let mut closed = vec![];
        while closed.len() < readers.len() {
            poll.wait_timeout(Timeout::from_millis(64))?;

            for (key, event) in poll.events() {
                assert!(event.is_readable());
                assert!(!event.is_writable());
                assert!(!event.has_errored());

                if event.has_hangup() {
                    closed.push(key.to_owned());
                    continue;
                }

                let mut buf = [0u8; 2];
                let mut reader = match *key {
                    "reader0" => &reader0,
                    "reader1" => &reader1,
                    "reader2" => &reader2,
                    _ => unreachable!(),
                };
                let n = reader.read(&mut buf[..])?;

                assert_eq!(n, 2);
                assert_eq!(&buf[..], &[1, 2]);
            }
        }
        handle.join().unwrap();

        Ok(())
    }

    #[test]
    fn test_unregister() -> io::Result<()> {
        use std::collections::HashSet;

        let (mut writer0, reader0) = UnixStream::pair()?;
        let (mut writer1, reader1) = UnixStream::pair()?;
        let (writer2, reader2) = UnixStream::pair()?;

        let mut poll = Poll::new();

        for reader in &[&reader0, &reader1, &reader2] {
            reader.set_nonblocking(true)?;
        }

        poll.register("reader0", &reader0, event::READ);
        poll.register("reader1", &reader1, event::READ);
        poll.register("reader2", &reader2, event::READ);

        {
            assert!(poll.wait_timeout(Timeout::from_millis(1)).unwrap());
            assert!(!poll.has_events());
        }

        {
            writer1.write_all(&[0x0])?;

            poll.wait_timeout(Timeout::from_millis(1))?;
            let (key, _) = poll.events().next().unwrap();

            assert_eq!(key, &"reader1");
        }

        // Unregister.
        {
            poll.unregister(&"reader1");
            writer1.write_all(&[0x0])?;

            poll.wait_timeout(Timeout::from_millis(1)).ok();
            assert!(poll.events().next().is_none());

            for w in &mut [&writer0, &writer1, &writer2] {
                w.write_all(&[0])?;
            }

            poll.wait_timeout(Timeout::from_millis(1))?;
            let keys = poll.events().map(|(k, _)| k).collect::<HashSet<_>>();

            assert!(keys.contains(&"reader0"));
            assert!(!keys.contains(&"reader1"));
            assert!(keys.contains(&"reader2"));

            poll.unregister(&"reader0");

            for w in &mut [&writer0, &writer1, &writer2] {
                w.write_all(&[0])?;
            }

            poll.wait_timeout(Timeout::from_millis(1))?;
            let keys = poll.events().map(|(k, _)| k).collect::<HashSet<_>>();

            assert!(!keys.contains(&"reader0"));
            assert!(!keys.contains(&"reader1"));
            assert!(keys.contains(&"reader2"));

            poll.unregister(&"reader2");

            for w in &mut [&writer0, &writer1, &writer2] {
                w.write_all(&[0])?;
            }

            poll.wait_timeout(Timeout::from_millis(1)).ok();

            assert!(!poll.has_events());
        }

        // Re-register.
        {
            poll.register("reader0", &reader0, event::READ);
            writer0.write_all(&[0])?;

            poll.wait_timeout(Timeout::from_millis(1))?;
            let (key, _) = poll.events().next().unwrap();

            assert_eq!(key, &"reader0");
        }

        Ok(())
    }

    #[test]
    fn test_set() -> io::Result<()> {
        let (mut writer0, reader0) = UnixStream::pair()?;
        let (mut writer1, reader1) = UnixStream::pair()?;

        let mut poll = Poll::new();

        for reader in &[&reader0, &reader1] {
            reader.set_nonblocking(true)?;
        }

        poll.register("reader0", &reader0, event::READ);
        poll.register("reader1", &reader1, event::NONE);

        writer0.write_all(&[0])?;

        poll.wait_timeout(Timeout::from_millis(1))?;
        let (key, _) = poll.events().next().unwrap();
        let key = (*key).to_owned();
        assert_eq!(key, "reader0");

        poll.unset(&key.as_str(), event::READ);
        writer0.write_all(&[0])?;

        poll.wait_timeout(Timeout::from_millis(1)).ok();
        assert!(poll.events().next().is_none());

        writer1.write_all(&[0])?;

        poll.wait_timeout(Timeout::from_millis(1)).ok();
        assert!(poll.events().next().is_none());

        poll.set(&"reader1", event::READ);
        writer1.write_all(&[0])?;

        poll.wait_timeout(Timeout::from_millis(1))?;
        let (key, _) = poll.events().next().unwrap();
        assert_eq!(key, &"reader1");

        Ok(())
    }

    #[test]
    fn test_waker() -> io::Result<()> {
        let mut poll = Poll::new();
        let mut waker = Waker::new(&mut poll, "waker")?;
        let buf = [0; 4096];

        poll.wait_timeout(Timeout::from_millis(1)).ok();
        assert!(poll.events().next().is_none());

        // Fill the waker stream until it would block..
        loop {
            match waker.writer.write(&buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    break;
                }
                Err(e) => return Err(e),
                _ => continue,
            }
        }

        poll.wait_timeout(Timeout::from_millis(1))?;
        let (key, event) = poll.events().next().unwrap();

        assert!(event.is_readable());
        assert!(!event.is_writable() && !event.has_hangup() && !event.has_errored());
        assert_eq!(key, &"waker");

        waker.wake()?;

        poll.wait_timeout(Timeout::from_millis(1))?;
        let (key, event) = poll.events().next().unwrap();

        assert!(event.is_readable());
        assert_eq!(key, &"waker");

        // Try to wake multiple times.
        waker.wake()?;
        waker.wake()?;
        waker.wake()?;

        poll.wait_timeout(Timeout::from_millis(1))?;
        assert_eq!(poll.events().count(), 1, "multiple wakes count as one");

        let (key, event) = poll.events().next().unwrap();
        assert_eq!(key, &"waker");

        Waker::reset(event).unwrap();

        // Try waiting multiple times.
        assert!(
            poll.wait_timeout(Timeout::from_millis(1)).unwrap(),
            "the waker should only wake once"
        );

        Ok(())
    }
}
