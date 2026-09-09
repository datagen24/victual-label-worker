//! Claim, verify, encode, send, report.
//!
//! The order is the contract. Verification happens **after** the artifact is in hand and
//! **before** any byte reaches a socket, because the one assertion this repository owes is
//! that an unsupported combination results in zero bytes reaching the device - counted rather
//! than assumed. Victual already refused impossible jobs where the person asking could see it;
//! media and driver versions can change between then and now, which is why the check is here
//! as well rather than instead.

mod api;
mod artifact;
mod ipp;
mod ql;

use serde_json::Value;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const BLACK: [u8; 3] = [0x00, 0x00, 0x00];
const RED: [u8; 3] = [0xff, 0x00, 0x00];

fn main() {
    let mut base = String::new();
    let mut key_file = String::new();
    let mut key = std::env::var("VICTUAL_WORKER_KEY").unwrap_or_default();
    let mut health = false;
    let mut once = false;
    let mut dry_run = false;

    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--health" => health = true,
            "--once" => once = true,
            // Assembles and verifies without opening a socket. It exists so the zero-bytes
            // assertion can be made without a printer, and so a deployment can be checked
            // before it is pointed at one.
            "--dry-run" => dry_run = true,
            "--api" => base = args.next().unwrap_or_default(),
            "--key-file" => key_file = args.next().unwrap_or_default(),
            "--version" => {
                println!("victual-label-worker {VERSION}");
                return;
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }

    if health {
        println!("{}", serde_json::json!({"ok": true, "component": "victual-label-worker", "version": VERSION}));
        return;
    }

    if !key_file.is_empty() {
        match std::fs::read_to_string(&key_file) {
            Ok(contents) => key = contents.trim().to_string(),
            Err(error) => {
                eprintln!("cannot read {key_file}: {error}");
                std::process::exit(2);
            }
        }
    }
    if base.is_empty() || key.is_empty() {
        eprintln!("usage: victual-label-worker --api <base url> --key-file <path> [--once] [--dry-run]");
        std::process::exit(2);
    }

    let api = api::Api::new(&base, &key, 30);

    // Registration is idempotent and append-only, and it happens every start rather than
    // once at install: a definition Victual already holds is accepted unchanged, and one that
    // would contradict a stored definition is refused - which is how a worker that shipped a
    // changed capability document finds out here rather than at a device.
    let capabilities: Value = serde_json::from_str(include_str!("../capabilities/brother-ql.json"))
        .expect("the embedded capability document parses");
    if let Err(error) = api.register(serde_json::json!({
        "drivers": [capabilities],
    })) {
        eprintln!("register: {error}");
        std::process::exit(1);
    }

    let mut printed = 0u32;

    loop {
        let jobs = match api.claim(1) {
            Ok(jobs) => jobs,
            Err(error) => {
                eprintln!("claim: {error}");
                std::process::exit(1);
            }
        };
        if jobs.is_empty() {
            break;
        }
        for job in jobs {
            deliver(&api, &job, dry_run);
            printed += 1;
        }
        if once {
            break;
        }
    }

    println!("{}", serde_json::json!({"ok": true, "attempts": printed}));
}

