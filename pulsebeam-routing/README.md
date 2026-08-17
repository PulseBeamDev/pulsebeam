# pulsebeam-routing

The single source of truth for packet classification: STUN/ufrag parsing,
the fixed inter-node Envelope, and the shard-steering decision built on top
of both. This crate is compiled into three places that must never disagree:

- the Aya eBPF program (`bpfel-unknown-none`, no OS, no allocator);
- the Linux userspace demuxer, as a validation/cache layer;
- the deterministic simulator's steering adapter.

If a byte is parsed differently by any of the three, a packet gets routed to
the wrong shard on real traffic while every non-eBPF test stays green. That
failure mode is exactly why this logic lives in one no-std crate instead of
three copies.

## Rules for anyone touching this crate

This crate must keep compiling for the eBPF verifier, not just for `rustc`.
The verifier proves every possible execution path terminates and never reads
or writes out of bounds — it cannot do that if the code doesn't make the
bounds provable locally.

- **No heap.** No `Vec`, `String`, `Box`, `format!`, or anything that
  allocates. `#![no_std]` catches most of this at compile time; the rest is
  discipline.
- **No unbounded loops.** Every loop must have a bound the verifier can see:
  either a compile-time constant, or a value derived from the packet itself
  that is provably capped (e.g. a STUN message length is a `u16`, so an
  attribute-parsing loop bounded by it is bounded by 65535 regardless of
  input).
- **Every read is bounds-checked.** Use `slice::get`/`get(..)` ranges, never
  bare indexing on a slice built from packet bytes. `checked_add`/
  `saturating_add` for any offset arithmetic derived from packet fields —
  a bare `+` on attacker-controlled lengths is exactly the bug this crate
  exists to prevent.
- **No iterator adaptors that allocate** (`collect`, `Vec::from_iter`, ...).
  Plain iteration or a manual bounded `while` is fine; it compiles to the
  same bounded loop either way.
- **No panics on malformed input.** A packet that doesn't parse returns
  `None`/`Err`/a `Drop(DropReason)` variant. `unwrap`/`expect`/`panic!` are
  workspace-denied outside tests for this exact reason: this parser sees the
  network before anything else does.
- **Indexing into a small fixed-size lookup table by a masked value** (e.g.
  a 5-bit Crockford symbol into a 32-entry alphabet) is the one place a
  bounds-check would be pure overhead — the mask already proves the range.
  Take the lint `#[allow]` there, with a one-line reason, rather than
  threading `Option` through arithmetic that cannot fail.

If you find yourself reaching for a `Vec` "just to make the loop easier,"
that is the crate telling you the eBPF program can't take the same code path
— find the fixed-size or slice-based equivalent instead.
