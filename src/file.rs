use crate::{Radio, RadioRx, RadioTx, error::Error};
use hound::WavWriter;
use num_complex::Complex32;
use std::io::{BufWriter, Seek, Write};
const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;
const FILE_OUTPUT_TARGET_PEAK: f32 = 0.90;
const FILE_OUTPUT_HARD_CLIP_PEAK: f32 = 1.0;
/// Writes TX samples to a stereo WAV file: I in the left channel and Q in the right,
/// both as signed 16-bit samples. Available with `file`.
///
/// Input components are scaled by 0.90 and then by `i16::MAX`. Exceeding unit
/// amplitude after scaling returns an error, possibly after writing a prefix.
/// Timestamps track the position in the file, in nanoseconds. They do not follow wall time.
/// Timed writes round to the nearest sample, pad gaps with silence, and reject
/// overlaps. TX enable and disable do not suppress file writes. Drop the TX half
/// to finish the WAV header before reading the file.
pub struct FileOutputRadio<W>
where
    W: Write + Seek,
{
    sink: WavWriter<BufWriter<W>>,
    sample_rate_hz: usize,
}

impl<W> FileOutputRadio<W>
where
    W: Write + Seek,
{
    /// Creates a WAV stream with a fixed sample rate in Hz. The rate must be positive
    /// and fit in `u32`. Later sample-rate requests must match. Returns I/O or WAV errors.
    pub fn new(writer: W, sample_rate_hz: usize) -> Result<FileOutputRadio<W>, Error> {
        if sample_rate_hz == 0 || sample_rate_hz > u32::MAX as usize {
            return Err(Error::invalid_setting(
                "WAV sample rate must be positive and fit in u32",
            ));
        }
        Ok(FileOutputRadio {
            sample_rate_hz,
            sink: WavWriter::new(
                BufWriter::new(writer),
                hound::WavSpec {
                    channels: 2,
                    sample_rate: sample_rate_hz as u32,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )?,
        })
    }
}

impl<W> Radio for FileOutputRadio<W>
where
    W: Write + Seek + Send + 'static,
{
    fn tx_capabilities(&self) -> crate::TxCapabilities {
        crate::TxCapabilities::from([
            crate::TxCapability::TimedSamples,
            crate::TxCapability::BurstEnd,
        ])
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        self.sample_rate_hz as f64
    }
    fn rx_sample_rate_hz(&self) -> Option<f64> {
        None
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }

    fn tick_rate(&self) -> u64 {
        NANOSECONDS_PER_SECOND
    }

    fn set_tx_frequency(&mut self, _center_frequency: usize) -> Result<(), Error> {
        Ok(())
    }

    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error> {
        if sample_rate_hz != self.sample_rate_hz as f64 {
            return Err(format!(
                "file output was opened at TX sample rate {}, requested {sample_rate_hz}",
                self.sample_rate_hz
            )
            .into());
        }
        Ok(sample_rate_hz)
    }

    fn set_tx_bandwidth(&mut self, _bandwidth: usize) -> Result<(), Error> {
        Ok(())
    }

    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        let tx = FileOutputTxHalf {
            sink: self.sink,
            sample_rate_hz: self.sample_rate_hz as u64,
            sample_position: 0,
        };
        Ok((Box::new(tx), None))
    }
}

struct FileOutputTxHalf<W: Write + Seek + Send> {
    sink: WavWriter<BufWriter<W>>,
    sample_rate_hz: u64,
    sample_position: u64,
}