fn deliver(api: &api::Api, job: &Value, dry_run: bool) {
    let attempt = job["attempt"]["id"].as_u64().unwrap_or_default();
    let printer = &job["printer"];
    let payload = &job["payload"];
    let manifest = &job["artifact"];

    // Everything below reports through `fail`, which posts a result and returns. A failure is
    // reported and ends there: no automatic redispatch after a claimed attempt, and a person
    // authorizes another naming the one they reviewed.
    let fail = |detail: String| {
        let _ = api.result(attempt, "failed", serde_json::json!({"error": detail}));
    };

    let artifact_id = payload["artifact"]["id"].as_u64().unwrap_or_default();
    let bytes = match api.artifact_bytes(artifact_id) {
        Ok(bytes) => bytes,
        Err(error) => return fail(format!("fetching the artifact: {error}")),
    };

    let raster = match artifact::decode(&bytes) {
        Ok(raster) => raster,
        Err(error) => return fail(format!("decoding the artifact: {error}")),
    };

    if let Err(detail) = verify(&raster, manifest, printer) {
        // Zero bytes have reached the device at this point and none will: the socket is not
        // opened until after this returns.
        return fail(detail);
    }

    // Two-colour is a property of the **loaded roll**, not of whether this particular label
    // happens to paint any red. A QL with 62red tape expects the two-plane stream; handing it
    // a single-plane one because today's label is black-only is a media mismatch, and a media
    // mismatch is what "Wrong Roll Type" means. So the resolved combination decides, and a
    // black-only label on red tape sends an empty red plane.
    let two_colour = printer["settings"]["color_mode"].as_str() == Some("black_red");
    let connection = printer["connection"].as_str().unwrap_or_default().to_string();
    let connection_type = printer["connection_type"].as_str().unwrap_or("tcp");

    let stream = match encode(&raster, printer, two_colour) {
        Ok(stream) => stream,
        Err(detail) => return fail(detail),
    };

    if dry_run {
        // A dry run has consumed an authorization and sent nothing, so it says so rather than
        // leaving a job that looks attempted. Reporting the attempt as failed is the honest
        // shape: no bytes reached the device, and a person authorizes the real one.
        println!("{}", serde_json::json!({
            "dry_run": true, "attempt_id": attempt, "byte_length": stream.len(),
            "two_colour": two_colour, "rows": raster.height, "connection_type": connection_type}));
        let _ = api.result(attempt, "failed", serde_json::json!({
            "error": "Dry run: the job was assembled and verified, and no bytes were sent"}));
        return;
    }

    // The lease is renewable while the attempt runs, and device I/O is where it can take
    // long enough to matter. The heartbeat stops when this returns, and it can never revive
    // an attempt that has ended - Victual refuses a heartbeat for one that is not current.
    let beating = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let ticker = {
        let beating = beating.clone();
        let base = api.base_url();
        let key = api.credential();
        std::thread::spawn(move || {
            let heart = api::Api::new(&base, &key, 10);
            while beating.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(15));
                if !beating.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let _ = heart.heartbeat(attempt);
            }
        })
    };
    let stop = || {
        beating.store(false, std::sync::atomic::Ordering::Relaxed);
    };

    match connection_type {
        "ipp" => {
            let job_name = format!("victual-{}", payload["label_uid"].as_str().unwrap_or("label"));
            match ipp::print_job(&connection, &stream, &job_name, two_colour) {
                Ok(outcome) => {
                    let _ = api.sent(attempt, stream.len());
                    // IPP is the path that can say more than "the bytes left here", so it
                    // does: the job is polled to a terminal state and the device's own
                    // account is what the result reports.
                    let final_state = poll(&connection, outcome);
                    if final_state.completed() {
                        let _ = api.result(attempt, "printed", serde_json::json!({
                            "job_state": final_state.state, "impressions": final_state.impressions,
                            "evidence": "device_reported"}));
                    } else {
                        let _ = api.result(attempt, "failed", serde_json::json!({
                            "error": format!("the printer reported job-state {:?}", final_state.state),
                            "job_state": final_state.state}));
                    }
                }
                Err(error) => fail(error),
            }
        }
        _ => match ql::send(&connection, &stream) {
            Ok(written) => {
                // Raw 9100 answers no status request on this device, so `sent` is the whole
                // of what can honestly be reported. Victual acknowledges on send for a
                // combination whose completion_evidence is `transport`, and the record says
                // which of the two happened.
                let _ = api.sent(attempt, written);
                let _ = api.result(attempt, "printed", serde_json::json!({
                    "byte_length": written, "evidence": "transport"}));
            }
            Err(error) => fail(format!("writing to {connection}: {error}")),
        },
    }

    stop();
    let _ = ticker.join();

    // Observed status, written only by the assigned worker and never read as configuration.
    // There is no `online` boolean: status goes stale rather than false, which is why this
    // reports what was last seen rather than whether the device is up.
    if let Some(printer_id) = printer["id"].as_u64() {
        let _ = api.status(printer_id, serde_json::json!({
            "last_seen_at": null,
            "reported_media": printer["settings"]["media"],
            "device_state": "reachable",
            "raw_report": {"transport": connection_type, "reported_by": "victual-label-worker", "version": VERSION}
        }));
    }
}

