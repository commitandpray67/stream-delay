//! Picture order counts that a decoder holding on to pictures across a broken
//! link never sees twice.
//!
//! A BLA picture (or a CRA after an end of sequence) starts a new coded video
//! sequence. By the standard the decoder lets go of every picture it held, so
//! the new sequence may number its pictures from anywhere. FFmpeg's decoder
//! (from version 8) lets go of them only after it has checked the new
//! picture's number against them, and drops a picture that repeats one
//! ("Duplicate POC in a sequence"). A held frame sent again is such a picture,
//! and now and then so is the keyframe a splice lands on.
//!
//! [`PocGuard`] follows the pictures written to the destination and what such
//! a decoder holds after each. Where a new sequence would repeat a number held,
//! it renumbers the sequence: every slice's `slice_pic_order_cnt_lsb` moves by
//! the same amount, so the pictures keep their order and their references.

use crate::{VideoCodec, inspect_video};

const TRAIL_N: u8 = 0;
const TSA_N: u8 = 2;
const STSA_N: u8 = 4;
const RADL_N: u8 = 6;
const RADL_R: u8 = 7;
const BLA_N_LP: u8 = 18;
const IDR_W_RADL: u8 = 19;
const IDR_N_LP: u8 = 20;
const SPS_NUT: u8 = 33;
const PPS_NUT: u8 = 34;
const EOS_NUT: u8 = 36;
const EOB_NUT: u8 = 37;

/// Enough of a slice segment's escaped bytes for its header up to the
/// reference picture set.
const SLICE_HEADER_BYTES: usize = 512;

/// Removes emulation prevention bytes: the RBSP of a NAL unit.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Adds emulation prevention bytes: the NAL unit for an RBSP (which begins
/// with the two-byte NAL unit header, never 0x0000).
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 1);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    // An RBSP ending in a cabac_zero_word.
    if out.last() == Some(&0) {
        out.push(3);
    }
    out
}

/// Reads an RBSP bit by bit.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    /// After the NAL unit header.
    fn new(rbsp: &[u8]) -> Bits<'_> {
        Bits { b: rbsp, pos: 16 }
    }

    fn bit(&mut self) -> Option<bool> {
        let byte = *self.b.get(self.pos / 8)?;
        self.pos += 1;
        Some((byte >> (7 - (self.pos - 1) % 8)) & 1 == 1)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        u32::try_from(v).ok()
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.b.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// ue(v)
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        u32::try_from((1u64 << zeros) - 1 + self.bits(zeros)? as u64).ok()
    }

    /// se(v)
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -k / 2 } as i32)
    }
}

/// Overwrites `n` bits at bit `pos` with `v`.
fn put_bits(b: &mut [u8], pos: usize, n: u32, v: u32) {
    for i in 0..n as usize {
        let p = pos + i;
        let mask = 0x80 >> (p % 8);
        if (v >> (n as usize - 1 - i)) & 1 == 1 {
            b[p / 8] |= mask;
        } else {
            b[p / 8] &= !mask;
        }
    }
}

fn ceil_log2(x: u64) -> u32 {
    if x <= 1 {
        0
    } else {
        64 - (x - 1).leading_zeros()
    }
}

/// A short-term reference picture set: the order count differences of the
/// pictures kept, before and after the picture (nearest first).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Rps {
    before: Vec<i32>,
    after: Vec<i32>,
}

/// st_ref_pic_set(`idx`), predicted from `sets` when it says so; `idx` is
/// num_short_term_ref_pic_sets in a slice header.
fn st_ref_pic_set(r: &mut Bits, sets: &[Rps], idx: usize, in_slice: bool) -> Option<Rps> {
    const MAX: usize = 16;
    if idx != 0 && r.bit()? {
        let delta_idx = if in_slice { r.ue()? as usize + 1 } else { 1 };
        let base = sets.get(idx.checked_sub(delta_idx)?)?;
        let negative = r.bit()?;
        let abs = r.ue()? as i64 + 1;
        if abs > 32768 {
            return None;
        }
        let delta = if negative { -abs } else { abs } as i32;
        let n = base.before.len() + base.after.len();
        // Kept: used by the picture, or else use_delta_flag.
        let mut kept = Vec::with_capacity(n + 1);
        for _ in 0..=n {
            let used = r.bit()?;
            kept.push(used || r.bit()?);
        }
        let k = base.before.len();
        // H.265 (7-61) and (7-62).
        let mut before = Vec::new();
        for (j, &d) in base.after.iter().enumerate().rev() {
            if d + delta < 0 && kept[k + j] {
                before.push(d + delta);
            }
        }
        if delta < 0 && kept[n] {
            before.push(delta);
        }
        for (j, &d) in base.before.iter().enumerate() {
            if d + delta < 0 && kept[j] {
                before.push(d + delta);
            }
        }
        let mut after = Vec::new();
        for (j, &d) in base.before.iter().enumerate().rev() {
            if d + delta > 0 && kept[j] {
                after.push(d + delta);
            }
        }
        if delta > 0 && kept[n] {
            after.push(delta);
        }
        for (j, &d) in base.after.iter().enumerate() {
            if d + delta > 0 && kept[k + j] {
                after.push(d + delta);
            }
        }
        return (before.len() + after.len() <= MAX).then_some(Rps { before, after });
    }
    let negative = r.ue()? as usize;
    let positive = r.ue()? as usize;
    if negative >= MAX || positive >= MAX {
        return None;
    }
    let mut list = |count: usize, sign: i32| -> Option<Vec<i32>> {
        let mut out = Vec::with_capacity(count);
        let mut prev = 0;
        for _ in 0..count {
            let d = r.ue()?.checked_add(1).filter(|&d| d <= 32768)? as i32;
            prev += sign * d;
            out.push(prev);
            r.skip(1)?; // used_by_curr_pic_flag
        }
        Some(out)
    };
    let before = list(negative, -1)?;
    let after = list(positive, 1)?;
    Some(Rps { before, after })
}

