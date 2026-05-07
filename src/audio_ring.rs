use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub struct AudioChunk {
    pub data: Vec<u8>,
    pub num_frames: u32,
    pub captured_at: Instant,
}

pub struct AudioRing {
    chunks: VecDeque<AudioChunk>,
    clip_secs: Arc<AtomicU32>,
}

impl AudioRing {
    pub fn new(clip_secs: Arc<AtomicU32>) -> Self {
        Self { chunks: VecDeque::new(), clip_secs }
    }

    pub fn push(&mut self, chunk: AudioChunk) {
        self.chunks.push_back(chunk);
        let max_secs = self.clip_secs.load(Ordering::Relaxed) as u64 + 8;
        let cutoff = Instant::now() - Duration::from_secs(max_secs);
        while self.chunks.front().map(|c| c.captured_at < cutoff).unwrap_or(false) {
            self.chunks.pop_front();
        }
    }

    /// Returns raw PCM bytes covering exactly [start, end], with silence filling any gaps.
    /// Chunks are placed at their correct time offset so silence periods are preserved.
    pub fn get_clip_data_for_range(
        &self,
        start: Instant,
        end: Instant,
        sample_rate: u32,
        block_align: u16,
    ) -> Vec<u8> {
        if end <= start {
            return vec![];
        }
        let bytes_per_sec = sample_rate as usize * block_align as usize;
        let total_bytes = (end.duration_since(start).as_secs_f64() * bytes_per_sec as f64) as usize;
        let total_bytes = (total_bytes / block_align as usize) * block_align as usize;

        let mut result = vec![0u8; total_bytes];

        for chunk in &self.chunks {
            let chunk_dur = Duration::from_secs_f64(chunk.num_frames as f64 / sample_rate as f64);
            let chunk_start = match chunk.captured_at.checked_sub(chunk_dur) {
                Some(t) => t,
                None => continue,
            };

            if chunk.captured_at <= start || chunk_start >= end {
                continue;
            }

            let result_offset = if chunk_start >= start {
                let secs = chunk_start.duration_since(start).as_secs_f64();
                ((secs * bytes_per_sec as f64) as usize / block_align as usize) * block_align as usize
            } else {
                0
            };

            let chunk_skip = if chunk_start < start {
                let secs = start.duration_since(chunk_start).as_secs_f64();
                ((secs * bytes_per_sec as f64) as usize / block_align as usize) * block_align as usize
            } else {
                0
            };

            if chunk_skip >= chunk.data.len() {
                continue;
            }

            let src = &chunk.data[chunk_skip..];
            let available = total_bytes.saturating_sub(result_offset);
            let len = src.len().min(available);

            if len > 0 {
                result[result_offset..result_offset + len].copy_from_slice(&src[..len]);
            }
        }

        result
    }
}