/// Polls an IPP job to a terminal state, bounded.
fn poll(endpoint: &str, first: ipp::Outcome) -> ipp::Outcome {
    let Some(job_id) = first.job_id else { return first };
    let mut latest = first;
    for _ in 0..30 {
        if latest.terminal() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        match ipp::job_attributes(endpoint, job_id) {
            Ok(next) => latest = ipp::Outcome { job_id: Some(job_id), ..next },
            Err(_) => break,
        }
    }
    latest
}

/// The check that owes the zero-bytes assertion.
///
/// Three things, and each has a failure it is there for. The **combination** is revalidated
/// because media and driver versions change between enqueue and now. The **grid** is checked
/// because `fixed_grid` means a mismatch is a refusal rather than a resize, which is issue #90
/// arriving from the other end. The **colour** is checked because a two-colour raster on
/// monochrome media prints something, and what it prints is not the label.
fn verify(raster: &artifact::Raster, manifest: &Value, printer: &Value) -> Result<(), String> {
    let expected_width = manifest["width_px"].as_u64().unwrap_or_default() as u32;
    if raster.width != expected_width {
        return Err(format!("the artifact is {} px wide and its manifest says {expected_width}", raster.width));
    }
    if raster.width as usize != ql::PRINTABLE_DOTS {
        return Err(format!("this device prints {} dots across and the artifact is {}; raster/png-indexed;v=1 is fixed_grid, so this is a refusal rather than a resize", ql::PRINTABLE_DOTS, raster.width));
    }
    if manifest["form"].as_str() != Some("raster/png-indexed;v=1") {
        return Err(format!("unsupported artifact form {:?}", manifest["form"].as_str()));
    }

    let colour_mode = manifest["color_mode"].as_str().unwrap_or("monochrome");
    if raster.has(RED) && colour_mode != "black_red" {
        return Err("the artifact paints red and the resolved combination is monochrome".into());
    }

    let settings = &printer["settings"];
    let media = settings["media"].as_str().unwrap_or_default();
    if raster.has(RED) && media != "62red" {
        return Err(format!("the artifact paints red and the loaded media is {media}"));
    }

    let dpi_y = manifest["dpi_y"].as_u64().unwrap_or(300);
    if raster.has(RED) && dpi_y != 300 {
        // Demonstrated on hardware 2026-09-07: a QL-820NWBc refused two colour at 600 dpi
        // with "Communications Command Error", while the same content at 300 dpi printed.
        // The capability document already said that combination does not exist; the job
        // simply never consulted it.
        return Err(format!("two-colour printing is not available at {dpi_y} dpi on this device"));
    }
    Ok(())
}

