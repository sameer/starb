// FIXME: Currently RingBuffer requires its type parameter to be
// `Copy` because items are stored in a fixed-length array (which,
// itself, is required to allow creating RingBuffers
// statically). There may be work-arounds with ptr routines, and it
// should be investigated.
//
// TODO: Needs impls for Iter/IterMut.
#![no_std]
#![feature(const_fn)]

use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Underlying buffer capacity. Needs to be hard-coded for now,
/// because the size of the structure needs to be known at compile
/// time so it can be statically allocated or created on the stack.
///
/// This will disappear when const generics appear.
const CAPACITY: usize = 1024;

#[derive(Debug, PartialEq)]
pub enum Error {
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
    pub const fn new(default: T) -> Self {
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            buf: UnsafeCell::new([default; CAPACITY]),
        }
    }

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

pub struct Reader<'a, T> {
    rb: *const RingBuffer<T>,
    _marker: PhantomData<&'a ()>,
}
unsafe impl<T> Send for Reader<'_, T> where T: Send {}
unsafe impl<T> Sync for Reader<'_, T> {}

pub struct Writer<'a, T> {
    rb: *const RingBuffer<T>,
    _marker: PhantomData<&'a ()>,
}
unsafe impl<T> Send for Writer<'_, T> where T: Send {}
unsafe impl<T> Sync for Writer<'_, T> {}

impl<'a, T> Reader<'a, T>
where
    T: Copy,
{
    pub fn len(&self) -> usize {
        let rb: &mut RingBuffer<T> = unsafe { core::mem::transmute(self.rb) };
        let h = rb.head.load(Ordering::SeqCst);
        let t = rb.tail.load(Ordering::SeqCst);
        let rc = (t + CAPACITY - h) % CAPACITY;
        rc
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the value at the start of the buffer and advances the
    /// start of the buffer to the next element.
    ///
    /// If nothing is available in the buffer, returns `None`
    pub fn shift(&mut self) -> Option<T> {
        let rb: &mut RingBuffer<T> = unsafe { core::mem::transmute(self.rb) };
        let h = rb.head.load(Ordering::SeqCst);
        let t = rb.tail.load(Ordering::SeqCst);
        if h == t {
            None
        } else {
            let nh = (h + 1) % CAPACITY;
            let buf = unsafe { &mut *rb.buf.get() };
            let rc = Some(buf[h]);
            rb.head.store(nh, Ordering::SeqCst);
            rc
        }
    }
}

impl<'a, T> Writer<'a, T>
where
    T: Copy,
{
    /// Put `v` at the end of the buffer.
    ///
    /// Returns `BufferFull` if appending `v` would overlap with the
    /// start of the buffer.
    pub fn unshift(&mut self, v: T) -> Result<(), Error> {
        let rb: &mut RingBuffer<T> = unsafe { core::mem::transmute(self.rb) };
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
            let buf = unsafe { &mut *rb.buf.get() };
            buf[t] = v;
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
}
