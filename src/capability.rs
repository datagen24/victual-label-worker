//! The embedded capability document, read as the worker's own geometry.
//!
//! Victual derives a profile's `raster_width_px` from a combination's `printable_width_um` and
//! its `resolution_x`, rounding to nearest (`MediaProfileService::Pixels()`). The worker reads
//! the same entry of the same document and applies the same rule, so the two cannot disagree
//! about how wide a medium prints: there is one number, and it lives in
//! `capabilities/brother-ql.json`.

use std::sync::OnceLock;

use serde_json::Value;

/// The capability document this build registers.
pub fn document() -> &'static Value {
    static DOCUMENT: OnceLock<Value> = OnceLock::new();
    DOCUMENT.get_or_init(|| {
        serde_json::from_str(include_str!("../capabilities/brother-ql.json"))
            .expect("the embedded capability document parses")
    })
}

/// Micrometres to dots, by the profile's stated rounding: nearest.
pub fn dots(micrometres: u64, dpi: u64) -> usize {
    ((micrometres * dpi) as f64 / 25400.0).round() as usize
}

/// The printable dots across the tape for the printer's resolved combination.
///
/// A printer whose `(model, connection type, media, resolution, colour mode)` is not an entry
/// of this document is refused here, before any byte is assembled.
pub fn printable_dots(printer: &Value) -> Result<usize, String> {
    let settings = &printer["settings"];
    let wanted = [
        ("model", &printer["model"]),
        ("connection_type", &printer["connection_type"]),
        ("media", &settings["media"]),
        ("resolution_x", &settings["resolution_x"]),
        ("resolution_y", &settings["resolution_y"]),
        ("color_mode", &settings["color_mode"]),
    ];
    let combinations = document()["capability_document"]["combinations"].as_array().map(Vec::as_slice).unwrap_or_default();
    let found = combinations.iter().find(|entry| wanted.iter().all(|(key, value)| &entry[*key] == *value));
    let Some(entry) = found else {
        let described: Vec<String> = wanted.iter().map(|(key, value)| format!("{key}={value}")).collect();
        return Err(format!("this worker's capability document has no combination {}", described.join(", ")));
    };
    let width = entry["printable_width_um"].as_u64().ok_or("the combination states no printable_width_um")?;
    let dpi = entry["resolution_x"].as_u64().ok_or("the combination states no resolution_x")?;
    Ok(dots(width, dpi))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn printer(media: &str, color_mode: &str) -> Value {
        serde_json::json!({"model": "QL-820NWBc", "connection_type": "tcp",
            "settings": {"media": media, "resolution_x": 300, "resolution_y": 300, "color_mode": color_mode}})
    }

    /// The maintainer's measurement, 2026-10-24: DK-22251 on a QL-820NWB keeps a fixed 1.78 mm
    /// either side, so 62 mm of tape prints 58.42 mm. At 300 dpi that is 689.99 dots, and
    /// nearest rounding - Victual's rule - makes it 690.
    #[test]
    fn red_tape_prints_690_dots_across() {
        assert_eq!(printable_dots(&printer("62red", "black_red")).unwrap(), 690);
    }

    /// Brother's Raster Command Reference (QL-800/810W/820NWB, v1.01, section 2.3.2) gives
    /// 62 mm continuous tape a 58.9 mm print area, 696 dots. Nobody has measured DK-22205
    /// otherwise, so it stays at Brother's figure.
    #[test]
    fn plain_62_still_prints_696_dots_across() {
        assert_eq!(printable_dots(&printer("62", "monochrome")).unwrap(), 696);
    }

    #[test]
    fn a_combination_the_document_does_not_have_is_refused() {
        let refusal = printable_dots(&printer("62", "black_red")).expect_err("62 is monochrome only");
        assert!(refusal.contains("no combination"), "{refusal}");
    }

    /// Every combination's band fits the head with an even split, so centring is exact.
    #[test]
    fn every_band_centres_on_the_head() {
        for entry in document()["capability_document"]["combinations"].as_array().unwrap() {
            let printable = dots(entry["printable_width_um"].as_u64().unwrap(), entry["resolution_x"].as_u64().unwrap());
            assert!(printable <= crate::ql::DEVICE_DOTS, "{entry}");
            assert!((crate::ql::DEVICE_DOTS - printable).is_multiple_of(2), "{entry}");
        }
    }
}
