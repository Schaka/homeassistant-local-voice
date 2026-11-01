//! PCM conversion and WAV files.

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub width: u16,
    pub channels: u16,
}

impl Format {
    pub const STT: Self = Self {
        rate: 16_000,
        width: 2,
        channels: 1,
    };
}

fn decode_sample(bytes: &[u8]) -> i32 {
    match bytes.len() {
        1 => (i32::from(bytes[0]) - 128) << 8,
        2 => i32::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        3 => i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 16,
        4 => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 16,
        _ => 0,
    }
}

/// Converts raw PCM to 16-bit mono at `rate`: channels are averaged, the rate
/// changes by linear interpolation. Home Assistant already sends 16 kHz
/// 16-bit mono, so in practice this copies the samples.
pub fn to_mono_i16(data: &[u8], from: Format, rate: u32) -> Vec<i16> {
    let width = usize::from(from.width.max(1));
    let channels = usize::from(from.channels.max(1));
    let frame = width * channels;
    let mono: Vec<i32> = data
        .chunks_exact(frame)
        .map(|f| {
            let sum: i32 = f.chunks_exact(width).map(decode_sample).sum();
            sum / i32::try_from(channels).unwrap_or(1)
        })
        .collect();
    let clamp = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    if from.rate == rate || from.rate == 0 || mono.is_empty() {
        return mono.into_iter().map(clamp).collect();
    }
    let out_len = (mono.len() as u64 * u64::from(rate) / u64::from(from.rate)) as usize;
    let step = f64::from(from.rate) / f64::from(rate);
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * step;
            let idx = pos as usize;
            let frac = pos - idx as f64;
            let a = f64::from(mono[idx.min(mono.len() - 1)]);
            let b = f64::from(mono[(idx + 1).min(mono.len() - 1)]);
            clamp((a + (b - a) * frac).round() as i32)
        })
        .collect()
}

/// A 16-bit PCM WAV file.
pub fn wav_bytes(pcm: &[i16], rate: u32, channels: u16) -> Vec<u8> {
    let data_len = u32::try_from(pcm.len() * 2).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Format and PCM data of a WAV file. A data size of 0 or 0xFFFFFFFF (a
/// streamed file) means "up to the end".
pub fn parse_wav(data: &[u8]) -> Result<(Format, &[u8])> {
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        bail!("TTS response is not a RIFF/WAV file");
    }
    let mut pos = 12;
    let mut format = None;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            if body + 16 > data.len() {
                bail!("WAV fmt chunk truncated");
            }
            let f = &data[body..body + 16];
            format = Some(Format {
                channels: u16::from_le_bytes([f[2], f[3]]),
                rate: u32::from_le_bytes([f[4], f[5], f[6], f[7]]),
                width: u16::from_le_bytes([f[14], f[15]]) / 8,
            });
        } else if id == b"data" {
            let fmt = format.ok_or_else(|| anyhow::anyhow!("WAV data before fmt"))?;
            let end = if size == 0 || size == 0xFFFF_FFFF {
                data.len()
            } else {
                (body + size).min(data.len())
            };
            return Ok((fmt, &data[body..end]));
        }
        pos = body + size + (size & 1);
    }
    bail!("WAV without data chunk")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_roundtrip() {
        let pcm = [0i16, 1000, -1000, i16::MAX];
        let wav = wav_bytes(&pcm, 24_000, 1);
        let (fmt, data) = parse_wav(&wav).unwrap();
        assert_eq!(
            fmt,
            Format {
                rate: 24_000,
                width: 2,
                channels: 1
            }
        );
        assert_eq!(data.len(), 8);
    }

    #[test]
    fn stereo_48k_to_mono_16k() {
        let mut data = Vec::new();
        for _ in 0..48 {
            data.extend_from_slice(&1000i16.to_le_bytes());
            data.extend_from_slice(&3000i16.to_le_bytes());
        }
        let out = to_mono_i16(
            &data,
            Format {
                rate: 48_000,
                width: 2,
                channels: 2,
            },
            16_000,
        );
        assert_eq!(out.len(), 16);
        assert!(out.iter().all(|&s| s == 2000));
    }

    #[test]
    fn same_format_is_a_copy() {
        let pcm = [1i16, -2, 3];
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(to_mono_i16(&bytes, Format::STT, 16_000), pcm);
    }
}