/// What slice headers need from a sequence parameter set.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sps {
    log2_max_poc_lsb: u32,
    separate_colour_plane: bool,
    /// Width of slice_segment_address.
    address_bits: u32,
    sets: Vec<Rps>,
    long_term_refs: bool,
}

/// profile_tier_level(1, `sub_layers`)
fn skip_profile_tier_level(r: &mut Bits, sub_layers: usize) -> Option<()> {
    // Profile (88 bits) and level of the stream.
    r.skip(96)?;
    let mut present = [(false, false); 7];
    for p in &mut present[..sub_layers] {
        *p = (r.bit()?, r.bit()?);
    }
    if sub_layers > 0 {
        r.skip(2 * (8 - sub_layers))?;
    }
    for &(profile, level) in &present[..sub_layers] {
        if profile {
            r.skip(88)?;
        }
        if level {
            r.skip(8)?;
        }
    }
    Some(())
}

fn skip_scaling_list_data(r: &mut Bits) -> Option<()> {
    for size_id in 0..4 {
        let mut matrix_id = 0;
        while matrix_id < 6 {
            if r.bit()? {
                if size_id > 1 {
                    r.se()?;
                }
                for _ in 0..64.min(1 << (4 + (size_id << 1))) {
                    r.se()?;
                }
            } else {
                r.ue()?;
            }
            matrix_id += if size_id == 3 { 3 } else { 1 };
        }
    }
    Some(())
}

/// The id of a sequence parameter set, and what slices need of it (`None`
/// when it cannot be read).
fn parse_sps(rbsp: &[u8]) -> Option<(usize, Option<Sps>)> {
    let mut r = Bits::new(rbsp);
    r.skip(4)?;
    let sub_layers = r.bits(3)? as usize;
    if sub_layers > 6 {
        return None;
    }
    r.skip(1)?;
    skip_profile_tier_level(&mut r, sub_layers)?;
    let id = r.ue()? as usize;
    if id >= 16 {
        return None;
    }
    Some((id, sps_rest(&mut r, sub_layers)))
}

fn sps_rest(r: &mut Bits, sub_layers: usize) -> Option<Sps> {
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc > 3 {
        return None;
    }
    let separate_colour_plane = chroma_format_idc == 3 && r.bit()?;
    let width = r.ue()? as u64;
    let height = r.ue()? as u64;
    if r.bit()? {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    r.ue()?;
    r.ue()?;
    let log2_max_poc_lsb = r.ue()?.checked_add(4).filter(|&b| b <= 16)?;
    let first = if r.bit()? { 0 } else { sub_layers };
    for _ in first..=sub_layers {
        for _ in 0..3 {
            r.ue()?;
        }
    }
    let log2_ctb_size = r.ue()?.checked_add(3)?.checked_add(r.ue()?)?;
    if log2_ctb_size > 16 {
        return None;
    }
    for _ in 0..4 {
        r.ue()?;
    }
    if r.bit()? && r.bit()? {
        skip_scaling_list_data(r)?;
    }
    r.skip(2)?;
    if r.bit()? {
        r.skip(8)?;
        r.ue()?;
        r.ue()?;
        r.skip(1)?;
    }
    let count = r.ue()? as usize;
    if count > 64 {
        return None;
    }
    let mut sets = Vec::with_capacity(count);
    for i in 0..count {
        let set = st_ref_pic_set(r, &sets, i, false)?;
        sets.push(set);
    }
    let long_term_refs = r.bit()?;
    let ctb = 1u64 << log2_ctb_size;
    let ctbs = width.div_ceil(ctb).checked_mul(height.div_ceil(ctb))?;
    if ctbs == 0 {
        return None;
    }
    Some(Sps {
        log2_max_poc_lsb,
        separate_colour_plane,
        address_bits: ceil_log2(ctbs),
        sets,
        long_term_refs,
    })
}

/// What slice headers need from a picture parameter set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pps {
    sps_id: usize,
    dependent_slices: bool,
    output_flag: bool,
    extra_bits: u32,
}

fn parse_pps(rbsp: &[u8]) -> Option<(usize, Pps)> {
    let mut r = Bits::new(rbsp);
    let id = r.ue()? as usize;
    let sps_id = r.ue()? as usize;
    if id >= 64 || sps_id >= 16 {
        return None;
    }
    Some((
        id,
        Pps {
            sps_id,
            dependent_slices: r.bit()?,
            output_flag: r.bit()?,
            extra_bits: r.bits(3)?,
        },
    ))
}

/// The start of a slice segment header.
struct Slice {
    first: bool,
    sps_id: usize,
    /// For all but IDR pictures (and dependent slice segments, which have
    /// none): the bit position, width and value of slice_pic_order_cnt_lsb.
    lsb: Option<(usize, u32, u32)>,
    rps: Rps,
}

/// Number of the picture by FFmpeg's reckoning (`ff_hevc_compute_poc`): from
/// the previous picture of temporal layer 0, except that a BLA picture starts
/// again from its bits.
fn compute_poc(prev_tid0: i64, lsb: u32, log2_max_lsb: u32, nal_type: u8) -> i64 {
    let max = 1i64 << log2_max_lsb;
    let lsb = lsb as i64;
    let prev_lsb = prev_tid0 % max;
    let prev_msb = prev_tid0 - prev_lsb;
    let msb = if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
        prev_msb + max
    } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
        prev_msb - max
    } else {
        prev_msb
    };
    if (super::BLA_W_LP..=BLA_N_LP).contains(&nal_type) {
        lsb
    } else {
        msb + lsb
    }
}

