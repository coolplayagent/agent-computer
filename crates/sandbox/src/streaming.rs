//! Bounded observations on the authenticated PID 1 stream, never execution authority.
mod channel;
use crate::{Error, Output, Result, startup};
pub(crate) use channel::Channel;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CHUNK_BYTES: usize = 8192;
pub const MAX_FRAME_BYTES: usize = 40 * 1024;
pub const MAX_CHUNKS: u32 = 8192;
pub const PARTIAL_FLUSH_MS: u64 = 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}
impl Stream {
    pub fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    pub version: u32,
    pub startup_grant_digest: String,
    pub sequence: u32,
    pub previous_digest: String,
    pub stream: Stream,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub observed_bytes: u64,
    pub truncated: bool,
    pub eof: bool,
}
impl Chunk {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || !startup::valid_digest(&self.startup_grant_digest)
            || !startup::valid_digest(&self.previous_digest)
            || !(1..=MAX_CHUNKS).contains(&self.sequence)
            || self.bytes.len() > CHUNK_BYTES
            || (self.bytes.is_empty() && !self.eof)
            || self
                .offset
                .checked_add(self.bytes.len() as u64)
                .is_none_or(|n| n > self.observed_bytes)
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        startup::digest("agent-computer/execution-output-chunk-v1", self)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub output: Chunk,
}
impl Frame {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(Error::InvalidRequest);
        }
        let frame: Self = serde_json::from_slice(bytes).map_err(|_| Error::InvalidRequest)?;
        frame.output.validate()?;
        Ok(frame)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub sequence: u32,
    pub last_digest: String,
}
impl Progress {
    pub fn validate(&self) -> Result<()> {
        if self.sequence > MAX_CHUNKS || !startup::valid_digest(&self.last_digest) {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub retained_bytes: u64,
    pub observed_bytes: u64,
    pub truncated: bool,
    pub eof: bool,
}

/// Process-local validation; a serialized cursor does not reconstruct this state.
pub struct Transcript {
    grant: String,
    cap: usize,
    progress: Progress,
    cursors: [Cursor; 2],
    hashes: [Sha256; 2],
}
impl Transcript {
    pub fn new(grant: String, cap: usize) -> Result<Self> {
        if !startup::valid_digest(&grant) || cap > crate::MAX_OUTPUT_BYTES {
            return Err(Error::InvalidRequest);
        }
        Ok(Self {
            progress: Progress {
                sequence: 0,
                last_digest: grant.clone(),
            },
            grant,
            cap,
            cursors: Default::default(),
            hashes: Default::default(),
        })
    }
    pub fn progress(&self) -> &Progress {
        &self.progress
    }
    pub fn cursor(&self, stream: Stream) -> &Cursor {
        &self.cursors[stream.index()]
    }
    pub fn accept(&mut self, chunk: &Chunk) -> Result<()> {
        chunk.validate()?;
        let old = self.cursor(chunk.stream);
        let end = chunk.offset + chunk.bytes.len() as u64;
        if chunk.startup_grant_digest != self.grant
            || chunk.sequence != self.progress.sequence + 1
            || chunk.previous_digest != self.progress.last_digest
            || old.eof
            || chunk.offset != old.retained_bytes
            || end > self.cap as u64
            || chunk.observed_bytes < old.observed_bytes
            || chunk.truncated != (chunk.observed_bytes > self.cap as u64)
            || (old.truncated && !chunk.truncated)
            || (chunk.eof && end != chunk.observed_bytes.min(self.cap as u64))
        {
            return Err(Error::InvalidRequest);
        }
        let digest = chunk.digest()?;
        self.hashes[chunk.stream.index()].update(&chunk.bytes);
        self.cursors[chunk.stream.index()] = Cursor {
            retained_bytes: end,
            observed_bytes: chunk.observed_bytes,
            truncated: chunk.truncated,
            eof: chunk.eof,
        };
        self.progress = Progress {
            sequence: chunk.sequence,
            last_digest: digest,
        };
        Ok(())
    }
    /// A final report may retain more bytes after an interrupted stream. It must
    /// match every delivered byte and must not undo an observed EOF/truncation.
    pub fn verify_report(&self, progress: &Progress, outputs: [&Output; 2]) -> Result<bool> {
        progress.validate()?;
        if progress != &self.progress {
            return Err(Error::InvalidRequest);
        }
        let mut complete = true;
        for (i, output) in outputs.into_iter().enumerate() {
            let cursor = &self.cursors[i];
            let prefix = output
                .bytes
                .get(..cursor.retained_bytes as usize)
                .ok_or(Error::InvalidRequest)?;
            if output.bytes.len() > self.cap
                || output.observed_bytes < output.bytes.len() as u64
                || output.truncated != (output.observed_bytes > output.bytes.len() as u64)
                || cursor.observed_bytes > output.observed_bytes
                || (cursor.truncated && !output.truncated)
                || (cursor.eof
                    && (!output.eof
                        || cursor.retained_bytes != output.bytes.len() as u64
                        || cursor.observed_bytes != output.observed_bytes))
                || Sha256::digest(prefix) != self.hashes[i].clone().finalize()
            {
                return Err(Error::InvalidRequest);
            }
            complete &= cursor.eof && cursor.retained_bytes == output.bytes.len() as u64;
        }
        Ok(complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn transcript(cap: usize) -> Transcript {
        Transcript::new(format!("sha256:{}", "a".repeat(64)), cap).unwrap()
    }
    fn chunk(t: &Transcript, stream: Stream, bytes: &[u8], eof: bool) -> Chunk {
        Chunk {
            version: 1,
            startup_grant_digest: t.grant.clone(),
            sequence: t.progress.sequence + 1,
            previous_digest: t.progress.last_digest.clone(),
            stream,
            offset: t.cursor(stream).retained_bytes,
            bytes: bytes.to_vec(),
            observed_bytes: t.cursor(stream).retained_bytes + bytes.len() as u64,
            truncated: false,
            eof,
        }
    }
    #[test]
    fn wrong_identity_sequence_offset_and_digest_never_advance_a_transcript() {
        let mut t = transcript(10);
        let c = chunk(&t, Stream::Stdout, &[0, 255], false);
        let initial = t.progress.clone();
        for field in [
            "startup_grant_digest",
            "sequence",
            "previous_digest",
            "offset",
            "observed_bytes",
            "truncated",
        ] {
            let mut value = serde_json::to_value(&c).unwrap();
            value[field] = match field {
                "startup_grant_digest" | "previous_digest" => {
                    format!("sha256:{}", "b".repeat(64)).into()
                }
                "truncated" => true.into(),
                "observed_bytes" => 0.into(),
                _ => 3.into(),
            };
            assert!(
                t.accept(&serde_json::from_value(value).unwrap()).is_err(),
                "{field}"
            );
            assert_eq!(t.progress, initial);
        }
        t.accept(&c).unwrap();
        assert!(t.accept(&c).is_err());
        assert_eq!(t.cursor(Stream::Stdout).retained_bytes, 2);
    }
    #[test]
    fn independent_streams_match_binary_prefixes_and_final_eof() {
        let mut t = transcript(10);
        let out = Output {
            bytes: vec![0, 255, 10],
            observed_bytes: 3,
            truncated: false,
            eof: true,
        };
        let err = Output {
            bytes: b"error".to_vec(),
            observed_bytes: 5,
            truncated: false,
            eof: true,
        };
        let c = chunk(&t, Stream::Stdout, &out.bytes[..2], false);
        t.accept(&c).unwrap();
        assert!(!t.verify_report(t.progress(), [&out, &err]).unwrap());
        let c = chunk(&t, Stream::Stderr, &err.bytes, true);
        t.accept(&c).unwrap();
        let c = chunk(&t, Stream::Stdout, &out.bytes[2..], true);
        t.accept(&c).unwrap();
        assert!(t.verify_report(t.progress(), [&out, &err]).unwrap());
        let mut wrong = out;
        wrong.bytes[0] = 1;
        assert!(t.verify_report(t.progress(), [&wrong, &err]).is_err());
        assert!(t.accept(&chunk(&t, Stream::Stderr, &[], true)).is_err());
    }
    #[test]
    fn truncation_zero_retention_and_frame_limits_are_explicit() {
        let mut t = transcript(0);
        let mut c = chunk(&t, Stream::Stdout, &[], true);
        c.observed_bytes = 123;
        c.truncated = true;
        t.accept(&c).unwrap();
        let c = chunk(&t, Stream::Stderr, &[], true);
        t.accept(&c).unwrap();
        assert!(
            t.verify_report(
                t.progress(),
                [
                    &Output {
                        bytes: vec![],
                        observed_bytes: 123,
                        truncated: true,
                        eof: true
                    },
                    &Output {
                        eof: true,
                        ..Output::default()
                    }
                ]
            )
            .unwrap()
        );
        let t = transcript(CHUNK_BYTES);
        let c = chunk(&t, Stream::Stdout, &vec![255; CHUNK_BYTES], true);
        let bytes = serde_json::to_vec(&Frame { output: c }).unwrap();
        assert!(bytes.len() < MAX_FRAME_BYTES);
        Frame::parse(&bytes).unwrap();
        assert!(Frame::parse(&vec![b' '; MAX_FRAME_BYTES + 1]).is_err());
        assert!(Frame::parse(br#"{"output":{},"unknown":true}"#).is_err());
    }
}
