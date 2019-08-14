# STAtic Ring Buffers

[![Documentation](https://docs.rs/starb/badge.svg)](https://docs.rs/starb)
[![Testing](https://api.travis-ci.org/repos/bjc/starb.svg?branch=master)](https://travis-ci.org/bjc/starb)

This is a simple ring-buffer structure that lives on the stack, rather
than the heap, so that it can be used in `no-std` environments, such
as embedded.