/// Renumbers HEVC pictures written to one destination, in order, so that a
/// new coded video sequence never repeats a picture order count the decoder
/// still holds (see the module documentation).
///
/// Leaves alone what it cannot follow: other codecs, multitrack video, NAL
/// unit lengths other than four bytes, long-term reference pictures, layers
/// above the base one, and anything it cannot parse.
#[derive(Debug, Clone)]
pub struct PocGuard {
    /// By id: the version it was given when it last changed, its RBSP, and
    /// what slices need of it.
    sps: Vec<Option<(u32, Vec<u8>, Sps)>>,
    pps: Vec<Option<Pps>>,
    versions: u32,
    /// The sequence parameter set of the last picture, by id and version.
    active: Option<(usize, u32)>,
    /// Picture order counts the decoder holds as references after the last
    /// picture, when known.
    held: Option<Vec<i64>>,
    prev_tid0: i64,
    /// The last packet ended a sequence.
    ended: bool,
    /// Pictures that lead the last random access point are not decoded.
    no_rasl_output: bool,
    /// What every picture order count of the current sequence moves by.
    shift: u32,
    /// NAL units have 4-byte lengths.
    lengths_ok: bool,
}

impl Default for PocGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl PocGuard {
    pub fn new() -> Self {
        Self {
            sps: vec![None; 16],
            pps: vec![None; 64],
            versions: 0,
            active: None,
            held: Some(Vec::new()),
            prev_tid0: 0,
            ended: false,
            no_rasl_output: false,
            shift: 0,
            lengths_ok: true,
        }
    }

    /// Takes the body of each video message written, in order. Returns the body
    /// to write instead, when its pictures are renumbered.
    pub fn video(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        let info = inspect_video(payload)?;
        if info.codec != VideoCodec::Hevc || info.multitrack {
            return None;
        }
        if let Some(offset) = info.config_offset {
            self.configuration(payload.get(offset..)?);
            return None;
        }
        self.coded(payload, info.nal_offset?)
    }

    /// An `hvcC` record: its parameter sets.
    fn configuration(&mut self, c: &[u8]) {
        let (Some(&lengths), Some(&arrays)) = (c.get(21), c.get(22)) else {
            return;
        };
        self.lengths_ok = lengths & 3 == 3;
        let mut pos = 23;
        for _ in 0..arrays {
            let Some(h) = c.get(pos..pos + 3) else { return };
            pos += 3;
            for _ in 0..u16::from_be_bytes([h[1], h[2]]) {
                let Some(l) = c.get(pos..pos + 2) else { return };
                let len = u16::from_be_bytes([l[0], l[1]]) as usize;
                let Some(nal) = c.get(pos + 2..pos + 2 + len) else {
                    return;
                };
                self.parameter_set(nal);
                pos += 2 + len;
            }
        }
    }

    fn parameter_set(&mut self, nal: &[u8]) {
        let &[b0, b1, ..] = nal else { return };
        if ((b0 & 1) << 5) | (b1 >> 3) != 0 {
            return;
        }
        match (b0 >> 1) & 0x3f {
            SPS_NUT => {
                let rbsp = unescape(nal);
                if let Some((id, sps)) = parse_sps(&rbsp) {
                    self.set_sps(id, rbsp, sps);
                }
            }
            PPS_NUT => {
                if let Some((id, pps)) = parse_pps(&unescape(nal)) {
                    self.pps[id] = Some(pps);
                }
            }
            _ => {}
        }
    }

    /// Like FFmpeg, keeps a set sent again byte for byte as it was, and
    /// forgets the picture parameter sets that use one that changed.
    fn set_sps(&mut self, id: usize, rbsp: Vec<u8>, sps: Option<Sps>) {
        if let (Some((_, old, _)), Some(_)) = (&self.sps[id], &sps)
            && *old == rbsp
        {
            return;
        }
        self.versions += 1;
        self.sps[id] = sps.map(|s| (self.versions, rbsp, s));
        for p in &mut self.pps {
            if p.is_some_and(|p| p.sps_id == id) {
                *p = None;
            }
        }
    }

    /// The start of a slice segment header, from its RBSP.
    fn slice(&self, rbsp: &[u8], nal_type: u8) -> Option<Slice> {
        let mut r = Bits::new(rbsp);
        let first = r.bit()?;
        if super::is_irap(nal_type) {
            r.skip(1)?;
        }
        let pps = (*self.pps.get(r.ue()? as usize)?)?;
        let (_, _, sps) = self.sps[pps.sps_id].as_ref()?;
        if sps.long_term_refs {
            return None;
        }
        let none = |first| {
            Some(Slice {
                first,
                sps_id: pps.sps_id,
                lsb: None,
                rps: Rps::default(),
            })
        };
        if !first {
            if pps.dependent_slices && r.bit()? {
                return none(false);
            }
            r.skip(sps.address_bits as usize)?;
        }
        r.skip(pps.extra_bits as usize)?;
        r.ue()?;
        r.skip(pps.output_flag as usize + 2 * sps.separate_colour_plane as usize)?;
        if matches!(nal_type, IDR_W_RADL | IDR_N_LP) {
            return none(first);
        }
        let at = r.pos;
        let lsb = r.bits(sps.log2_max_poc_lsb)?;
        let rps = if r.bit()? {
            let n = sps.sets.len();
            let idx = r.bits(ceil_log2(n as u64))? as usize;
            sps.sets.get(idx)?.clone()
        } else {
            st_ref_pic_set(&mut r, &sps.sets, sps.sets.len(), true)?
        };
        Some(Slice {
            first,
            sps_id: pps.sps_id,
            lsb: Some((at, sps.log2_max_poc_lsb, lsb)),
            rps,
        })
    }

