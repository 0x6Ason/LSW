// SPDX-License-Identifier: GPL-3.0-or-later

//! CPU correctness path for bounded, damage-aware seamless-window frames.

use lsw_core::{GuiWindowDamage, LswError, Result, MAX_GUI_FRAME_BYTES, MAX_GUI_WINDOW_DIMENSION};

const TILE_SIDE: usize = 128;

#[derive(Default)]
pub(super) struct DamageTracker {
    width: usize,
    height: usize,
    previous: Vec<u8>,
    sequence: u64,
}

impl DamageTracker {
    pub(super) fn update(
        &mut self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Result<Vec<GuiWindowDamage>> {
        let (width, height, frame_bytes) = validate_frame(width, height, bgra)?;
        let resized = self.width != width || self.height != height;
        if resized {
            self.width = width;
            self.height = height;
            self.previous.clear();
            self.previous.resize(frame_bytes, 0);
        }

        self.sequence = self.sequence.checked_add(1).unwrap_or(1);
        let mut damages = Vec::new();
        for y in (0..height).step_by(TILE_SIDE) {
            let tile_height = TILE_SIDE.min(height - y);
            for x in (0..width).step_by(TILE_SIDE) {
                let tile_width = TILE_SIDE.min(width - x);
                if !resized
                    && tile_matches(&self.previous, bgra, width, x, y, tile_width, tile_height)
                {
                    continue;
                }
                let mut tile = Vec::with_capacity(tile_width * tile_height * 4);
                for row in y..y + tile_height {
                    let start = (row * width + x) * 4;
                    tile.extend_from_slice(&bgra[start..start + tile_width * 4]);
                }
                damages.push(GuiWindowDamage {
                    sequence: self.sequence,
                    x: u32::try_from(x).expect("validated GUI x fits u32"),
                    y: u32::try_from(y).expect("validated GUI y fits u32"),
                    width: u32::try_from(tile_width).expect("GUI tile width fits u32"),
                    height: u32::try_from(tile_height).expect("GUI tile height fits u32"),
                    bgra: tile,
                });
            }
        }
        if resized || !damages.is_empty() {
            self.previous.copy_from_slice(bgra);
        }
        Ok(damages)
    }
}

fn validate_frame(width: u32, height: u32, bgra: &[u8]) -> Result<(usize, usize, usize)> {
    if width == 0
        || height == 0
        || width > MAX_GUI_WINDOW_DIMENSION
        || height > MAX_GUI_WINDOW_DIMENSION
    {
        return Err(LswError::Protocol(format!(
            "captured GUI dimensions must be between 1 and {MAX_GUI_WINDOW_DIMENSION}"
        )));
    }
    let width = usize::try_from(width)
        .map_err(|_| LswError::Protocol("captured GUI width does not fit usize".to_owned()))?;
    let height = usize::try_from(height)
        .map_err(|_| LswError::Protocol("captured GUI height does not fit usize".to_owned()))?;
    let frame_bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| LswError::Protocol("captured GUI frame length overflowed".to_owned()))?;
    if frame_bytes > MAX_GUI_FRAME_BYTES {
        return Err(LswError::Protocol(format!(
            "captured GUI frame exceeds the {MAX_GUI_FRAME_BYTES} byte limit"
        )));
    }
    if bgra.len() != frame_bytes {
        return Err(LswError::Protocol(format!(
            "captured GUI frame contains {} bytes; expected {frame_bytes}",
            bgra.len()
        )));
    }
    Ok((width, height, frame_bytes))
}

fn tile_matches(
    previous: &[u8],
    current: &[u8],
    frame_width: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> bool {
    (y..y + height).all(|row| {
        let start = (row * frame_width + x) * 4;
        previous[start..start + width * 4] == current[start..start + width * 4]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_is_tiled_and_identical_frame_has_no_damage() {
        let mut tracker = DamageTracker::default();
        let frame = vec![7; 130 * 129 * 4];
        let first = tracker.update(130, 129, &frame).unwrap();
        assert_eq!(first.len(), 4);
        assert_eq!((first[0].width, first[0].height), (128, 128));
        assert_eq!((first[3].x, first[3].y), (128, 128));
        assert!(tracker.update(130, 129, &frame).unwrap().is_empty());
    }

    #[test]
    fn one_changed_pixel_damages_only_its_tile() {
        let mut tracker = DamageTracker::default();
        let mut frame = vec![0; 256 * 256 * 4];
        tracker.update(256, 256, &frame).unwrap();
        frame[(200 * 256 + 201) * 4] = 1;
        let damage = tracker.update(256, 256, &frame).unwrap();
        assert_eq!(damage.len(), 1);
        assert_eq!((damage[0].x, damage[0].y), (128, 128));
        assert_eq!((damage[0].width, damage[0].height), (128, 128));
    }

    #[test]
    fn resize_forces_a_complete_new_frame() {
        let mut tracker = DamageTracker::default();
        tracker.update(64, 64, &vec![0; 64 * 64 * 4]).unwrap();
        let damage = tracker.update(129, 64, &vec![0; 129 * 64 * 4]).unwrap();
        assert_eq!(damage.len(), 2);
        assert_eq!((damage[1].x, damage[1].width), (128, 1));
    }

    #[test]
    fn malformed_or_unbounded_frames_are_rejected() {
        let mut tracker = DamageTracker::default();
        assert!(tracker.update(1, 1, &[0; 3]).is_err());
        assert!(tracker
            .update(MAX_GUI_WINDOW_DIMENSION + 1, 1, &[])
            .is_err());
    }
}
