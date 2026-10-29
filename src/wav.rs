//! 16-bit mono WAV writer that keeps its header valid while recording.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::Result;

pub struct WavWriter {
    file: BufWriter<File>,
    rate: u32,
    data_bytes: u32,
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32) -> Result<Self> {
        let mut w = Self {
            file: BufWriter::new(File::create(path)?),
            rate,
            data_bytes: 0,
        };
        w.write_header()?;
        Ok(w)
    }

    pub fn write(&mut self, pcm: &[f32]) -> Result<()> {
        for &s in pcm {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            self.file.write_all(&v.to_le_bytes())?;
        }
        self.data_bytes += (pcm.len() * 2) as u32;
        Ok(())
    }

    /// Rewrites the sizes so the file is playable even if the process is killed.
    pub fn sync(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.seek(SeekFrom::Start(0))?;
        self.write_header()?;
        self.file.seek(SeekFrom::End(0))?;
        self.file.flush()?;
        Ok(())
    }

    fn write_header(&mut self) -> Result<()> {
        let f = &mut self.file;
        f.write_all(b"RIFF")?;
        f.write_all(&(36 + self.data_bytes).to_le_bytes())?;
        f.write_all(b"WAVEfmt ")?;
        f.write_all(&16u32.to_le_bytes())?;
        f.write_all(&1u16.to_le_bytes())?; // PCM
        f.write_all(&1u16.to_le_bytes())?; // mono
        f.write_all(&self.rate.to_le_bytes())?;
        f.write_all(&(self.rate * 2).to_le_bytes())?;
        f.write_all(&2u16.to_le_bytes())?;
        f.write_all(&16u16.to_le_bytes())?;
        f.write_all(b"data")?;
        f.write_all(&self.data_bytes.to_le_bytes())?;
        Ok(())
    }
}

impl Drop for WavWriter {
    fn drop(&mut self) {
        let _ = self.sync();
    }
}