    /// A coded frame: NAL units after `offset`.
    fn coded(&mut self, payload: &[u8], offset: usize) -> Option<Vec<u8>> {
        if !self.lengths_ok {
            return None;
        }
        // What follows the last whole NAL unit stays as it is.
        let mut units = Vec::new();
        let mut pos = offset;
        while let Some(len) = payload.get(pos..pos + 4) {
            let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
            let Some(end) = (pos + 4).checked_add(len).filter(|&e| e <= payload.len()) else {
                break;
            };
            units.push(pos + 4..end);
            pos = end;
        }
        // As FFmpeg reads it, an end of sequence before anything else in a
        // packet ends the sequence before the packet; after something, the
        // packet's.
        let mut eos = std::mem::take(&mut self.ended);
        let mut at_start = true;
        let mut rewritten: Vec<Option<Vec<u8>>> = vec![None; units.len()];
        for (unit, out) in units.iter().zip(&mut rewritten) {
            let nal = &payload[unit.clone()];
            let &[b0, b1, ..] = nal else { continue };
            let nal_type = (b0 >> 1) & 0x3f;
            if matches!(nal_type, EOS_NUT | EOB_NUT) {
                if at_start {
                    eos = true;
                } else {
                    self.ended = true;
                }
                continue;
            }
            at_start = false;
            let layer = ((b0 & 1) << 5) | (b1 >> 3);
            let tid = (b1 & 7).checked_sub(1);
            if layer != 0 || nal_type >= 32 {
                if layer == 0 {
                    self.parameter_set(nal);
                } else if nal_type < 32 {
                    self.held = None;
                }
                continue;
            }
            let Some(tid) = tid else { continue };
            let head = unescape(&nal[..nal.len().min(SLICE_HEADER_BYTES)]);
            let Some(slice) = self.slice(&head, nal_type) else {
                self.held = None;
                continue;
            };
            if slice.first {
                self.picture(nal_type, tid, &slice, eos);
            }
            if let Some((at, bits, lsb)) = slice.lsb
                && self.shift != 0
            {
                let mut rbsp = unescape(nal);
                let lsb = (lsb + self.shift) & ((1 << bits) - 1);
                put_bits(&mut rbsp, at, bits, lsb);
                *out = Some(escape(&rbsp));
            }
        }
        if rewritten.iter().all(Option::is_none) {
            return None;
        }
        let mut out = Vec::with_capacity(payload.len() + 8);
        out.extend_from_slice(&payload[..offset]);
        for (unit, new) in units.into_iter().zip(&rewritten) {
            let nal = new.as_deref().unwrap_or(&payload[unit]);
            out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            out.extend_from_slice(nal);
        }
        out.extend_from_slice(&payload[pos..]);
        Some(out)
    }

