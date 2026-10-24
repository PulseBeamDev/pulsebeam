//! Test-only reversible transform. This is not a secure encryption algorithm.
use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::thread::{self, ThreadId};

#[derive(Debug)]
pub struct Call {
    pub encrypt: bool,
    pub media: MediaType,
    pub thread: ThreadId,
    pub ssrc: u32,
    pub csrcs: Vec<u32>,
    pub additional: Vec<u8>,
    pub input: Vec<u8>,
}
#[derive(Default)]
pub struct Trace {
    pub calls: Mutex<Vec<Call>>,
    pub encoder_drops: AtomicUsize,
    pub decoder_drops: AtomicUsize,
}
pub struct Cipher {
    trace: Arc<Trace>,
    encrypt: bool,
}
impl Cipher {
    pub fn encoder(trace: Arc<Trace>) -> Self {
        Self {
            trace,
            encrypt: true,
        }
    }
    pub fn decoder(trace: Arc<Trace>) -> Self {
        Self {
            trace,
            encrypt: false,
        }
    }
    fn record(&self, media: MediaType, ssrc: u32, csrcs: &[u32], additional: &[u8], input: &[u8]) {
        self.trace.calls.lock().unwrap().push(Call {
            encrypt: self.encrypt,
            media,
            thread: thread::current().id(),
            ssrc,
            csrcs: csrcs.to_vec(),
            additional: additional.to_vec(),
            input: input.to_vec(),
        });
    }
}
impl Drop for Cipher {
    fn drop(&mut self) {
        if self.encrypt {
            self.trace.encoder_drops.fetch_add(1, Ordering::SeqCst);
        } else {
            self.trace.decoder_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
}
impl FrameEncryptorProvider for Cipher {
    fn max_ciphertext_byte_size(&self, _: MediaType, size: usize) -> usize {
        size.checked_add(1).unwrap()
    }
    fn encrypt(
        &self,
        media: MediaType,
        ssrc: u32,
        additional: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> FrameEncryptionResult {
        self.record(media, ssrc, &[], additional, input);
        assert!(
            output.iter().all(|byte| *byte == 0),
            "SDK scratch must be initialized"
        );
        if output.len() < input.len() + 1 {
            return FrameEncryptionResult {
                status: -57,
                bytes_written: 0,
            };
        }
        output[0] = 0xa5;
        for (out, byte) in output[1..].iter_mut().zip(input) {
            *out = *byte ^ 0x5a;
        }
        FrameEncryptionResult {
            status: 0,
            bytes_written: input.len() + 1,
        }
    }
}
impl FrameDecryptorProvider for Cipher {
    fn max_plaintext_byte_size(&self, _: MediaType, size: usize) -> usize {
        size.saturating_sub(1)
    }
    fn decrypt(
        &self,
        media: MediaType,
        csrcs: &[u32],
        additional: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> FrameDecryptionResult {
        self.record(media, 0, csrcs, additional, input);
        assert!(
            output.iter().all(|byte| *byte == 0),
            "SDK scratch must be initialized"
        );
        if input.first() != Some(&0xa5) || output.len() < input.len() - 1 {
            return FrameDecryptionResult::new(FrameDecryptionStatus::FailedToDecrypt, 0);
        }
        for (out, byte) in output.iter_mut().zip(&input[1..]) {
            *out = *byte ^ 0x5a;
        }
        FrameDecryptionResult::new(FrameDecryptionStatus::Ok, input.len() - 1)
    }
}