fn encode(raster: &artifact::Raster, printer: &Value, two_colour: bool) -> Result<Vec<u8>, String> {
    let black = raster.plane(BLACK).ok_or("the artifact palette carries no black")?;
    let black_plane = ql::pack(&black, raster.width as usize, raster.height as usize);
    let red_plane = if two_colour {
        Some(ql::pack(&raster.plane(RED).unwrap_or_default(), raster.width as usize, raster.height as usize))
    } else {
        None
    };

    let settings = &printer["settings"];
    let job = ql::Job {
        two_colour,
        cut_at_end: settings["cut_at_end"].as_bool().unwrap_or(true),
        auto_cut: settings["auto_cut"].as_bool().unwrap_or(true),
        high_quality: true,
        dpi_600: settings["resolution_y"].as_u64().unwrap_or(300) == 600,
        tape_mm: settings["tape_mm"].as_u64().unwrap_or(62) as u8,
    };

    Ok(ql::build(&black_plane, red_plane.as_ref(), raster.height as usize, &job))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conforming artifact at the device's printable width.
    fn raster(width: u32, height: u32, red: bool) -> artifact::Raster {
        let palette = if red { vec![[0xff; 3], BLACK, RED] } else { vec![[0xff; 3], BLACK] };
        let mut indices = vec![0u8; (width * height) as usize];
        indices[0] = 1;
        if red {
            indices[1] = 2;
        }
        artifact::Raster { width, height, indices, palette }
    }

    fn manifest(width: u32, dpi_y: u64, colour_mode: &str) -> Value {
        serde_json::json!({"form": "raster/png-indexed;v=1", "width_px": width,
            "dpi_x": 300, "dpi_y": dpi_y, "color_mode": colour_mode})
    }

    fn printer(media: &str) -> Value {
        serde_json::json!({"id": 1, "connection": "127.0.0.1:9100", "connection_type": "tcp",
            "model": "QL-820NWBc",
            "settings": {"media": media, "resolution_x": 300, "resolution_y": 300, "color_mode": "black_red"}})
    }

    /// The assertion this repository owes: an unsupported combination sends nothing.
    ///
    /// Written as "verify refuses and encode is never reached" rather than as a byte count on
    /// a socket, because the socket is opened after this point and there is nothing to count.
    /// That is the stronger shape - a count of zero on a socket that was opened would still
    /// leave the connection made.
    #[test]
    fn two_colour_at_600_dpi_sends_zero_bytes() {
        let art = raster(ql::PRINTABLE_DOTS as u32, 100, true);
        let refusal = verify(&art, &manifest(ql::PRINTABLE_DOTS as u32, 600, "black_red"), &printer("62red"))
            .expect_err("two colour at 600 dpi does not exist on this device");
        assert!(refusal.contains("600 dpi"), "{refusal}");
    }

    #[test]
    fn red_on_monochrome_media_sends_zero_bytes() {
        let art = raster(ql::PRINTABLE_DOTS as u32, 100, true);
        let refusal = verify(&art, &manifest(ql::PRINTABLE_DOTS as u32, 300, "black_red"), &printer("62"))
            .expect_err("red on 62 is not a combination");
        assert!(refusal.contains("loaded media is 62"), "{refusal}");
    }

    /// Issue #90 from the delivery end: a grid that is not the device's is a refusal.
    ///
    /// The prototype's defect was authoring against one dot count and letting the library
    /// resize to another. There is no resize here to disable - there is a comparison, and it
    /// fails closed.
    #[test]
    fn a_grid_that_is_not_the_devices_is_refused_rather_than_resized() {
        let art = raster(900, 100, false);
        let refusal = verify(&art, &manifest(900, 300, "monochrome"), &printer("62"))
            .expect_err("900 dots across is not this device");
        assert!(refusal.contains("fixed_grid"), "{refusal}");
    }

    #[test]
    fn a_conforming_monochrome_job_assembles() {
        let art = raster(ql::PRINTABLE_DOTS as u32, 100, false);
        verify(&art, &manifest(ql::PRINTABLE_DOTS as u32, 300, "monochrome"), &printer("62")).unwrap();
        let stream = encode(&art, &printer("62"), false).unwrap();

        // ESC i a 01 comes before the invalidate as well as after the initialize. A QL
        // configured for P-touch Template emulation reads the stream as template commands
        // otherwise, and answers "Wrong Roll Type" while holding exactly the tape the job
        // declared - which is how that was misdiagnosed as a media problem on 2026-09-07.
        assert_eq!(&stream[0..4], b"\x1B\x69\x61\x01");
        assert!(stream.windows(2).any(|w| w == b"\x4D\x00"), "compression off");
        assert_eq!(*stream.last().unwrap(), 0x1A, "print with feed");
    }

    /// The raster's own layout is the device's: x across the tape, y along the feed.
    ///
    /// So there is one rotation authority because there is no rotation, and the only
    /// placement is a whole-pixel offset into the device row. The prototype passed an image
    /// and a rotation flag to a library that had its own opinion about both.
    #[test]
    fn packing_offsets_without_rotating_or_scaling() {
        let width = ql::PRINTABLE_DOTS;
        let mut ink = vec![false; width * 2];
        ink[0] = true;
        let packed = ql::pack(&ink, width, 2);

        assert_eq!(packed.0.len(), ql::BYTES_PER_ROW * 2, "one device row per raster row, unscaled");
        let set: Vec<usize> = packed.0.iter().enumerate().filter(|(_, b)| **b != 0).map(|(i, _)| i).collect();
        assert_eq!(set.len(), 1, "one inked pixel becomes one inked byte");
        // x = 0 lands at device dot 12 (720 - 696 - 12), mirrored to 707, byte 88.
        assert_eq!(set[0], 88);
    }
}
