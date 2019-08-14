//! An implementation of STAtically allocated Ring Buffers.
//!
//! This is a simple ring-buffer structure that lives on the stack,
//! rather than the heap, so that it can be used in `no-std`
//! environments, such as embedded.
#![no_std]

use core::{
    cell::UnsafeCell,
    cmp,
    marker::PhantomData,
    mem::{self, MaybeUninit},
    ptr,
    sync::atomic::{self, AtomicUsize, Ordering},
};

/// Underlying buffer capacity. Needs to be hard-coded for now,
/// because the size of the structure needs to be known at compile
/// time so it can be statically allocated or created on the stack.
///
/// This will disappear when const generics appear.
pub const CAPACITY: usize = 1024;

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
    buf: UnsafeCell<MaybeUninit<[T; CAPACITY]>>,
}

impl<T> RingBuffer<T> {
    /// Create a new RingBuffer.
    pub const fn new() -> Self {
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            buf: UnsafeCell::new(MaybeUninit::uninit()),
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

impl<T> Reader<'_, T> {
    /// The number of elements currently available for reading.
    ///
    /// NB: Because the `Writer` half of the ring buffer may be adding
    /// elements in another thread, the value read from `len` is a
    /// *minimum* of what may actually be available by the time the
    /// reading takes place.
    pub fn len(&self) -> usize {
        let rb: &mut RingBuffer<T> = unsafe { mem::transmute(self.rb) };
        let h = rb.head.load(Ordering::Relaxed);
        let t = rb.tail.load(Ordering::Relaxed);
        atomic::fence(Ordering::Acquire);

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

        let h = rb.head.load(Ordering::Relaxed);
        let t = rb.tail.load(Ordering::Relaxed);

        if h == t {
            None
        } else {
            atomic::fence(Ordering::Acquire);
            let nh = (h + 1) % CAPACITY;
            let rc = unsafe {
                let buf: &MaybeUninit<[T; CAPACITY]> = &*rb.buf.get();
                Some(Self::load_val_at(h, buf))
            };
            atomic::fence(Ordering::Release);
            rb.head.store(nh, Ordering::Relaxed);
            rc
        }
    }

    /// Shift all available data into `buf` up to the size of `buf`.
    ///
    /// Returns the number of items written into `buf`.
    pub fn shift_into(&mut self, buf: &mut [T]) -> usize {
        let rb: &mut RingBuffer<T> = unsafe { mem::transmute(self.rb) };

        let mut h = rb.head.load(Ordering::Relaxed);
        let t = rb.tail.load(Ordering::Relaxed);
        atomic::fence(Ordering::Acquire);

        let mylen = (t + CAPACITY - h) % CAPACITY;
        let buflen = buf.len();
        let len = cmp::min(mylen, buflen);

        unsafe {
            let rbuf: &MaybeUninit<[T; CAPACITY]> = &*rb.buf.get();
            for i in 0..len {
                *buf.get_unchecked_mut(i) = Self::load_val_at(h, rbuf);
                h = (h + 1) % CAPACITY;
            }
        }

        atomic::fence(Ordering::Release);
        rb.head.store(h, Ordering::Relaxed);
        len
    }

    #[inline(always)]
    unsafe fn load_val_at(i: usize, buf: &MaybeUninit<[T; CAPACITY]>) -> T {
        let b: &[T; CAPACITY] = &*buf.as_ptr();
        ptr::read(b.get_unchecked(i))
    }
}

impl<T> Iterator for Reader<'_, T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        self.shift()
    }
}

impl<T> Writer<'_, T> {
    /// Put `v` at the end of the buffer.
    ///
    /// Returns `BufferFull` if appending `v` would overlap with the
    /// start of the buffer.
    pub fn unshift(&mut self, v: T) -> Result<(), Error> {
        let rb: &mut RingBuffer<T> = unsafe { mem::transmute(self.rb) };

        let h = rb.head.load(Ordering::Relaxed);
        let t = rb.tail.load(Ordering::Relaxed);

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
            atomic::fence(Ordering::Acquire);
            unsafe {
                let buf = &mut *rb.buf.get();
                Self::store_val_at(t, buf, v);
            }
            atomic::fence(Ordering::Release);
            rb.tail.store(nt, Ordering::Relaxed);
            Ok(())
        }
    }

    #[inline(always)]
    unsafe fn store_val_at(i: usize, buf: &mut MaybeUninit<[T; CAPACITY]>, val: T) {
        let b: &mut [T; CAPACITY] = &mut *buf.as_mut_ptr();
        ptr::write(b.get_unchecked_mut(i), val);
    }
}

