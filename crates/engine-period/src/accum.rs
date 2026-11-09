//! The running grids of one window (#1379): an `f32` accumulator and a
//! `u16` count of the frames with a value at each cell, 6 bytes per cell,
//! whatever the window's length. A frame is added once when it enters the
//! window and subtracted once when it leaves, strip by strip, with the same
//! contribution bit for bit, so only `f32` rounding separates the running grid from a
//! sum from scratch; the engine re-sums every window periodically to clear
//! it. Pure functions, no I/O.

/// The statistic a collection serves over each window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Statistic {
    /// The time integral: each frame's value times the hours it counts
    /// for. A rain rate in mm/h sums to an amount in mm.
    Sum,
    /// The mean of the frames' values at the cell, over the frames with a
    /// value there.
    Mean,
    /// The frames at or above the threshold.
    CountAbove(f32),
}

impl Statistic {
    /// Whether a frame's contribution depends on the time it counts for:
    /// only then does a retimed frame change the grids.
    pub fn weighs_time(self) -> bool {
        matches!(self, Statistic::Sum)
    }

    /// What a frame with value `v` adds at a cell when it counts for
    /// `hours`. NaN (nodata) adds nothing.
    #[inline]
    fn contribution(self, v: f32, hours: f32) -> f32 {
        match self {
            Statistic::Sum => v * hours,
            Statistic::Mean => v,
            Statistic::CountAbove(threshold) => {
                if v.is_nan() {
                    f32::NAN
                } else if v >= threshold {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }
}

/// One frame's values on the working grid, as the statistic's input (the
/// rain rate when the collection converts reflectivity). NaN is nodata.
/// Raw bytes keep one byte per cell and decode through a 256-entry table.
pub enum Frame {
    U8 {
        data: Vec<u8>,
        table: Box<[f32; 256]>,
    },
    F32(Vec<f32>),
}

impl Frame {
    pub fn len(&self) -> usize {
        match self {
            Frame::U8 { data, .. } => data.len(),
            Frame::F32(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The value at cell `i`.
    #[inline]
    pub fn value(&self, i: usize) -> f32 {
        match self {
            Frame::U8 { data, table } => table[data[i] as usize],
            Frame::F32(values) => values[i],
        }
    }

    /// A digest of the values, as the statistic's input, cell by cell (NaN
    /// as one value): equal for a strip read again unchanged, so a frame
    /// revised in place since it was added is never subtracted with its new
    /// values. Not cryptographic: it guards against revisions, not attacks.
    pub fn digest(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ self.len() as u64;
        let mut mix = |v: f32| {
            let bits = if v.is_nan() { 0x7fc0_0000 } else { v.to_bits() };
            h = (h ^ u64::from(bits)).wrapping_mul(0x0000_0100_0000_01b3);
            h ^= h >> 29;
        };
        match self {
            Frame::U8 { data, table } => data.iter().for_each(|&raw| mix(table[raw as usize])),
            Frame::F32(values) => values.iter().for_each(|&v| mix(v)),
        }
        h
    }

    /// Bytes its buffer holds.
    pub fn bytes(&self) -> u64 {
        match self {
            Frame::U8 { data, .. } => data.capacity() as u64 + 1024,
            Frame::F32(values) => (values.capacity() * 4) as u64,
        }
    }
}

/// Whether a frame enters or leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    Remove,
}

/// A window's running grids.
#[derive(Clone)]
pub struct Grids {
    acc: Vec<f32>,
    valid: Vec<u16>,
}

impl Grids {
    pub fn new(cells: usize) -> Self {
        Self {
            acc: vec![0.0; cells],
            valid: vec![0; cells],
        }
    }

    pub fn cells(&self) -> usize {
        self.acc.len()
    }

    /// Bytes the two grids hold.
    pub fn bytes(&self) -> u64 {
        (self.acc.capacity() * 4 + self.valid.capacity() * 2) as u64
    }

    /// Add or take out `frame`, counting for `secs`. A cell whose last
    /// frame leaves goes back to exactly zero, so no rounding remainder of
    /// frames long gone is left behind.
    pub fn apply(&mut self, frame: &Frame, statistic: Statistic, secs: u32, op: Op) {
        debug_assert_eq!(frame.len(), self.cells());
        self.apply_at(frame, statistic, secs, op, 0);
    }

    /// [`Self::apply`] for part of a frame, a strip of whole rows whose
    /// first cell is cell `offset` of the grid. Each cell gets exactly the
    /// operation it would get from the whole frame, so folding a frame in
    /// strips leaves the grids bit for bit as folding it whole.
    pub fn apply_at(
        &mut self,
        frame: &Frame,
        statistic: Statistic,
        secs: u32,
        op: Op,
        offset: usize,
    ) {
        let end = offset + frame.len();
        assert!(
            end <= self.cells(),
            "strip past the grid: {offset}+{}",
            frame.len()
        );
        let acc = &mut self.acc[offset..end];
        let valid = &mut self.valid[offset..end];
        let hours = secs as f32 / 3600.0;
        match frame {
            Frame::U8 { data, table } => {
                let mut contribution = [f32::NAN; 256];
                for (c, &v) in contribution.iter_mut().zip(table.iter()) {
                    *c = statistic.contribution(v, hours);
                }
                for ((acc, valid), &raw) in acc.iter_mut().zip(valid).zip(data) {
                    step(acc, valid, contribution[raw as usize], op);
                }
            }
            Frame::F32(values) => {
                for ((acc, valid), &v) in acc.iter_mut().zip(valid).zip(values) {
                    step(acc, valid, statistic.contribution(v, hours), op);
                }
            }
        }
    }

    /// The frames with a value at cell `i`.
    #[inline]
    pub fn valid(&self, i: usize) -> u16 {
        self.valid[i]
    }

    /// The statistic at cell `i`: NaN where no frame of the window has a
    /// value. `non_negative` clamps a sum or mean of non-negative inputs
    /// (rain rates) at zero, where rounding could leave `-0.000001`.
    #[inline]
    pub fn value(&self, i: usize, statistic: Statistic, non_negative: bool) -> f32 {
        let n = self.valid[i];
        if n == 0 {
            return f32::NAN;
        }
        let acc = self.acc[i];
        let v = match statistic {
            Statistic::Sum => acc,
            Statistic::Mean => acc / f32::from(n),
            Statistic::CountAbove(_) => acc.round(),
        };
        if non_negative {
            v.max(0.0)
        } else {
            v
        }
    }

    /// The share of the window covered at cell `i`: the window's coverage
    /// scaled by the share of its frames with a value there. NaN where none
    /// has, as for the statistic.
    #[inline]
    pub fn coverage(&self, i: usize, window_coverage: f32, frames: usize) -> f32 {
        let n = self.valid[i];
        if n == 0 || frames == 0 {
            return f32::NAN;
        }
        window_coverage * f32::from(n) / frames as f32
    }
}

#[inline]
fn step(acc: &mut f32, valid: &mut u16, x: f32, op: Op) {
    if x.is_nan() {
        return;
    }
    match op {
        Op::Add => {
            *acc += x;
            *valid += 1;
        }
        Op::Remove => {
            debug_assert!(*valid > 0, "removing a frame that was never added");
            *valid = valid.saturating_sub(1);
            if *valid == 0 {
                *acc = 0.0;
            } else {
                *acc -= x;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(values: &[f32]) -> Frame {
        Frame::F32(values.to_vec())
    }

    #[test]
    fn sum_mean_and_count_match_a_direct_computation() {
        let frames = [
            frame(&[1.0, f32::NAN, 4.0]),
            frame(&[3.0, f32::NAN, 0.0]),
            frame(&[2.0, 5.0, f32::NAN]),
        ];
        let secs = [300, 300, 600];
        for statistic in [Statistic::Sum, Statistic::Mean, Statistic::CountAbove(2.0)] {
            let mut g = Grids::new(3);
            for (f, &s) in frames.iter().zip(&secs) {
                g.apply(f, statistic, s, Op::Add);
            }
            let got: Vec<f32> = (0..3).map(|i| g.value(i, statistic, false)).collect();
            let want: [f32; 3] = match statistic {
                Statistic::Sum => [1.0 / 12.0 + 3.0 / 12.0 + 2.0 / 6.0, 5.0 / 6.0, 4.0 / 12.0],
                Statistic::Mean => [2.0, 5.0, 2.0],
                Statistic::CountAbove(_) => [2.0, 1.0, 1.0],
            };
            for (g, w) in got.iter().zip(want) {
                assert!((g - w).abs() < 1e-6, "{statistic:?}: {got:?} vs {want:?}");
            }
            assert_eq!((g.valid(0), g.valid(1), g.valid(2)), (3, 1, 2));
        }
    }

    /// Nodata in every frame stays nodata; a cell whose frames all left is
    /// nodata again, and its accumulator exactly zero.
    #[test]
    fn nodata_stays_nodata_and_a_cell_empties_exactly() {
        let mut g = Grids::new(2);
        let a = frame(&[0.1, f32::NAN]);
        let b = frame(&[0.7, f32::NAN]);
        g.apply(&a, Statistic::Sum, 300, Op::Add);
        g.apply(&b, Statistic::Sum, 300, Op::Add);
        assert!(g.value(1, Statistic::Sum, true).is_nan());
        g.apply(&a, Statistic::Sum, 300, Op::Remove);
        g.apply(&b, Statistic::Sum, 300, Op::Remove);
        assert!(g.value(0, Statistic::Sum, true).is_nan());
        assert_eq!(g.acc[0].to_bits(), 0f32.to_bits());
    }

    /// A raw-byte frame decodes through its table exactly as the same
    /// values as f32 would.
    #[test]
    fn raw_bytes_and_f32_frames_add_the_same() {
        let mut table = Box::new([f32::NAN; 256]);
        for (i, v) in table.iter_mut().enumerate().take(255) {
            *v = i as f32 * 0.5;
        }
        let raw = Frame::U8 {
            data: vec![0, 7, 255, 200],
            table,
        };
        let float = frame(&[0.0, 3.5, f32::NAN, 100.0]);
        for statistic in [Statistic::Sum, Statistic::Mean, Statistic::CountAbove(3.5)] {
            let (mut a, mut b) = (Grids::new(4), Grids::new(4));
            a.apply(&raw, statistic, 300, Op::Add);
            b.apply(&float, statistic, 300, Op::Add);
            for i in 0..4 {
                assert_eq!(
                    a.value(i, statistic, false).to_bits(),
                    b.value(i, statistic, false).to_bits()
                );
            }
        }
    }

    /// The digest is the values': raw bytes and the same values as `f32`
    /// agree, and one changed cell changes it.
    #[test]
    fn a_digest_follows_the_values() {
        let mut table = Box::new([f32::NAN; 256]);
        for (i, v) in table.iter_mut().enumerate().take(255) {
            *v = i as f32 * 0.5;
        }
        let raw = Frame::U8 {
            data: vec![0, 7, 255, 200],
            table,
        };
        let float = frame(&[0.0, 3.5, f32::NAN, 100.0]);
        assert_eq!(raw.digest(), float.digest());
        assert_ne!(float.digest(), frame(&[0.0, 3.5, 0.0, 100.0]).digest());
        assert_ne!(float.digest(), frame(&[0.0, 3.5, f32::NAN]).digest());
    }

    #[test]
    fn coverage_scales_by_the_frames_with_a_value() {
        let mut g = Grids::new(2);
        g.apply(&frame(&[1.0, 1.0]), Statistic::Sum, 300, Op::Add);
        g.apply(&frame(&[1.0, f32::NAN]), Statistic::Sum, 300, Op::Add);
        assert_eq!(g.coverage(0, 0.9, 2), 0.9);
        assert_eq!(g.coverage(1, 0.9, 2), 0.45);
        assert_eq!(g.bytes(), 2 * 6);
    }
}
