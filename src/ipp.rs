//! IPP/1.1, enough of it to print and to learn whether the device thinks it did.
//!
//! This transport exists for the second half of that sentence. Over raw port 9100 a QL-820NWBc
//! answers no status request, so `sent` is the most an honest worker can report; over IPP the
//! same device returns `job-state` and `job-impressions-completed`, which is the difference
//! between "the bytes left here" and "the printer says it finished". ADR-0019 scopes
//! `completion_evidence` to the `(driver, connection_type, combination)` triple for exactly
//! this reason - the same driver is `transport` over one connection type and `device_reported`
//! over the other.
//!
//! The document is sent as `application/octet-stream`, which the device treats as a
//! passthrough: the identical two-colour raster printed on 2026-09-07 and reached
//! `job-state = completed`, although the device's own IPP attributes advertise `monochrome`
//! at 300 dpi. What IPP declares describes its own driver path rather than what the hardware
//! does when handed native commands, which is why the capability document records demonstrated
//! paths rather than advertised ones.

const VERSION: [u8; 2] = [0x01, 0x01];
const PRINT_JOB: u16 = 0x0002;
const GET_JOB_ATTRIBUTES: u16 = 0x0009;

const TAG_OPERATION: u8 = 0x01;
const TAG_JOB: u8 = 0x02;
const TAG_END: u8 = 0x03;
const TAG_INTEGER: u8 = 0x21;
const TAG_ENUM: u8 = 0x23;
const TAG_URI: u8 = 0x45;
const TAG_KEYWORD: u8 = 0x44;
const TAG_NAME: u8 = 0x42;
const TAG_CHARSET: u8 = 0x47;
const TAG_LANGUAGE: u8 = 0x48;
const TAG_MIME: u8 = 0x49;

pub struct Outcome {
    pub job_id: Option<i32>,
    pub state: Option<i32>,
    pub impressions: Option<i32>,
}

impl Outcome {
    /// IPP job states: 9 completed, 7 canceled, 8 aborted.
    pub fn completed(&self) -> bool {
        self.state == Some(9)
    }
    pub fn terminal(&self) -> bool {
        matches!(self.state, Some(7) | Some(8) | Some(9))
    }
}

fn attribute(out: &mut Vec<u8>, tag: u8, name: &str, value: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
}

fn envelope(operation: u16, request_id: u32, uri: &str, extra: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&VERSION);
    out.extend_from_slice(&operation.to_be_bytes());
    out.extend_from_slice(&request_id.to_be_bytes());
    out.push(TAG_OPERATION);
    attribute(&mut out, TAG_CHARSET, "attributes-charset", b"utf-8");
    attribute(&mut out, TAG_LANGUAGE, "attributes-natural-language", b"en");
    attribute(&mut out, TAG_URI, "printer-uri", uri.as_bytes());
    extra(&mut out);
    out.push(TAG_END);
    out
}

/// Submits a document and returns the job id the printer assigned.
pub fn print_job(endpoint: &str, document: &[u8], job_name: &str, two_colour: bool) -> Result<Outcome, String> {
    let (url, uri) = endpoints(endpoint);
    let mut body = envelope(PRINT_JOB, 1, &uri, |out| {
        attribute(out, TAG_NAME, "requesting-user-name", b"victual");
        attribute(out, TAG_NAME, "job-name", job_name.as_bytes());
        attribute(out, TAG_MIME, "document-format", b"application/octet-stream");
        out.push(TAG_JOB);
        attribute(out, TAG_KEYWORD, "print-color-mode", if two_colour { b"color" } else { b"monochrome" });
    });
    body.extend_from_slice(document);

    let response = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .post(&url)
        .set("Content-Type", "application/ipp")
        .send_bytes(&body)
        .map_err(|e| format!("IPP Print-Job to {url}: {e}"))?;

    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes)
        .map_err(|e| format!("reading the IPP response: {e}"))?;
    Ok(parse(&bytes))
}

/// Asks what the printer thinks became of the job.
pub fn job_attributes(endpoint: &str, job_id: i32) -> Result<Outcome, String> {
    let (url, uri) = endpoints(endpoint);
    let body = envelope(GET_JOB_ATTRIBUTES, 2, &uri, |out| {
        attribute(out, TAG_INTEGER, "job-id", &job_id.to_be_bytes());
        attribute(out, TAG_NAME, "requesting-user-name", b"victual");
    });

    let response = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .post(&url)
        .set("Content-Type", "application/ipp")
        .send_bytes(&body)
        .map_err(|e| format!("IPP Get-Job-Attributes to {url}: {e}"))?;

    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes)
        .map_err(|e| format!("reading the IPP response: {e}"))?;
    Ok(parse(&bytes))
}

/// `host[:port][/path]` becomes the HTTP URL to POST to and the `ipp://` URI to name inside.
fn endpoints(endpoint: &str) -> (String, String) {
    let trimmed = endpoint.trim_start_matches("ipp://").trim_start_matches("http://");
    let (authority, path) = match trimmed.find('/') {
        Some(index) => (&trimmed[..index], &trimmed[index..]),
        None => (trimmed, "/ipp/print"),
    };
    let authority = if authority.contains(':') { authority.to_string() } else { format!("{authority}:631") };
    (format!("http://{authority}{path}"), format!("ipp://{authority}{path}"))
}

/// Reads job-id, job-state and job-impressions-completed out of a response.
///
/// Deliberately not a general IPP parser. Everything else in a response is something this
/// worker has no use for, and a parser that understood more would be more to be wrong about.
fn parse(bytes: &[u8]) -> Outcome {
    let mut outcome = Outcome { job_id: None, state: None, impressions: None };
    if bytes.len() < 9 {
        return outcome;
    }
    let mut offset = 8usize;
    let mut name = String::new();

    while offset < bytes.len() {
        let tag = bytes[offset];
        offset += 1;
        if tag == TAG_END {
            break;
        }
        if tag <= 0x0f {
            // A delimiter starts a new attribute group and carries no value of its own.
            continue;
        }
        if offset + 2 > bytes.len() {
            break;
        }
        let name_length = u16::from_be_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        if offset + name_length > bytes.len() {
            break;
        }
        if name_length > 0 {
            name = String::from_utf8_lossy(&bytes[offset..offset + name_length]).to_string();
        }
        offset += name_length;

        if offset + 2 > bytes.len() {
            break;
        }
        let value_length = u16::from_be_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        if offset + value_length > bytes.len() {
            break;
        }
        let value = &bytes[offset..offset + value_length];
        offset += value_length;

        if (tag == TAG_INTEGER || tag == TAG_ENUM) && value_length == 4 {
            let number = i32::from_be_bytes(value.try_into().unwrap());
            match name.as_str() {
                "job-id" => outcome.job_id = Some(number),
                "job-state" => outcome.state = Some(number),
                "job-impressions-completed" => outcome.impressions = Some(number),
                _ => {}
            }
        }
    }
    outcome
}