// TODO: this needs to be `Copy` because we're pulling data from a
// slice, and we can't just take stuff out of an index without
// replacing it, and there's no good value for that.
impl<T> Writer<'_, T>
where
    T: Copy,
{
    /// Copy as much of `buf` into the ring buffer as possible.
    ///
    /// Returns the number of items copied.
    pub fn unshift_from(&mut self, buf: &[T]) -> usize {
        let rb: &mut RingBuffer<T> = unsafe { mem::transmute(self.rb) };

        let h = rb.head.load(Ordering::Relaxed);
        let mut t = rb.tail.load(Ordering::Relaxed);
        atomic::fence(Ordering::Acquire);

        let mylen = (t + CAPACITY - h) % CAPACITY;
        let buflen = buf.len();
        let len = cmp::min(CAPACITY - mylen - 1, buflen);

        unsafe {
            let rbuf = &mut *rb.buf.get();
            for i in 0..len {
                Self::store_val_at(t, rbuf, *buf.get_unchecked(i));
                t = (t + 1) % CAPACITY;
            }
        }

        atomic::fence(Ordering::Release);
        rb.tail.store(t, Ordering::Relaxed);
        len
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn detects_empty() {
        let rb = RingBuffer::<bool>::new();
        let (mut rbr, mut rbw) = rb.split();
        assert!(rbr.is_empty());
        rbw.unshift(true).ok();
        assert!(!rbr.is_empty());
        rbr.shift();
        assert!(rbr.is_empty());
    }

    #[test]
    fn len_matches() {
        let rb = RingBuffer::<bool>::new();
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
        let rb = RingBuffer::<usize>::new();
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
        let rb = RingBuffer::<usize>::new();
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
        let rb = RingBuffer::<usize>::new();
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
        let rb = RingBuffer::<usize>::new();
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
        let rb = RingBuffer::<usize>::new();
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

    #[test]
    fn unshift_from_smaller() {
        let rb = RingBuffer::<usize>::new();
        let (mut rbr, mut rbw) = rb.split();

        let buf: [usize; CAPACITY / 2] = [0xdead; CAPACITY / 2];
        assert_eq!(rbw.unshift_from(&buf), CAPACITY / 2);
        for i in 0..CAPACITY / 2 {
            assert_eq!(rbr.shift(), Some(0xdead), "wrong value at index {}", i);
        }
        assert!(rbr.shift().is_none());
    }

    #[test]
    fn unshift_from_bigger() {
        let rb = RingBuffer::<usize>::new();
        let (mut rbr, mut rbw) = rb.split();

        let buf: [usize; CAPACITY * 2] = [0xdead; CAPACITY * 2];
        assert_eq!(rbw.unshift_from(&buf), CAPACITY - 1);
        assert_eq!(rbw.unshift(0xbeef), Err(Error::BufferFull));
        for i in 0..CAPACITY - 1 {
            assert_eq!(rbr.shift(), Some(0xdead), "wrong value at index {}", i);
        }
        assert!(rbr.shift().is_none());
    }

    #[test]
    fn ownership_passes_through() {
        static mut DROPPED: bool = false;
        struct DropTest {};
        impl DropTest {
            fn i_own_it_now(self) {}
        }
        impl Drop for DropTest {
            fn drop(&mut self) {
                unsafe { DROPPED = true };
            }
        }

        let rb = RingBuffer::<DropTest>::new();
        let (mut rbr, mut rbw) = rb.split();

        // Create a closure to take ownership of a `DropTest` so we
        // can make sure it's not dropped when the closure is over.
        let mut cl = |dt| {
            rbw.unshift(dt).expect("couldn't store item");
        };
        cl(DropTest {});
        assert_eq!(unsafe { DROPPED }, false);

        // Still, nothing should be dropped, since we now own the
        // value.
        let dt = rbr.shift().expect("buffer was empty");
        assert_eq!(unsafe { DROPPED }, false);

        // And, finally, by giving ownership away, it'll get dropped.
        dt.i_own_it_now();
        assert_eq!(unsafe { DROPPED }, true);
    }
}
