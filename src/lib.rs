// FIXME: Currently RingBuffer requires its type parameter to be
// `Copy` because items are stored in a fixed-length array (which,
// itself, is required to allow creating RingBuffers
// statically). There may be work-arounds with ptr routines, and it
// should be investigated.
#![no_std]
#![feature(const_fn)]

use core::{
    cell::UnsafeCell,
    cmp,
    marker::PhantomData,
    mem,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Underlying buffer capacity. Needs to be hard-coded for now,
/// because the size of the structure needs to be known at compile
/// time so it can be statically allocated or created on the stack.
///
/// This will disappear when const generics appear.
const CAPACITY: usize = 1024;

/// Errors that can be made when interacting with the ring buffer.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// The buffer is at capacity and cannot fit any more
    /// elements. You need to `unshift` at least once to make some
    /// space.
    BufferFull,
}

/// A lock-free concurrent ring buffer that prevents overwriting data
/// before it is read.
pub struct RingBuffer<T> {
    head: AtomicUsize,
    tail: AtomicUsize,

    // Backing store needs to be UnsafeCell to make sure it's not
    // stored in read-only data sections.
    buf: UnsafeCell<[T; CAPACITY]>,
}

impl<T> RingBuffer<T>
where
    T: Copy,
{
    /// Create a new RingBuffer.
    ///
    /// The `default` element isn't important, but is, unfortunately,
    /// currently required to make stable rust happy.
    pub const fn new(default: T) -> Self {
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            buf: UnsafeCell::new([default; CAPACITY]),
        }
    }

    /// Splits the ring buffer into a consumer/producer pair
    /// (`Reader`/`Writer` respectively). These structures can be used
    /// safely across threads.
    // Honestly, this should consume `self` or at least be `mut`, but
    // it needs to be available in const static contexts, which
    // prevents that. Basically all the rest of the unsafe stuff in
    // here is a consequence of that.
    //
    // No, lazy_static is not an option, because it doesn't work on
    // architectures where CAS atomics are missing.
    pub const fn split<'a>(&'a self) -> (Reader<T>, Writer<T>) {
        let rbr = Reader {
            rb: self as *const _,
            _marker: PhantomData,
        };
        let rbw = Writer {
            rb: self as *const _,
            _marker: PhantomData,
        };
        (rbr, rbw)
    }
}

/// Consumer of `RingBuffer`.
pub struct Reader<'a, T> {
    rb: *const RingBuffer<T>,
    _marker: PhantomData<&'a ()>,
}
unsafe impl<T> Send for Reader<'_, T> where T: Send {}
unsafe impl<T> Sync for Reader<'_, T> {}

/// Producer for `Ringbuffer`.
pub struct Writer<'a, T> {
    rb: *const RingBuffer<T>,
    _marker: PhantomData<&'a ()>,
}
unsafe impl<T> Send for Writer<'_, T> where T: Send {}
unsafe impl<T> Sync for Writer<'_, T> {}

impl<T> Reader<'_, T>
where
    T: Copy,
{
    /// The number of elements currently available for reading.
    ///
    /// NB: Because the `Writer` half of the ring buffer may be adding
    /// elements in another thread, the value read from `len` is a
    /// *minimum* of what may actually be available by the time the
    /// reading takes place.
    pub fn len(&self) -> usize {
        let rb: &mut RingBuffer<T> = unsafe { mem::transmute(self.rb) };
        let h = rb.head.load(Ordering::SeqCst);
        let t = rb.tail.load(Ordering::SeqCst);
        let rc = (t + CAPACITY - h) % CAPACITY;
        rc
    }

    /// Whether or not the ring buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the value at the start of the buffer and advances the
    /// start of the buffer to the next element.
    ///
    /// If nothing is available in the buffer, returns `None`
    pub fn shift(&mut self) -> Option<T> {
        let rb: &mut RingBuffer<T> = unsafe { mem::transmute(self.rb) };
        let h = rb.head.load(Ordering::SeqCst);
        let t = rb.tail.load(Ordering::SeqCst);
        if h == t {
            None
        } else {
            let nh = (h + 1) % CAPACITY;
            let rc = unsafe {
                let buf = &mut *rb.buf.get();
                Some(*buf.get_unchecked(h))
            };
            rb.head.store(nh, Ordering::SeqCst);
            rc
        }
    }

    /// Shift all available data into `buf` up to the size of `buf`.
    ///
    /// Returns the number of items written into `buf`.
    pub fn shift_into(&mut self, buf: &mut [T]) -> usize {
        let rb: &mut RingBuffer<T> = unsafe { mem::transmute(self.rb) };

        let mut h = rb.head.load(Ordering::SeqCst);
        let t = rb.tail.load(Ordering::SeqCst);

        let mylen = (t + CAPACITY - h) % CAPACITY;
        let buflen = buf.len();
        let len = cmp::min(mylen, buflen);

        unsafe {
            let rbuf = &mut *rb.buf.get();
            for i in 0..len {
                *buf.get_unchecked_mut(i) = *rbuf.get_unchecked(h);
                h = (h + 1) % CAPACITY;
            }
        }

        rb.head.store(h, Ordering::SeqCst);
        len
    }
}