impl<W> RadioTx for FileOutputTxHalf<W>
where
    W: Write + Seek + Send,
{
    fn capabilities(&self) -> crate::TxCapabilities {
        crate::TxCapabilities::from([
            crate::TxCapability::TimedSamples,
            crate::TxCapability::BurstEnd,
        ])
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        Ok(())
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        self.sample_rate_hz as f64
    }
    fn set_tx_frequency_hz(&mut self, _frequency_hz: f64) -> Result<(), Error> {
        Ok(())
    }

    fn supports_timed_tx(&self) -> bool {
        true
    }

    fn tick_rate(&self) -> u64 {
        NANOSECONDS_PER_SECOND
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(
            ((self.sample_position as u128 * NANOSECONDS_PER_SECOND as u128)
                / self.sample_rate_hz as u128) as u64,
        )
    }

    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.write_samples(samples)
    }

    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        if let Some(tick) = tick {
            let position = ((tick as u128 * self.sample_rate_hz as u128
                + NANOSECONDS_PER_SECOND as u128 / 2)
                / NANOSECONDS_PER_SECOND as u128) as u64;
            if position < self.sample_position {
                return Err(format!(
                    "WAV timed TX overlaps existing samples: requested={position} next={}",
                    self.sample_position
                )
                .into());
            }
            while self.sample_position < position {
                self.sink.write_sample(0_i16)?;
                self.sink.write_sample(0_i16)?;
                self.sample_position += 1;
            }
        }
        self.write_samples(samples)
    }

    fn enable_transmit(&mut self, _enable: bool) -> Result<(), Error> {
        Ok(())
    }
}

impl<W> FileOutputTxHalf<W>
where
    W: Write + Seek + Send,
{
    fn write_samples(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        for (idx, sample) in samples.iter().enumerate() {
            // Keep deterministic fixed scaling for WAV output. Any overflow is a TX bug.
            let re = sample.re * FILE_OUTPUT_TARGET_PEAK;
            let im = sample.im * FILE_OUTPUT_TARGET_PEAK;
            if re.abs() > FILE_OUTPUT_HARD_CLIP_PEAK || im.abs() > FILE_OUTPUT_HARD_CLIP_PEAK {
                log::error!(
                    "TX hard clip detected in FileOutputRadio: idx={} raw_re={} raw_im={} scaled_re={} scaled_im={} hard_limit={} target_peak={}",
                    idx,
                    sample.re,
                    sample.im,
                    re,
                    im,
                    FILE_OUTPUT_HARD_CLIP_PEAK,
                    FILE_OUTPUT_TARGET_PEAK
                );
                return Err("TX hard clip: signal exceeds file output range".into());
            }
            self.sink.write_sample((re * (i16::MAX as f32)) as i16)?;
            self.sink.write_sample((im * (i16::MAX as f32)) as i16)?;
            self.sample_position += 1;
        }
        self.sink.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn timed_wav_places_samples_and_rejects_overlap() {
        let mut bytes = Cursor::new(Vec::new());
        {
            let radio = FileOutputRadio::new(&mut bytes, 2_000_000).unwrap();
            let mut tx = FileOutputTxHalf {
                sink: radio.sink,
                sample_rate_hz: 2_000_000,
                sample_position: 0,
            };
            assert!(tx.supports_timed_tx());
            tx.transmit_at(&[Complex32::new(0.5, -0.25)], Some(1_000))
                .unwrap();
            assert_eq!(tx.sample_position, 3);
            assert_eq!(tx.get_hardware_time().unwrap(), 1_500);
            assert!(
                tx.transmit_at(&[Complex32::new(0.5, 0.0)], Some(1_000))
                    .is_err()
            );
            tx.transmit_at(&[Complex32::new(0.25, 0.5)], Some(2_500))
                .unwrap();
            assert_eq!(tx.sample_position, 6);
            assert_eq!(tx.get_hardware_time().unwrap(), 3_000);
            drop(tx);
        }
        bytes.set_position(0);
        let mut reader = hound::WavReader::new(bytes).unwrap();
        assert_eq!(reader.spec().sample_rate, 2_000_000);
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.duration(), 6);
        let samples: Vec<i16> = reader.samples().map(Result::unwrap).collect();
        assert_eq!(samples, [0, 0, 0, 0, 14745, -7372, 0, 0, 0, 0, 7372, 14745]);
    }

    #[test]
    fn timed_wav_rounds_fractional_nanosecond_sample_boundaries() {
        let radio = FileOutputRadio::new(Cursor::new(Vec::new()), 3_000_000).unwrap();
        let (mut tx, _) = Box::new(radio).split().unwrap();
        for position in 0..100_u64 {
            let tick = position * NANOSECONDS_PER_SECOND / 3_000_000;
            tx.transmit_at(&[Complex32::new(0.1, 0.0)], Some(tick))
                .unwrap();
        }
        assert_eq!(tx.get_hardware_time().unwrap(), 33_333);
    }
}
