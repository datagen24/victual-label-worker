//! Brother QL raster, without Python.
//!
//! The whole CPython-override problem existed to ship `brother_ql-inventree`, and after
//! ADR-0021 moved rendering out of the worker that library was the only Python-specific thing
//! left in it. This is the part of it a QL-820NWBc actually needs: turn a 1-bit raster into the
//! command stream and put it on the wire.
//!
//! Constants are taken from `brother_ql-inventree` 1.3 rather than invented — `models.py`
//! (QL-820NWB: 90 bytes per row, `additional_offset_r` 0, 400 invalidate bytes) and `labels.py`
//! (`62`: 732 total dots, 696 printable, right margin 12, feed margin 35).
//!
//! **Issue #90 is designed out rather than fixed here.** The defect was authoring an image
//! against `dots_total` and letting the library resize it to `dots_printable`. This never
//! resizes: the renderer produces exactly `PRINTABLE_DOTS` across, and the only placement is a
//! whole-pixel offset into the device row. There is also one rotation authority, because there
//! is no rotation — the renderer already draws x across the tape and y along the feed, which is
//! the raster's own layout.

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

pub const BYTES_PER_ROW: usize = 90; // QL-820NWB
pub const DEVICE_DOTS: usize = BYTES_PER_ROW * 8; // 720
pub const PRINTABLE_DOTS: usize = 696; // label "62"
pub const RIGHT_MARGIN_DOTS: usize = 12; // label "62" + additional_offset_r 0
pub const FEED_MARGIN: u16 = 35;
const INVALIDATE_BYTES: usize = 400;

pub struct Job {
    pub two_colour: bool,
    pub cut_at_end: bool,
    pub auto_cut: bool,
    pub high_quality: bool,
    /// Set when the raster's rows are 600 per inch along the feed rather than 300. It has to
    /// agree with the resolution the artifact was rendered at, or the label comes out at twice
    /// or half its intended length — which is issue #90's failure arriving from the other end.
    pub dpi_600: bool,
    /// The tape width the *loaded roll* is, in millimetres. It has to match what is in the
    /// printer or the device refuses the job with "wrong roll type" — the declaration is a
    /// claim about the media, not a request.
    pub tape_mm: u8,
}

impl Default for Job {
    fn default() -> Self {
        Job { two_colour: false, cut_at_end: true, auto_cut: true, high_quality: true,
              dpi_600: false, tape_mm: 62 }
    }
}

/// One raster plane: `rows` rows of `BYTES_PER_ROW`, bit set = ink.
pub struct Plane(pub Vec<u8>);

/// Pack an ink mask into device rows.
///
/// `ink[y * PRINTABLE_DOTS + x]` is true where the label is inked. The bit order is reversed
/// within each byte, which is what `brother_ql`'s `FLIP_LEFT_RIGHT` before packing amounts to.
pub fn pack(ink: &[bool], width: usize, height: usize) -> Plane {
    assert_eq!(width, PRINTABLE_DOTS, "the renderer must author at dots_printable");
    let offset = DEVICE_DOTS - PRINTABLE_DOTS - RIGHT_MARGIN_DOTS; // 12
    let mut out = vec![0u8; BYTES_PER_ROW * height];
    for y in 0..height {
        for x in 0..width {
            if ink[y * width + x] {
                let device_x = x + offset;
                let mirrored = DEVICE_DOTS - 1 - device_x;
                out[y * BYTES_PER_ROW + mirrored / 8] |= 0x80 >> (mirrored % 8);
            }
        }
    }
    Plane(out)
}

pub fn build(black: &Plane, red: Option<&Plane>, rows: usize, job: &Job) -> Vec<u8> {
    let mut d: Vec<u8> = Vec::new();

    // `ESC i a 1` goes **before** the invalidate as well as after the initialize, which is what
    // `brother_ql` does and what this spike's first physical attempt did not.
    //
    // The reason is device state rather than protocol taste: a QL-820NWBc configured with
    // "P-touch Template" emulation reads the incoming stream as template commands, so a raster
    // job that only switches mode *after* the invalidate has already been misread by then — the
    // printer answered "Wrong Roll Type / Check Print Data" while holding exactly the 62 mm
    // continuous tape the job declared. The emulation mode is observed device state that a job
    // has to assert over, not a setting the operator should have to change first.
    d.extend_from_slice(b"\x1B\x69\x61\x01"); // ESC i a 1   raster mode
    d.extend(std::iter::repeat(0u8).take(INVALIDATE_BYTES)); // clear the command buffer
    d.extend_from_slice(b"\x1B\x40"); // ESC @   initialize
    d.extend_from_slice(b"\x1B\x69\x61\x01"); // ESC i a 1   raster mode, again
    d.extend_from_slice(b"\x1B\x69\x53"); // ESC i S   status information request

    // ESC i z — media and quality. Flags say which of the following fields are meaningful.
    d.extend_from_slice(b"\x1B\x69\x7A");
    let mut flags: u8 = 0x80;
    flags |= 1 << 1; // media type
    flags |= 1 << 2; // media width
    flags |= 1 << 3; // media length
    if job.high_quality { flags |= 1 << 6; }
    d.push(flags);
    d.push(0x0A); // endless label
    d.push(job.tape_mm); // tape width, mm
    d.push(0); // length, 0 for endless
    d.extend_from_slice(&(rows as u32).to_le_bytes());
    d.push(0); // first page
    d.push(0);

    if job.auto_cut {
        d.extend_from_slice(b"\x1B\x69\x4D"); // ESC i M
        d.push(1 << 6);
        d.extend_from_slice(b"\x1B\x69\x41"); // ESC i A   cut every n
        d.push(1);
    }

    // ESC i K — expanded mode: two colour in bit 0, cut-at-end in bit 3, 600 dpi in bit 6.
    d.extend_from_slice(b"\x1B\x69\x4B");
    let mut expanded: u8 = 0;
    if job.two_colour { expanded |= 1 << 0; }
    if job.cut_at_end { expanded |= 1 << 3; }
    if job.dpi_600 { expanded |= 1 << 6; }
    d.push(expanded);

    d.extend_from_slice(b"\x1B\x69\x64"); // ESC i d   feed margin
    d.extend_from_slice(&FEED_MARGIN.to_le_bytes());

    d.extend_from_slice(b"\x4D\x00"); // M 0   compression off: the rows go as they are

    for y in 0..rows {
        let from = y * BYTES_PER_ROW;
        let row = &black.0[from..from + BYTES_PER_ROW];
        if let Some(r) = red {
            d.extend_from_slice(b"\x77\x01"); // w 1   black plane
            d.push(BYTES_PER_ROW as u8);
            d.extend_from_slice(row);
            d.extend_from_slice(b"\x77\x02"); // w 2   red plane
            d.push(BYTES_PER_ROW as u8);
            d.extend_from_slice(&r.0[from..from + BYTES_PER_ROW]);
        } else {
            d.extend_from_slice(b"\x67\x00"); // g 0   single plane
            d.push(BYTES_PER_ROW as u8);
            d.extend_from_slice(row);
        }
    }

    d.push(0x1A); // print with feed, last page
    d
}

pub fn send(host: &str, data: &[u8]) -> std::io::Result<usize> {
    let mut s = TcpStream::connect(host)?;
    s.set_write_timeout(Some(Duration::from_secs(20)))?;
    s.write_all(data)?;
    s.flush()?;
    Ok(data.len())
}
