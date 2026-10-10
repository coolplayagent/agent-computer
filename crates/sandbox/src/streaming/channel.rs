use super::*;
use crate::renewal::{ChallengeFrame, Grant, Window};
use std::{
    fs::File,
    io::{Read, Write},
    time::{Duration, Instant},
};

/// One bounded pending frame, shared by output and renewal control. A partial
/// write cannot interleave frames or block the supervisor's termination loop.
pub(crate) struct Channel {
    pub(crate) window: Window,
    transcript: Transcript,
    input: File,
    output: File,
    output_flags: rustix::fs::OFlags,
    input_buffer: Vec<u8>,
    pending: Vec<u8>,
    written: usize,
    last_output: [Instant; 2],
    next_stream: usize,
}
impl Channel {
    pub(crate) fn new(window: Window, cap: usize) -> Result<Self> {
        use rustix::{fs, io};
        let transcript = Transcript::new(window.progress().grant_digest.clone(), cap)?;
        let input = File::from(io::dup(std::io::stdin()).map_err(|_| Error::Setup)?);
        let output = File::from(io::dup(std::io::stdout()).map_err(|_| Error::Setup)?);
        let output_flags = fs::fcntl_getfl(&output).map_err(|_| Error::Setup)?;
        fs::fcntl_setfl(&input, fs::OFlags::NONBLOCK).map_err(|_| Error::Setup)?;
        fs::fcntl_setfl(&output, output_flags | fs::OFlags::NONBLOCK).map_err(|_| Error::Setup)?;
        Ok(Self {
            window,
            transcript,
            input,
            output,
            output_flags,
            input_buffer: Vec::new(),
            pending: Vec::new(),
            written: 0,
            last_output: [Instant::now(); 2],
            next_stream: 0,
        })
    }
    pub(crate) fn progress(&self) -> Result<Progress> {
        if !self.pending.is_empty() {
            return Err(Error::Setup);
        }
        Ok(self.transcript.progress().clone())
    }
    pub(crate) fn complete(&self, outputs: [&Output; 2]) -> bool {
        self.pending.is_empty()
            && !self.window.awaiting_grant()
            && outputs.into_iter().enumerate().all(|(i, output)| {
                let cursor = &self.transcript.cursors[i];
                cursor.eof
                    && cursor.retained_bytes == output.bytes.len() as u64
                    && cursor.observed_bytes == output.observed_bytes
            })
    }
    pub(crate) fn poll(&mut self, outputs: [&Output; 2], running: bool) -> Result<()> {
        if running {
            if Instant::now() >= self.window.deadline() {
                return Err(Error::LeaseExpired);
            }
            self.read_grant()?;
        }
        flush(&mut self.pending, &mut self.written, &mut self.output)?;
        if self.pending.is_empty() {
            let challenge = if running {
                self.window.challenge(Instant::now())?
            } else {
                None
            };
            if let Some(renewal) = challenge {
                self.pending =
                    serde_json::to_vec(&ChallengeFrame { renewal }).map_err(|_| Error::Setup)?;
                self.pending.push(b'\n');
            } else if let Some(chunk) = self.next_chunk(outputs, Instant::now())? {
                self.transcript.accept(&chunk)?;
                self.pending =
                    serde_json::to_vec(&Frame { output: chunk }).map_err(|_| Error::Setup)?;
                self.pending.push(b'\n');
            }
        }
        if self.pending.len() > MAX_FRAME_BYTES {
            return Err(Error::Setup);
        }
        flush(&mut self.pending, &mut self.written, &mut self.output)
    }
    fn read_grant(&mut self) -> Result<()> {
        let mut bytes = [0; crate::renewal::MAX_FRAME_BYTES];
        match self.input.read(&mut bytes) {
            Ok(0) => return Err(Error::Setup),
            Ok(n) => {
                if n > crate::renewal::MAX_FRAME_BYTES.saturating_sub(self.input_buffer.len()) {
                    return Err(Error::InvalidRequest);
                }
                self.input_buffer.extend_from_slice(&bytes[..n]);
                if let Some(end) = self.input_buffer.iter().position(|b| *b == b'\n') {
                    if end + 1 != self.input_buffer.len() {
                        return Err(Error::InvalidRequest);
                    }
                    let grant = Grant::parse(&self.input_buffer[..end])?;
                    self.window.accept(&grant, Instant::now())?;
                    self.input_buffer.clear();
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(Error::Setup),
        }
        Ok(())
    }
    fn next_chunk(&mut self, outputs: [&Output; 2], now: Instant) -> Result<Option<Chunk>> {
        for index in [self.next_stream, 1 - self.next_stream] {
            let output = outputs[index];
            let stream = [Stream::Stdout, Stream::Stderr][index];
            let cursor = self.transcript.cursor(stream);
            if cursor.eof {
                continue;
            }
            let offset = cursor.retained_bytes as usize;
            let rest = output.bytes.get(offset..).ok_or(Error::InvalidRequest)?;
            if (rest.is_empty() && !output.eof)
                || (!output.eof
                    && rest.len() < CHUNK_BYTES
                    && now.duration_since(self.last_output[index])
                        < Duration::from_millis(PARTIAL_FLUSH_MS))
            {
                continue;
            }
            let bytes = &rest[..rest.len().min(CHUNK_BYTES)];
            let chunk = Chunk {
                version: 1,
                startup_grant_digest: self.transcript.grant.clone(),
                sequence: self.transcript.progress.sequence + 1,
                previous_digest: self.transcript.progress.last_digest.clone(),
                stream,
                offset: cursor.retained_bytes,
                bytes: bytes.to_vec(),
                observed_bytes: output.observed_bytes,
                truncated: output.truncated,
                eof: output.eof && bytes.len() == rest.len(),
            };
            self.last_output[index] = now;
            self.next_stream = 1 - index;
            return Ok(Some(chunk));
        }
        Ok(None)
    }
}
fn flush(pending: &mut Vec<u8>, written: &mut usize, output: &mut impl Write) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let end = pending.len().min(*written + CHUNK_BYTES);
    match output.write(&pending[*written..end]) {
        Ok(0) => return Err(Error::Setup),
        Ok(n) => *written += n,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) => {}
        Err(_) => return Err(Error::Setup),
    }
    if *written == pending.len() {
        pending.clear();
        *written = 0;
    }
    Ok(())
}
impl Drop for Channel {
    fn drop(&mut self) {
        let _ = rustix::fs::fcntl_setfl(&self.output, self.output_flags);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn channel(cap: usize, now: Instant) -> Channel {
        let grant = format!("sha256:{}", "a".repeat(64));
        let output = File::open("/dev/null").unwrap();
        let flags = rustix::fs::fcntl_getfl(&output).unwrap();
        Channel {
            window: Window::new(grant.clone(), 30000, 30000, now).unwrap(),
            transcript: Transcript::new(grant, cap).unwrap(),
            input: File::open("/dev/null").unwrap(),
            output,
            output_flags: flags,
            input_buffer: vec![],
            pending: vec![],
            written: 0,
            last_output: [now; 2],
            next_stream: 0,
        }
    }
    #[test]
    fn eof_waits_for_an_outstanding_renewal_before_final_report() {
        let start = Instant::now();
        let mut c = channel(0, start);
        c.window = Window::new(c.transcript.grant.clone(), 30_000, 90_000, start).unwrap();
        let output = Output {
            eof: true,
            ..Output::default()
        };
        for _ in 0..2 {
            let chunk = c.next_chunk([&output; 2], start).unwrap().unwrap();
            c.transcript.accept(&chunk).unwrap();
        }
        assert!(c.complete([&output; 2]));
        let now = start + Duration::from_secs(10);
        let challenge = c.window.challenge(now).unwrap().unwrap();
        // Even after the challenge frame and both EOFs have been written, the
        // final report must wait for the controller's one outstanding grant.
        assert!(!c.complete([&output; 2]));
        assert_eq!(c.window.deadline(), start + Duration::from_secs(30));
        let grant = Grant {
            version: 1,
            challenge_digest: challenge.digest().unwrap(),
            lease_budget_ms: 30_000,
        };
        c.window
            .accept(&grant, now + Duration::from_millis(1))
            .unwrap();
        assert!(c.complete([&output; 2]));
        assert_eq!(c.window.progress().sequence, 1);
        assert_eq!(c.window.progress().grant_digest, grant.digest().unwrap());
    }
    #[test]
    fn partial_flush_is_timed_fair_and_does_not_claim_eof() {
        let start = Instant::now();
        let mut c = channel(10, start);
        let stdout = Output {
            bytes: b"out".to_vec(),
            observed_bytes: 3,
            truncated: false,
            eof: false,
        };
        let stderr = Output {
            bytes: b"err".to_vec(),
            observed_bytes: 3,
            truncated: false,
            eof: false,
        };
        assert!(
            c.next_chunk([&stdout, &stderr], start + Duration::from_millis(999))
                .unwrap()
                .is_none()
        );
        for stream in [Stream::Stdout, Stream::Stderr] {
            let chunk = c
                .next_chunk([&stdout, &stderr], start + Duration::from_millis(1000))
                .unwrap()
                .unwrap();
            assert_eq!(chunk.stream, stream);
            assert!(!chunk.eof);
            c.transcript.accept(&chunk).unwrap();
        }
        assert!(!c.complete([&stdout, &stderr]));
        assert!(
            c.next_chunk([&stdout, &stderr], start + Duration::from_secs(100))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn retained_limit_and_output_flood_emit_bounded_bytes_then_one_eof_per_stream() {
        for cap in [0, CHUNK_BYTES] {
            let start = Instant::now();
            let mut c = channel(cap, start);
            let mut stdout = Output {
                bytes: vec![255; cap],
                observed_bytes: 100_000,
                truncated: true,
                eof: false,
            };
            let mut stderr = Output {
                bytes: vec![0; cap],
                observed_bytes: 200_000,
                truncated: true,
                eof: false,
            };
            if cap > 0 {
                for stream in [Stream::Stdout, Stream::Stderr] {
                    let chunk = c.next_chunk([&stdout, &stderr], start).unwrap().unwrap();
                    assert_eq!(chunk.stream, stream);
                    assert_eq!(chunk.bytes.len(), cap);
                    assert!(!chunk.eof);
                    let bytes = serde_json::to_vec(&Frame {
                        output: chunk.clone(),
                    })
                    .unwrap();
                    assert!(bytes.len() < MAX_FRAME_BYTES);
                    c.transcript.accept(&chunk).unwrap();
                }
            }
            for seconds in 1..=100 {
                assert!(
                    c.next_chunk([&stdout, &stderr], start + Duration::from_secs(seconds))
                        .unwrap()
                        .is_none()
                );
            }
            stdout.observed_bytes += 50000;
            stdout.eof = true;
            stderr.eof = true;
            for stream in [Stream::Stdout, Stream::Stderr] {
                let chunk = c
                    .next_chunk([&stdout, &stderr], start + Duration::from_secs(101))
                    .unwrap()
                    .unwrap();
                assert_eq!(chunk.stream, stream);
                assert!(chunk.bytes.is_empty());
                assert!(chunk.eof);
                c.transcript.accept(&chunk).unwrap();
            }
            assert!(c.complete([&stdout, &stderr]));
            assert!(
                c.transcript
                    .verify_report(&c.progress().unwrap(), [&stdout, &stderr])
                    .unwrap()
            );
        }
    }
    struct Partial {
        bytes: Vec<u8>,
        blocked: bool,
    }
    impl Write for Partial {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.blocked {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            assert!(bytes.len() <= CHUNK_BYTES);
            let n = bytes.len().min(7);
            self.bytes.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            panic!("must not perform a blocking flush")
        }
    }
    #[test]
    fn partial_nonblocking_writes_preserve_one_frame_and_yield() {
        let original = vec![255; CHUNK_BYTES + 3];
        let mut pending = original.clone();
        let mut offset = 0;
        let mut output = Partial {
            bytes: vec![],
            blocked: false,
        };
        flush(&mut pending, &mut offset, &mut output).unwrap();
        assert_eq!(offset, 7);
        output.blocked = true;
        flush(&mut pending, &mut offset, &mut output).unwrap();
        assert_eq!(offset, 7);
        output.blocked = false;
        while !pending.is_empty() {
            flush(&mut pending, &mut offset, &mut output).unwrap();
        }
        assert_eq!(offset, 0);
        assert_eq!(output.bytes, original);
    }
}