impl<T> Iterator for Reader<'_, T>
where
    T: Copy,
{
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        self.shift()
    }
}

impl<T> Writer<'_, T>
where
    T: Copy,
{
    /// Put `v` at the end of the buffer.
    ///
    /// Returns `BufferFull` if appending `v` would overlap with the
    /// start of the buffer.
    pub fn unshift(&mut self, v: T) -> Result<(), Error> {
        let rb: &mut RingBuffer<T> = unsafe { mem::transmute(self.rb) };
        let h = rb.head.load(Ordering::SeqCst);
        let t = rb.tail.load(Ordering::SeqCst);
        let nt = (t + 1) % CAPACITY;
        // We can't allow overwrites of the head position, because it
        // would then be possible to write to the same memory location
        // that is being read. If reading a value of `T` takes more
        // than one memory read, then reading from the head would
        // produce garbage in this scenario.
        if nt == h {
            // FIXME: this comparison is wrong in the trivial
            // 1-element buffer case which would never allow an
            // `unshift`. In larger buffers it wastes a buffer slot.
            Err(Error::BufferFull)
        } else {
            unsafe {
                let buf = &mut *rb.buf.get();
                *buf.get_unchecked_mut(t) = v;
            }
            rb.tail.store(nt, Ordering::SeqCst);
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn detects_empty() {
        let rb = RingBuffer::<bool>::new(false);
        let (mut rbr, mut rbw) = rb.split();
        assert!(rbr.is_empty());
        rbw.unshift(true).ok();
        assert!(!rbr.is_empty());
        rbr.shift();
        assert!(rbr.is_empty());
    }

    #[test]
    fn len_matches() {
        let rb = RingBuffer::<bool>::new(false);
        let (mut rbr, mut rbw) = rb.split();

        // Count length up.
        for i in 0..CAPACITY - 1 {
            assert_eq!(rbr.len(), i);
            assert_eq!(rbw.unshift(true), Ok(()));
        }

        // ...and back down again.
        for i in 0..CAPACITY - 1 {
            assert_eq!(rbr.len(), CAPACITY - 1 - i);
            rbr.shift();
        }

        // Final check for empty.
        assert_eq!(rbr.len(), 0);
    }

    #[test]
    fn can_wrap() {
        let rb = RingBuffer::<usize>::new(0);
        let (mut rbr, mut rbw) = rb.split();

        // Make sure we can store n-1 elements.
        for i in 0..CAPACITY - 1 {
            assert_eq!(rbw.unshift(i), Ok(()))
        }

        // ...and that we can load them back again.
        for i in 0..CAPACITY - 1 {
            assert_eq!(rbr.shift(), Some(i))
        }
    }

    #[test]
    fn cannot_overwrite() {
        let rb = RingBuffer::<usize>::new(0);
        let (mut rbr, mut rbw) = rb.split();

        for i in 0..CAPACITY - 1 {
            assert_eq!(rbw.unshift(i), Ok(()));
        }
        assert_eq!(rbw.unshift(0xffff), Err(Error::BufferFull));

        // We can drop an element to allow a slot to write to again.
        rbr.shift();
        assert_eq!(rbw.unshift(0xffff), Ok(()));
    }

    #[test]
    fn can_iter() {
        let rb = RingBuffer::<usize>::new(0);
        let (rbr, mut rbw) = rb.split();

        for i in 0..CAPACITY - 1 {
            assert_eq!(rbw.unshift(i), Ok(()));
        }

        let mut i = 0;
        for e in rbr {
            assert_eq!(e, i);
            i += 1;
        }
    }

    #[test]
    fn shift_into_smaller() {
        let rb = RingBuffer::<usize>::new(0);
        let (mut rbr, mut rbw) = rb.split();
        for i in 0..CAPACITY - 1 {
            assert_eq!(rbw.unshift(i), Ok(()));
        }

        let mut buf: [usize; CAPACITY / 2] = [0; CAPACITY / 2];
        assert_eq!(rbr.shift_into(&mut buf), CAPACITY / 2, "return len wrong");
        for i in 0..CAPACITY / 2 {
            assert_eq!(buf[i], i, "slot {} wrong", i)
        }

        assert!(!rbr.shift().is_none());
    }

    #[test]
    fn shift_into_bigger() {
        let rb = RingBuffer::<usize>::new(0);
        let (mut rbr, mut rbw) = rb.split();
        for i in 0..CAPACITY - 1 {
            assert_eq!(rbw.unshift(i), Ok(()));
        }

        let mut buf: [usize; CAPACITY * 2] = [0; CAPACITY * 2];
        assert_eq!(rbr.shift_into(&mut buf), CAPACITY - 1, "return len wrong");
        for i in 0..CAPACITY - 1 {
            assert_eq!(buf[i], i, "first half")
        }
        for i in CAPACITY - 1..CAPACITY * 2 {
            assert_eq!(buf[i], 0, "second half")
        }

        assert!(rbr.shift().is_none());
    }
}
