use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub struct SegmentRing {
    segments: VecDeque<PathBuf>,
    segments_dir: PathBuf,
    max_count: usize,
}

impl SegmentRing {
    pub fn new(segments_dir: PathBuf, max_secs: u32) -> Self {
        Self {
            segments: VecDeque::new(),
            segments_dir,
            max_count: max_secs as usize + 4,
        }
    }

    pub fn update(&mut self, clip_secs: u32) {
        self.max_count = clip_secs as usize + 4;
        let completed_cutoff = SystemTime::now() - Duration::from_secs(2);
        let known: HashSet<&Path> = self.segments.iter().map(PathBuf::as_path).collect();

        let mut new_segs: Vec<(SystemTime, PathBuf)> = vec![];
        if let Ok(dir) = std::fs::read_dir(&self.segments_dir) {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "ts").unwrap_or(false)
                    && !known.contains(path.as_path())
                {
                    if let Ok(meta) = entry.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            if mtime < completed_cutoff {
                                new_segs.push((mtime, path));
                            }
                        }
                    }
                }
            }
        }

        new_segs.sort_by_key(|(m, _)| *m);
        for (_, path) in new_segs {
            self.segments.push_back(path);
        }

        while self.segments.len() > self.max_count {
            if let Some(old) = self.segments.pop_front() {
                let _ = std::fs::remove_file(&old);
            }
        }
    }

    pub fn clear(&mut self) {
        for path in self.segments.drain(..) {
            let _ = std::fs::remove_file(&path);
        }
    }

    pub fn get_clip_segments(&self, duration_secs: u32) -> Vec<PathBuf> {
        let want = (duration_secs as usize).min(self.segments.len());
        let start = self.segments.len() - want;
        self.segments.iter().skip(start).cloned().collect()
    }
}
