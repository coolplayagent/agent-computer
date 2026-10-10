use serde::{Deserialize, Serialize};
use std::io::{self, Read};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub bytes: Vec<u8>,
    pub observed_bytes: u64,
    pub truncated: bool,
    pub eof: bool,
}

impl Output {
    /// A finite per-tick budget prevents a flooding child from starving the watchdog.
    pub(crate) fn drain(&mut self, reader: &mut impl Read, limit: usize) -> io::Result<()> {
        let mut buffer = [0; 8192];
        for _ in 0..8 {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => {
                    self.observed_bytes = self.observed_bytes.saturating_add(n as u64);
                    let keep = n.min(limit.saturating_sub(self.bytes.len()));
                    self.bytes.extend_from_slice(&buffer[..keep]);
                    self.truncated |= keep != n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_flood_keeps_prefix_and_yields_after_finite_work() {
        let input = vec![0xff; 200_000];
        let mut reader = input.as_slice();
        let mut output = Output::default();
        output.drain(&mut reader, 7).unwrap();
        assert_eq!(output.bytes, vec![0xff; 7]);
        assert_eq!(output.observed_bytes, 65536);
        assert!(output.truncated && !output.eof);
        while !output.eof {
            output.drain(&mut reader, 7).unwrap();
        }
        assert_eq!(output.observed_bytes, 200_000);
        assert_eq!(output.bytes.len(), 7);
    }
    #[test]
    fn zero_retention_still_drains_and_counts_binary_bytes() {
        let mut output = Output::default();
        output.drain(&mut &[0, 255, 10][..], 0).unwrap();
        assert!(output.eof && output.truncated && output.bytes.is_empty());
        assert_eq!(output.observed_bytes, 3);
    }
}