    /// The first slice of a picture (in a packet after an end of sequence,
    /// or not): how the decoder numbers it (renumbered if it starts a sequence
    /// on a number held), and what it holds after.
    fn picture(&mut self, nal_type: u8, tid: u8, s: &Slice, eos: bool) {
        let Some((version, _, sps)) = &self.sps[s.sps_id] else {
            return;
        };
        let (version, log2) = (*version, sps.log2_max_poc_lsb);
        let idr = matches!(nal_type, IDR_W_RADL | IDR_N_LP);
        let bla = (super::BLA_W_LP..=BLA_N_LP).contains(&nal_type);
        if super::is_irap(nal_type) {
            self.no_rasl_output = idr || bla || (nal_type == super::CRA && eos);
        }
        if matches!(nal_type, super::RASL_N | super::RASL_R) && self.no_rasl_output {
            // Not decoded.
            return;
        }
        // A different sequence parameter set clears what the decoder held.
        let changed = self.active != Some((s.sps_id, version));
        self.active = Some((s.sps_id, version));
        if idr || changed {
            self.held = Some(Vec::new());
        }
        if idr || bla || eos || changed {
            self.shift = 0;
            if let (Some((_, _, lsb)), Some(held)) = (s.lsb, &self.held) {
                let mask = (1u32 << log2) - 1;
                let prev = self.prev_tid0;
                self.shift = (0..=mask)
                    .find(|d| !held.contains(&compute_poc(prev, (lsb + d) & mask, log2, nal_type)))
                    .unwrap_or(0);
            }
        }
        let poc = match s.lsb {
            Some((_, _, lsb)) => compute_poc(
                self.prev_tid0,
                (lsb + self.shift) & ((1 << log2) - 1),
                log2,
                nal_type,
            ),
            None => 0,
        };
        if tid == 0
            && !matches!(
                nal_type,
                TRAIL_N | TSA_N | STSA_N | RADL_N | RADL_R | super::RASL_N | super::RASL_R
            )
        {
            self.prev_tid0 = poc;
        }
        let refs = s.rps.before.iter().chain(&s.rps.after);
        self.held = Some(
            std::iter::once(poc)
                .chain(refs.map(|&d| poc + d as i64))
                .collect(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BLA_W_LP, CRA};
    use super::*;

    const TRAIL_R: u8 = 1;

    /// Writes an RBSP.
    #[derive(Default)]
    struct W {
        b: Vec<u8>,
        n: usize,
    }

    impl W {
        fn u(&mut self, bits: u32, v: u64) -> &mut Self {
            for i in (0..bits).rev() {
                if self.n.is_multiple_of(8) {
                    self.b.push(0);
                }
                if v >> i & 1 == 1 {
                    *self.b.last_mut().unwrap() |= 0x80 >> (self.n % 8);
                }
                self.n += 1;
            }
            self
        }

        fn flag(&mut self, f: bool) -> &mut Self {
            self.u(1, f as u64)
        }

        fn ue(&mut self, v: u32) -> &mut Self {
            let x = v as u64 + 1;
            let len = 64 - x.leading_zeros();
            self.u(len - 1, 0).u(len, x)
        }

        fn se(&mut self, v: i32) -> &mut Self {
            self.ue(if v > 0 {
                2 * v as u32 - 1
            } else {
                (-2 * v) as u32
            })
        }

        /// The NAL unit of type `t` with this RBSP, given its trailing bits.
        fn nal(&mut self, t: u8, tid: u8) -> Vec<u8> {
            self.u(1, 1);
            while !self.n.is_multiple_of(8) {
                self.u(1, 0);
            }
            let mut rbsp = vec![t << 1, tid + 1];
            rbsp.extend_from_slice(&self.b);
            escape(&rbsp)
        }
    }

    /// An explicit st_ref_pic_set: differences before (negative, nearest
    /// first) and after (positive, nearest first), all used.
    fn write_rps(w: &mut W, before: &[i32], after: &[i32]) {
        w.ue(before.len() as u32).ue(after.len() as u32);
        let mut prev = 0;
        for &d in before {
            w.ue((prev - d - 1) as u32).flag(true);
            prev = d;
        }
        prev = 0;
        for &d in after {
            w.ue((d - prev - 1) as u32).flag(true);
            prev = d;
        }
    }

    /// Choices for the parameter sets of a test stream.
    #[derive(Clone)]
    struct Params {
        log2_lsb: u32,
        sub_layers: u32,
        chroma_444_planes: bool,
        scaling_lists: bool,
        pcm: bool,
        /// Sets in the SPS: writes each one after the "predicted" flag.
        sets: Vec<fn(&mut W)>,
        long_term: bool,
        dependent_slices: bool,
        output_flag: bool,
        extra_bits: u32,
        /// After what slices need.
        strong_intra_smoothing: bool,
    }

    /// Like x265: 640x360, 32x32 CTBs (240 of them), 8 bits of POC LSB.
    fn x265() -> Params {
        Params {
            log2_lsb: 8,
            sub_layers: 0,
            chroma_444_planes: false,
            scaling_lists: false,
            pcm: false,
            sets: Vec::new(),
            long_term: false,
            dependent_slices: false,
            output_flag: false,
            extra_bits: 0,
            strong_intra_smoothing: true,
        }
    }

    /// Everything that changes where a slice's order count is.
    fn unusual() -> Params {
        Params {
            log2_lsb: 6,
            sub_layers: 2,
            chroma_444_planes: true,
            scaling_lists: true,
            pcm: true,
            sets: vec![
                // 0: before -1 and -3, after 2.
                |w| write_rps(w, &[-1, -3], &[2]),
                // 1: predicted from 0 with a difference of -1: -2, -4 and
                // (by use_delta_flag) 1; not the difference itself.
                |w| {
                    w.flag(true).flag(true).ue(0);
                    w.flag(true)
                        .flag(false)
                        .flag(true)
                        .flag(true)
                        .flag(false)
                        .flag(false);
                },
            ],
            long_term: false,
            dependent_slices: true,
            output_flag: true,
            extra_bits: 2,
            strong_intra_smoothing: true,
        }
    }

    fn sps(p: &Params) -> Vec<u8> {
        let mut w = W::default();
        w.u(4, 0).u(3, p.sub_layers as u64).flag(true);
        // profile_tier_level: Main, level 3.1.
        w.u(8, 0x01)
            .u(32, 0x6000_0000)
            .u(4, 0b1001)
            .u(44, 0)
            .u(8, 93);
        for i in 0..p.sub_layers {
            w.flag(i == 0).flag(true);
        }
        if p.sub_layers > 0 {
            w.u(2 * (8 - p.sub_layers), 0);
        }
        for i in 0..p.sub_layers {
            if i == 0 {
                w.u(56, 0x01_6000_0000_9000).u(32, 0);
            }
            w.u(8, 90);
        }
        w.ue(0);
        if p.chroma_444_planes {
            w.ue(3).flag(true);
        } else {
            w.ue(1);
        }
        w.ue(640).ue(360);
        // A conformance window.
        w.flag(true).ue(0).ue(0).ue(0).ue(4);
        w.ue(0).ue(0).ue(p.log2_lsb - 4);
        w.flag(true);
        for _ in 0..=p.sub_layers {
            w.ue(4).ue(2).ue(0);
        }
        w.ue(0).ue(2).ue(0).ue(3).ue(1).ue(1);
        w.flag(p.scaling_lists);
        if p.scaling_lists {
            w.flag(true);
            for size_id in 0..4 {
                let mut m = 0;
                while m < 6 {
                    if (size_id + m) % 2 == 0 {
                        w.flag(false).ue(0);
                    } else {
                        w.flag(true);
                        if size_id > 1 {
                            w.se(8);
                        }
                        for i in 0..64.min(1 << (4 + (size_id << 1))) {
                            w.se(i % 5 - 2);
                        }
                    }
                    m += if size_id == 3 { 3 } else { 1 };
                }
            }
        }
        w.flag(false).flag(true);
        w.flag(p.pcm);
        if p.pcm {
            w.u(4, 7).u(4, 7).ue(0).ue(1).flag(false);
        }
        w.ue(p.sets.len() as u32);
        for set in &p.sets {
            set(&mut w);
        }
        w.flag(p.long_term);
        if p.long_term {
            w.ue(0);
        }
        // sps_temporal_mvp, strong intra smoothing, no VUI, no extensions.
        w.flag(true)
            .flag(p.strong_intra_smoothing)
            .flag(false)
            .flag(false);
        w.nal(SPS_NUT, 0)
    }

    fn pps(p: &Params) -> Vec<u8> {
        let mut w = W::default();
        w.ue(0)
            .ue(0)
            .flag(p.dependent_slices)
            .flag(p.output_flag)
            .u(3, p.extra_bits as u64);
        w.flag(false)
            .flag(false)
            .ue(0)
            .ue(0)
            .se(0)
            .flag(false)
            .flag(false)
            .flag(false);
        w.nal(PPS_NUT, 0)
    }

    /// Where a slice's reference pictures come from.
    #[derive(Clone, Copy)]
    enum Refs<'a> {
        Explicit(&'a [i32], &'a [i32]),
        Sps(u32),
    }

    /// A slice segment: the first of its picture or at `address`, `dependent`
    /// or not; then the slice data.
    #[allow(clippy::too_many_arguments)]
    fn slice_segment(
        p: &Params,
        t: u8,
        tid: u8,
        address: Option<u32>,
        dependent: bool,
        lsb: u32,
        refs: Refs,
    ) -> Vec<u8> {
        let mut w = W::default();
        w.flag(address.is_none());
        if (16..=23).contains(&t) {
            w.flag(false);
        }
        w.ue(0);
        if let Some(a) = address {
            if p.dependent_slices {
                w.flag(dependent);
            }
            w.u(8, a as u64);
        }
        if !dependent {
            w.u(p.extra_bits, 0b10);
            w.ue(if (16..=23).contains(&t) { 2 } else { 1 });
            if p.output_flag {
                w.flag(true);
            }
            if p.chroma_444_planes {
                w.u(2, 1);
            }
            if !matches!(t, IDR_W_RADL | IDR_N_LP) {
                w.u(p.log2_lsb, lsb as u64);
                match refs {
                    Refs::Explicit(before, after) => {
                        w.flag(false);
                        if !p.sets.is_empty() {
                            w.flag(false);
                        }
                        write_rps(&mut w, before, after);
                    }
                    Refs::Sps(i) => {
                        w.flag(true).u(ceil_log2(p.sets.len() as u64), i as u64);
                    }
                }
                w.flag(true);
            }
            w.flag(true).flag(true).se(-1);
        }
        // Slice data that needs emulation prevention, and a cabac_zero_word.
        w.u(1, 1);
        while !w.n.is_multiple_of(8) {
            w.u(1, 0);
        }
        w.b.extend_from_slice(&[0x5a, 0, 0, 1, 0, 0, 0, 0xa5, 0x80, 0, 0]);
        let mut rbsp = vec![t << 1, tid + 1];
        rbsp.extend_from_slice(&w.b);
        escape(&rbsp)
    }

    fn slice(p: &Params, t: u8, lsb: u32, refs: Refs) -> Vec<u8> {
        slice_segment(p, t, 0, None, false, lsb, refs)
    }

    /// An FLV video tag body (legacy HEVC) with these NAL units.
    fn frame(key: bool, nals: &[Vec<u8>]) -> Vec<u8> {
        let mut v = vec![if key { 0x1c } else { 0x2c }, 1, 0, 0, 0];
        for n in nals {
            v.extend_from_slice(&(n.len() as u32).to_be_bytes());
            v.extend_from_slice(n);
        }
        v
    }

    /// An FLV HEVC sequence header (enhanced) with these parameter sets.
    fn config(nals: &[Vec<u8>], length_size: u8) -> Vec<u8> {
        let mut v = vec![0x90];
        v.extend_from_slice(b"hvc1");
        let mut record = vec![
            1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 93, 0xf0, 0, 0xfc,
        ];
        record.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0, 0, 0x0c | (length_size - 1)]);
        record.push(nals.len() as u8);
        for n in nals {
            record.push(0x80 | (n[0] >> 1));
            record.extend_from_slice(&1u16.to_be_bytes());
            record.extend_from_slice(&(n.len() as u16).to_be_bytes());
            record.extend_from_slice(n);
        }
        v.extend_from_slice(&record);
        v
    }

    /// A guard that has had the parameter sets.
    fn guard(p: &Params) -> PocGuard {
        let mut g = PocGuard::new();
        assert_eq!(g.video(&config(&[sps(p), pps(p)], 4)), None);
        g
    }

    /// Sends a frame through and returns what is written.
    fn send(g: &mut PocGuard, frame: Vec<u8>) -> Vec<u8> {
        g.video(&frame).unwrap_or(frame)
    }

    /// An IDR picture, then trailing pictures 1 to `last` (each keeping the one
    /// before).
    fn start(g: &mut PocGuard, p: &Params, last: u32) {
        let idr = frame(true, &[slice(p, IDR_N_LP, 0, Refs::Explicit(&[], &[]))]);
        assert_eq!(g.video(&idr), None);
        for lsb in 1..=last {
            let f = frame(false, &[slice(p, TRAIL_R, lsb, Refs::Explicit(&[-1], &[]))]);
            assert_eq!(g.video(&f), None, "{lsb}");
        }
    }

    #[test]
    fn escaping_is_undone_exactly() {
        let rbsp = [2, 1, 0, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0];
        let nal = escape(&rbsp);
        assert_eq!(
            nal,
            [
                2, 1, 0, 0, 3, 0, 0, 3, 0, 1, 0, 0, 3, 2, 0, 0, 3, 3, 0, 0, 4, 0, 0, 3
            ]
        );
        assert_eq!(unescape(&nal), rbsp);
        assert_eq!(escape(&unescape(&nal)), nal);
    }

    #[test]
    fn a_held_keyframe_sent_again_is_renumbered_each_time() {
        let p = x265();
        let mut g = guard(&p);
        start(&mut g, &p, 20);
        // The keyframe that aired last, as a BLA: what the decoder holds
        // after it (8, and 5 and 3 in its reference set) includes its own
        // number, so the next copy takes the next free one, and the one after
        // that is free again.
        let held = |lsb| {
            frame(
                true,
                &[slice(&p, BLA_W_LP, lsb, Refs::Explicit(&[-3, -5], &[]))],
            )
        };
        assert_eq!(g.video(&held(8)), None);
        for copy in 0..5 {
            let lsb = if copy % 2 == 0 { 9 } else { 8 };
            assert_eq!(send(&mut g, held(8)), held(lsb), "copy {}", copy + 2);
        }
        // The keyframe the hold ends on has the number of the last copy (9):
        // it and the pictures of its sequence move on by one.
        let bla = frame(
            true,
            &[slice(&p, BLA_W_LP, 9, Refs::Explicit(&[-3, -5], &[]))],
        );
        let bla_next = frame(
            true,
            &[slice(&p, BLA_W_LP, 10, Refs::Explicit(&[-3, -5], &[]))],
        );
        assert_eq!(send(&mut g, bla), bla_next);
        for lsb in 10..=300u32 {
            let f = |lsb: u32| {
                frame(
                    false,
                    &[slice(&p, TRAIL_R, lsb & 255, Refs::Explicit(&[-1], &[]))],
                )
            };
            assert_eq!(send(&mut g, f(lsb)), f(lsb + 1), "{lsb}");
        }
        // As does a CRA, which carries the sequence on, with what follows its
        // last NAL unit kept as it is.
        let cra = |lsb| {
            let mut f = frame(true, &[slice(&p, CRA, lsb, Refs::Explicit(&[-1], &[]))]);
            f.extend_from_slice(&[0, 0, 1]);
            f
        };
        assert_eq!(send(&mut g, cra(45)), cra(46));
        // An IDR starts again from nothing held.
        start(&mut g, &p, 3);
        assert_eq!(g.video(&held(1)), None);
    }

    #[test]
    fn a_splice_renumbers_only_on_a_number_held() {
        let p = x265();
        let mut g = guard(&p);
        start(&mut g, &p, 20);
        // 30 and 40 are not held after picture 20 (which keeps 19).
        let bla = |lsb| frame(true, &[slice(&p, BLA_W_LP, lsb, Refs::Explicit(&[], &[]))]);
        assert_eq!(g.video(&bla(30)), None);
        assert_eq!(g.video(&bla(40)), None);
        // Picture 41 keeps 40 and 39 (not decoded, but the decoder keeps a
        // stand-in for it).
        let trail = frame(
            false,
            &[slice(&p, TRAIL_R, 41, Refs::Explicit(&[-1, -2], &[]))],
        );
        assert_eq!(g.video(&trail), None);
        assert_eq!(send(&mut g, bla(39)), bla(42));
        assert_eq!(g.video(&bla(41)), None);
    }

    #[test]
    fn numbers_wrap_around_their_bits() {
        let p = x265();
        let mut g = guard(&p);
        start(&mut g, &p, 2);
        let bla = |lsb| {
            frame(
                true,
                &[slice(&p, BLA_W_LP, lsb, Refs::Explicit(&[-1], &[]))],
            )
        };
        assert_eq!(g.video(&bla(255)), None);
        // The same again: the next number is 0, and its pictures go on from
        // there.
        assert_eq!(send(&mut g, bla(255)), bla(0));
        let trail = |lsb| {
            frame(
                false,
                &[slice(&p, TRAIL_R, lsb, Refs::Explicit(&[-1], &[]))],
            )
        };
        assert_eq!(send(&mut g, trail(0)), trail(1));
    }

    #[test]
    fn a_cra_after_an_end_of_sequence_starts_one_too() {
        let p = x265();
        let mut g = guard(&p);
        start(&mut g, &p, 20);
        // An end of sequence after picture 20, in its packet: the CRA after
        // it starts a sequence, numbered (by FFmpeg) on from 20, which is held.
        let eos = vec![EOS_NUT << 1, 1];
        let last = frame(
            false,
            &[
                slice(&p, TRAIL_R, 21, Refs::Explicit(&[-1], &[])),
                eos.clone(),
            ],
        );
        assert_eq!(g.video(&last), None);
        let cra = |lsb| frame(true, &[slice(&p, CRA, lsb, Refs::Explicit(&[], &[]))]);
        assert_eq!(send(&mut g, cra(20)), cra(22));
        // At the start of a packet, it ends the sequence before.
        assert_eq!(
            send(
                &mut g,
                frame(true, &[eos, slice(&p, CRA, 22, Refs::Explicit(&[], &[]))])
            ),
            frame(
                true,
                &[
                    vec![EOS_NUT << 1, 1],
                    slice(&p, CRA, 23, Refs::Explicit(&[], &[]))
                ]
            )
        );
        // Without one, a CRA carries the sequence on: nothing to check.
        assert_eq!(g.video(&cra(23)), Some(cra(24)));
    }

    #[test]
    fn every_slice_and_parameter_set_of_a_picture_counts() {
        let p = unusual();
        let mut g = guard(&p);
        // Pictures in three slice segments, the last dependent, after their
        // parameter sets sent again.
        let picture = |t, lsb, refs| {
            frame(
                t != TRAIL_R,
                &[
                    sps(&p),
                    pps(&p),
                    slice_segment(&p, t, 0, None, false, lsb, refs),
                    slice_segment(&p, t, 0, Some(80), false, lsb, refs),
                    slice_segment(&p, t, 0, Some(160), true, lsb, refs),
                ],
            )
        };
        start(&mut g, &p, 4);
        // With the set predicted in the SPS, 5 holds 3, 1 and 6.
        assert_eq!(g.video(&picture(TRAIL_R, 5, Refs::Sps(1))), None);
        // A splice to 6 is renumbered (to 7) in each slice that has a number.
        let bla = |lsb| picture(BLA_W_LP, lsb, Refs::Explicit(&[-2], &[]));
        assert_eq!(send(&mut g, bla(6)), bla(7));
        assert_eq!(g.video(&bla(6)), None);
    }

    #[test]
    fn a_sequence_parameter_set_that_changed_clears_what_is_held() {
        let p = x265();
        let mut g = guard(&p);
        start(&mut g, &p, 20);
        let bla = frame(true, &[slice(&p, BLA_W_LP, 20, Refs::Explicit(&[], &[]))]);
        // Sent again as it was, it changes nothing: 20 is held.
        assert_eq!(g.video(&config(&[sps(&p), pps(&p)], 4)), None);
        assert!(g.clone().video(&bla).is_some());
        // Different in a part slices do not need: FFmpeg takes it as a new
        // one, and lets go of what it held.
        let other = Params {
            strong_intra_smoothing: false,
            ..x265()
        };
        assert_eq!(g.video(&config(&[sps(&other), pps(&other)], 4)), None);
        assert_eq!(g.video(&bla), None);
    }

    #[test]
    fn values_out_of_range_are_refused() {
        // log2_max_pic_order_cnt_lsb_minus4 as large as ue(v) goes.
        let mut w = W::default();
        w.ue(1)
            .ue(640)
            .ue(360)
            .flag(false)
            .ue(0)
            .ue(0)
            .ue(u32::MAX - 1);
        w.u(64, 0);
        assert_eq!(sps_rest(&mut Bits { b: &w.b, pos: 0 }, 0), None);
        // Differences that add up past what an i32 holds.
        let mut w = W::default();
        w.ue(3).ue(0);
        for _ in 0..3 {
            w.ue(u32::MAX - 1).flag(true);
        }
        assert_eq!(
            st_ref_pic_set(&mut Bits { b: &w.b, pos: 0 }, &[], 0, true),
            None
        );
    }

    #[test]
    fn what_it_cannot_follow_is_left_alone() {
        let p = x265();
        let held = |p: &Params| frame(true, &[slice(p, BLA_W_LP, 8, Refs::Explicit(&[], &[]))]);
        let twice = |g: &mut PocGuard, p: &Params| {
            for _ in 0..2 {
                assert_eq!(g.video(&held(p)), None);
            }
        };
        // Without parameter sets.
        twice(&mut PocGuard::new(), &p);
        // With NAL unit lengths of two bytes.
        let mut g = PocGuard::new();
        assert_eq!(g.video(&config(&[sps(&p), pps(&p)], 2)), None);
        twice(&mut g, &p);
        // With long-term reference pictures.
        let long_term = Params {
            long_term: true,
            ..x265()
        };
        twice(&mut guard(&long_term), &long_term);
        // Other codecs, and what is not a whole frame.
        let mut g = guard(&p);
        assert_eq!(g.video(&held(&p)), None);
        let mut cut = held(&p);
        cut.pop();
        let avc = [0x17, 1, 0, 0, 0, 0, 0, 0, 2, 0x65, 0x88];
        for body in [&cut[..], &avc, &[0x2c], &[]] {
            assert_eq!(g.video(body), None);
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn bit(b: &[u8], i: usize) -> bool {
        (b[i / 8] >> (7 - i % 8)) & 1 == 1
    }

    /// The parameter sets of x265 3.5 (`-preset veryfast -g 60`, 640x360),
    /// and the start of its IDR slice and of its first CRA slice (picture 60).
    #[test]
    fn real_x265_pictures() {
        let vps = hex("40010c01ffff01600000030090000003000003003f959809");
        let sps = hex(
            "42010101600000030090000003000003003fa00502016965959a4932bc05a020000003002000000303c1",
        );
        let pps = hex("4401c172b46240");
        let idr =
            hex("2801af084c31854fe8a995fe3e6063fb4d7681ad304a619790cb9a615311ab52b4288c4802f26728");
        let mut bla =
            hex("2a01acf05948c9c70c31548388839f8b86a0b0ac095c515fff3c138f431b993fd6ac430c11d697a4");
        bla[0] = BLA_W_LP << 1;
        let mut g = PocGuard::new();
        assert_eq!(g.video(&config(&[vps, sps, pps], 4)), None);
        assert_eq!(g.video(&frame(true, &[idr])), None);
        let held = frame(true, &[bla.clone()]);
        assert_eq!(g.video(&held), None);
        let again = g.video(&held).expect("renumbered");
        // Only its order count changed: 60 is now 61.
        let (before, after) = (unescape(&bla), unescape(&again[9..]));
        let (at, bits, lsb) = g.slice(&before, BLA_W_LP).unwrap().lsb.unwrap();
        assert_eq!((bits, lsb), (8, 60));
        assert_eq!(g.slice(&after, BLA_W_LP).unwrap().lsb, Some((at, 8, 61)));
        assert_eq!(before.len(), after.len());
        let differ: Vec<usize> = (0..before.len() * 8)
            .filter(|&i| bit(&before, i) != bit(&after, i))
            .collect();
        assert!(
            differ.iter().all(|i| (at..at + 8).contains(i)),
            "{differ:?}"
        );
    }

    proptest::proptest! {
        #[test]
        fn escaping_round_trips(
            body in proptest::collection::vec(
                proptest::prop_oneof![proptest::strategy::Just(0u8), 0u8..4, proptest::arbitrary::any::<u8>()],
                0..48,
            ),
            zero_words in 0usize..3,
        ) {
            // An RBSP: the NAL unit header, data, a stop bit, cabac_zero_words.
            let mut rbsp = vec![TRAIL_R << 1, 1];
            rbsp.extend_from_slice(&body);
            rbsp.push(0x80);
            rbsp.extend(std::iter::repeat_n(0, 2 * zero_words));
            let nal = escape(&rbsp);
            proptest::prop_assert_eq!(&unescape(&nal), &rbsp);
            proptest::prop_assert_eq!(&escape(&unescape(&nal)), &nal);
            for w in nal.windows(3) {
                proptest::prop_assert!(w[0] != 0 || w[1] != 0 || w[2] > 2, "{nal:?}");
            }
            proptest::prop_assert_ne!(nal.last(), Some(&0));
        }

        #[test]
        fn damaged_streams_never_panic(
            damage in proptest::collection::vec(
                (0usize..16, proptest::arbitrary::any::<usize>(), 1u8..),
                0..6,
            ),
            unusual_params in proptest::arbitrary::any::<bool>(),
        ) {
            let p = if unusual_params { unusual() } else { x265() };
            let explicit = Refs::Explicit(&[-1, -3], &[2]);
            let mut bodies = vec![
                config(&[sps(&p), pps(&p)], 4),
                frame(true, &[slice(&p, IDR_W_RADL, 0, explicit)]),
                frame(false, &[slice(&p, TRAIL_R, 1, explicit)]),
                frame(true, &[slice(&p, BLA_W_LP, 1, explicit)]),
                frame(true, &[vec![EOS_NUT << 1, 1], slice(&p, CRA, 1, explicit)]),
                frame(
                    false,
                    &[
                        sps(&p),
                        pps(&p),
                        slice_segment(&p, TRAIL_R, 0, None, false, 2, explicit),
                        slice_segment(&p, TRAIL_R, 0, Some(9), true, 2, explicit),
                    ],
                ),
            ];
            if !p.sets.is_empty() {
                bodies.push(frame(false, &[slice(&p, TRAIL_R, 3, Refs::Sps(1))]));
            }
            for (which, at, xor) in damage {
                let n = bodies.len();
                let b = &mut bodies[which % n];
                let i = at % b.len();
                b[i] ^= xor;
            }
            let mut g = PocGuard::new();
            for b in bodies.iter().cycle().take(4 * bodies.len()) {
                let _ = g.video(b);
            }
        }
    }
}
