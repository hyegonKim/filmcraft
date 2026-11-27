//! Frame.io (`FFC_framemob`) JSON and CSV review comment + Telestrator annotation interchange.

pub use filmcraft_project::{FrameIoReviewComment, ReviewStatus, TelestratorAnnotation, TelestratorShape, replies_to, root_comments};
use filmcraft_time::{FrameRate, format_timecode_frames, parse_timecode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

const MAX_REVIEW_BYTES: usize = 16 * 1024 * 1024;
const MAX_REVIEW_COMMENTS: usize = 50_000;
const CSV_HEADER: &str = "ID,Parent ID,Author,Frame,Timecode,Duration Frames,Status,Comment,Annotations";

/// Review interchange wire format (`json` or `csv`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameIoFormat {
    Json,
    Csv,
}

impl FrameIoFormat {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().trim_start_matches('.').to_ascii_lowercase().as_str() {
            "json" | "frameio" => Some(FrameIoFormat::Json),
            "csv" => Some(FrameIoFormat::Csv),
            _ => None,
        }
    }

    pub fn detect(data: &str, path_hint: Option<&str>) -> Self {
        if let Some(p) = path_hint
            && let Some((_, ext)) = p.rsplit_once('.')
            && let Some(fmt) = Self::parse(ext)
        {
            return fmt;
        }
        let trimmed = data.trim_start_matches('\u{feff}').trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') { FrameIoFormat::Json } else { FrameIoFormat::Csv }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FrameIoFormat::Json => "json",
            FrameIoFormat::Csv => "csv",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameIoRateMeta {
    num: i64,
    den: i64,
    drop_frame: bool,
    #[serde(default)]
    start_timecode: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameIoEnvelope {
    schema: String,
    asset: String,
    frame_rate: FrameIoRateMeta,
    comments: Vec<FrameIoReviewComment>,
}

/// Format a sequence-relative `frame` as SMPTE timecode using `start_tc_frames` and `drop_frame`.
pub fn frame_to_timecode(frame: i64, rate: FrameRate, drop_frame: bool, start_tc_frames: i64) -> String {
    let total = frame.max(0).saturating_add(start_tc_frames.max(0));
    format_timecode_frames(total, rate, drop_frame)
}

/// Convert SMPTE timecode into sequence-relative frames (`0`-based from `start_tc_frames`).
pub fn timecode_to_frame(tc: &str, rate: FrameRate, start_tc_frames: i64) -> Option<i64> {
    let trimmed = tc.trim();
    let abs_frame = parse_timecode(trimmed, rate, trimmed.contains(';'), 0).ok()?;
    Some((abs_frame - start_tc_frames).max(0))
}

/// Export Frame.io review comments (including replies and Telestrator annotations) as pretty JSON.
pub fn export_frameio_json(comments: &[FrameIoReviewComment], asset_name: &str, rate: FrameRate, drop_frame: bool, start_tc_frames: i64) -> Result<String> {
    if comments.len() > MAX_REVIEW_COMMENTS {
        return Err(Error::Other(format!("too many review comments (maximum {MAX_REVIEW_COMMENTS})")));
    }
    let mut normalized = Vec::with_capacity(comments.len());
    for c in comments {
        c.validate().map_err(Error::Other)?;
        let mut item = c.clone();
        if item.timecode.trim().is_empty() {
            item.timecode = frame_to_timecode(item.frame, rate, drop_frame, start_tc_frames);
        }
        normalized.push(item);
    }
    let env = FrameIoEnvelope {
        schema: "frameio.review.v1".into(),
        asset: asset_name.into(),
        frame_rate: FrameIoRateMeta { num: rate.num, den: rate.den, drop_frame, start_timecode: start_tc_frames },
        comments: normalized,
    };
    let out = serde_json::to_string_pretty(&env).map_err(|e| Error::Other(format!("JSON serialization failed: {e}")))?;
    if out.len() > MAX_REVIEW_BYTES {
        return Err(Error::Other("Frame.io JSON export exceeds 16 MiB limit".into()));
    }
    Ok(out)
}

fn parse_external_comment(v: &Value, index: usize, rate: FrameRate, drop_frame: bool, start_tc_frames: i64) -> Result<FrameIoReviewComment> {
    if let Ok(mut direct) = serde_json::from_value::<FrameIoReviewComment>(v.clone()) {
        if direct.timecode.trim().is_empty() {
            direct.timecode = frame_to_timecode(direct.frame, rate, drop_frame, start_tc_frames);
        } else if v.get("frame").is_none()
            && let Some(parsed_f) = timecode_to_frame(&direct.timecode, rate, start_tc_frames)
        {
            direct.frame = parsed_f;
        }
        if direct.author.trim().is_empty() {
            direct.author = "Reviewer".into();
        }
        direct.validate().map_err(Error::Other)?;
        return Ok(direct);
    }

    let obj = v.as_object().ok_or_else(|| Error::Other(format!("comment #{index} must be a JSON object")))?;
    let id = obj.get("id").or_else(|| obj.get("comment_id")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("fio-{}", index + 1));
    let parent_id = obj.get("parentId").or_else(|| obj.get("parent_id")).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let author = obj
        .get("author")
        .and_then(Value::as_str)
        .or_else(|| obj.get("owner").and_then(|o| o.get("name")).and_then(Value::as_str))
        .unwrap_or("Reviewer")
        .to_string();
    let text = obj.get("text").or_else(|| obj.get("comment")).and_then(Value::as_str).unwrap_or_default().to_string();
    let raw_tc = obj.get("timecode").and_then(Value::as_str).unwrap_or_default();
    let frame = obj
        .get("frame")
        .or_else(|| obj.get("timestamp"))
        .and_then(Value::as_i64)
        .or_else(|| (!raw_tc.is_empty()).then(|| timecode_to_frame(raw_tc, rate, start_tc_frames)).flatten())
        .unwrap_or(0);
    let timecode = if raw_tc.trim().is_empty() { frame_to_timecode(frame, rate, drop_frame, start_tc_frames) } else { raw_tc.to_string() };
    let duration_frames = obj.get("durationFrames").or_else(|| obj.get("duration_frames")).or_else(|| obj.get("duration")).and_then(Value::as_i64).unwrap_or(0);
    let status = if let Some(st_str) = obj.get("status").and_then(Value::as_str) {
        ReviewStatus::parse(st_str).ok_or_else(|| Error::Other(format!("unknown review status `{st_str}`")))?
    } else if obj.get("completed").and_then(Value::as_bool) == Some(true) {
        ReviewStatus::Approved
    } else {
        ReviewStatus::NeedsReview
    };
    let annotations = match obj.get("annotations") {
        None | Some(Value::Null) => Vec::new(),
        Some(arr) => {
            serde_json::from_value::<Vec<TelestratorAnnotation>>(arr.clone()).map_err(|e| Error::Other(format!("invalid annotations on `{id}`: {e}")))?
        }
    };
    let comment = FrameIoReviewComment { id, parent_id, author, frame, timecode, duration_frames, text, status, annotations };
    comment.validate().map_err(Error::Other)?;
    Ok(comment)
}

/// Import Frame.io review comments from JSON (envelope object or raw comment array).
pub fn import_frameio_json(data: &str, rate: FrameRate, drop_frame: bool, start_tc_frames: i64) -> Result<Vec<FrameIoReviewComment>> {
    if data.len() > MAX_REVIEW_BYTES {
        return Err(Error::Other("Frame.io JSON exceeds 16 MiB limit".into()));
    }
    let trimmed = data.trim_start_matches('\u{feff}').trim();
    let root: Value = serde_json::from_str(trimmed).map_err(|e| Error::Other(format!("Frame.io JSON parse error: {e}")))?;
    let (items, eff_rate, eff_df, eff_start) = match &root {
        Value::Array(arr) => (arr.as_slice(), rate, drop_frame, start_tc_frames),
        Value::Object(map) => {
            let arr = map
                .get("comments")
                .or_else(|| map.get("data"))
                .and_then(Value::as_array)
                .ok_or_else(|| Error::Other("Frame.io JSON object must contain a `comments` array".into()))?;
            let (r, df, st) = if let Some(fr) = map.get("frameRate").and_then(Value::as_object) {
                let num = fr.get("num").and_then(Value::as_i64).unwrap_or(rate.num);
                let den = fr.get("den").and_then(Value::as_i64).unwrap_or(rate.den);
                let valid = if num > 0 && den > 0 { FrameRate { num, den } } else { rate };
                let d = fr.get("dropFrame").and_then(Value::as_bool).unwrap_or(drop_frame);
                let s = fr.get("startTimecode").and_then(Value::as_i64).unwrap_or(start_tc_frames);
                (valid, d, s)
            } else {
                (rate, drop_frame, start_tc_frames)
            };
            (arr.as_slice(), r, df, st)
        }
        _ => return Err(Error::Other("Frame.io JSON root must be an object or array".into())),
    };
    if items.len() > MAX_REVIEW_COMMENTS {
        return Err(Error::Other(format!("too many review comments (maximum {MAX_REVIEW_COMMENTS})")));
    }
    let mut out = Vec::with_capacity(items.len());
    let mut seen = std::collections::BTreeSet::new();
    for (idx, item) in items.iter().enumerate() {
        let parsed = parse_external_comment(item, idx, eff_rate, eff_df, eff_start)?;
        if !seen.insert(parsed.id.clone()) {
            return Err(Error::Other(format!("duplicate review comment id `{}`", parsed.id)));
        }
        out.push(parsed);
    }
    Ok(out)
}

fn push_csv_cell(out: &mut String, field: &str, escape_formula: bool) {
    out.push('"');
    if escape_formula && field.trim_start().starts_with(['=', '+', '-', '@']) {
        out.push('\'');
    }
    for ch in field.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
}

fn unescape_formula_cell(cell: &str) -> String {
    if let Some(rest) = cell.strip_prefix('\'')
        && rest.trim_start().starts_with(['=', '+', '-', '@'])
    {
        return rest.to_string();
    }
    cell.to_string()
}

/// Export Frame.io review comments (including reply threading & Telestrator annotations) as RFC 4180 CSV.
pub fn export_frameio_csv(comments: &[FrameIoReviewComment]) -> Result<String> {
    if comments.len() > MAX_REVIEW_COMMENTS {
        return Err(Error::Other(format!("too many review comments (maximum {MAX_REVIEW_COMMENTS})")));
    }
    let mut csv = String::with_capacity(256 + comments.len() * 128);
    csv.push_str(CSV_HEADER);
    csv.push_str("\r\n");
    for c in comments {
        c.validate().map_err(Error::Other)?;
        let ann_json = if c.annotations.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&c.annotations).map_err(|e| Error::Other(format!("annotation serialize error: {e}")))?
        };
        let cells: [(&str, bool); 9] = [
            (&c.id, true),
            (c.parent_id.as_deref().unwrap_or(""), true),
            (&c.author, true),
            (&c.frame.to_string(), false),
            (&c.timecode, false),
            (&c.duration_frames.to_string(), false),
            (c.status.as_str(), false),
            (&c.text, true),
            (&ann_json, false),
        ];
        for (i, (val, esc_formula)) in cells.iter().enumerate() {
            if i > 0 {
                csv.push(',');
            }
            push_csv_cell(&mut csv, val, *esc_formula);
        }
        csv.push_str("\r\n");
        if csv.len() > MAX_REVIEW_BYTES {
            return Err(Error::Other("Frame.io CSV export exceeds 16 MiB limit".into()));
        }
    }
    Ok(csv)
}

/// Parse RFC 4180 CSV into rows of string fields without panicking on malformed quotes.
fn parse_rfc4180_rows(input: &str) -> Result<Vec<Vec<String>>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut in_quotes = false;
    let mut chars = input.trim_start_matches('\u{feff}').chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    let _ = chars.next();
                    cell.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cell.push(ch);
            }
        } else {
            match ch {
                '"' if cell.is_empty() => in_quotes = true,
                ',' => {
                    row.push(std::mem::take(&mut cell));
                }
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        let _ = chars.next();
                    }
                    row.push(std::mem::take(&mut cell));
                    if row.len() > 1 || row.first().is_some_and(|s| !s.trim().is_empty()) {
                        rows.push(std::mem::take(&mut row));
                    } else {
                        row.clear();
                    }
                }
                '\n' => {
                    row.push(std::mem::take(&mut cell));
                    if row.len() > 1 || row.first().is_some_and(|s| !s.trim().is_empty()) {
                        rows.push(std::mem::take(&mut row));
                    } else {
                        row.clear();
                    }
                }
                other => cell.push(other),
            }
        }
    }
    if in_quotes {
        return Err(Error::Other("unterminated quoted field in Frame.io CSV".into()));
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        if row.len() > 1 || row.first().is_some_and(|s| !s.trim().is_empty()) {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn find_col(headers: &[String], names: &[&str]) -> Option<usize> {
    headers.iter().position(|h| {
        let norm = h.trim().to_ascii_lowercase().replace([' ', '_', '-'], "");
        names.iter().any(|cand| norm == *cand)
    })
}

/// Import Frame.io review comments from CSV (both our lossless 9-column CSV and standard Frame.io CSV exports).
pub fn import_frameio_csv(csv_text: &str, rate: FrameRate, drop_frame: bool, start_tc_frames: i64) -> Result<Vec<FrameIoReviewComment>> {
    if csv_text.len() > MAX_REVIEW_BYTES {
        return Err(Error::Other("Frame.io CSV exceeds 16 MiB limit".into()));
    }
    let rows = parse_rfc4180_rows(csv_text)?;
    let Some((header, data_rows)) = rows.split_first() else {
        return Ok(Vec::new());
    };
    if data_rows.len() > MAX_REVIEW_COMMENTS {
        return Err(Error::Other(format!("too many CSV rows (maximum {MAX_REVIEW_COMMENTS})")));
    }

    let c_id = find_col(header, &["id", "commentid"]);
    let c_parent = find_col(header, &["parentid", "replyto", "parent"]);
    let c_author = find_col(header, &["author", "user", "reviewer", "commenter", "owner"]);
    let c_frame = find_col(header, &["frame", "startframe", "framein"]);
    let c_tc = find_col(header, &["timecode", "tc", "timecodein", "starttimecode"]);
    let c_dur = find_col(header, &["durationframes", "duration"]);
    let c_status = find_col(header, &["status", "state", "completed"]);
    let c_text =
        find_col(header, &["comment", "text", "note", "body"]).ok_or_else(|| Error::Other("Frame.io CSV requires a `Comment` or `Text` column".into()))?;
    let c_ann = find_col(header, &["annotations", "telestrator", "drawings"]);

    let cell = |r: &[String], idx: Option<usize>| -> String { idx.and_then(|i| r.get(i)).cloned().unwrap_or_default() };

    let mut out = Vec::with_capacity(data_rows.len());
    let mut seen = std::collections::BTreeSet::new();
    for (row_idx, r) in data_rows.iter().enumerate() {
        let raw_id = unescape_formula_cell(cell(r, c_id).trim());
        let id = if raw_id.is_empty() { format!("fio-{}", row_idx + 1) } else { raw_id };
        let parent_raw = unescape_formula_cell(cell(r, c_parent).trim());
        let parent_id = (!parent_raw.is_empty()).then_some(parent_raw);
        let author_raw = unescape_formula_cell(cell(r, c_author).trim());
        let author = if author_raw.is_empty() { "Reviewer".into() } else { author_raw };
        let tc_raw = cell(r, c_tc).trim().to_string();
        let frame_str = cell(r, c_frame);
        let frame = if !frame_str.trim().is_empty() {
            frame_str.trim().parse::<i64>().map_err(|_| Error::Other(format!("invalid frame `{}` on row {}", frame_str.trim(), row_idx + 2)))?
        } else if !tc_raw.is_empty() {
            timecode_to_frame(&tc_raw, rate, start_tc_frames).ok_or_else(|| Error::Other(format!("invalid timecode `{tc_raw}` on row {}", row_idx + 2)))?
        } else {
            0
        };
        let timecode = if tc_raw.is_empty() { frame_to_timecode(frame, rate, drop_frame, start_tc_frames) } else { tc_raw };
        let dur_str = cell(r, c_dur);
        let duration_frames = if dur_str.trim().is_empty() {
            0
        } else {
            dur_str.trim().parse::<i64>().map_err(|_| Error::Other(format!("invalid durationFrames `{}` on row {}", dur_str.trim(), row_idx + 2)))?
        };
        let st_str = cell(r, c_status);
        let status = if st_str.trim().is_empty() {
            ReviewStatus::NeedsReview
        } else {
            ReviewStatus::parse(&st_str).ok_or_else(|| Error::Other(format!("unknown review status `{}` on row {}", st_str.trim(), row_idx + 2)))?
        };
        let text = unescape_formula_cell(&cell(r, Some(c_text)));
        let ann_str = cell(r, c_ann);
        let annotations = if ann_str.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str::<Vec<TelestratorAnnotation>>(ann_str.trim())
                .map_err(|e| Error::Other(format!("invalid annotations JSON on row {}: {e}", row_idx + 2)))?
        };
        let comment = FrameIoReviewComment { id, parent_id, author, frame, timecode, duration_frames, text, status, annotations };
        comment.validate().map_err(Error::Other)?;
        if !seen.insert(comment.id.clone()) {
            return Err(Error::Other(format!("duplicate review comment id `{}`", comment.id)));
        }
        out.push(comment);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_comments() -> Vec<FrameIoReviewComment> {
        vec![
            FrameIoReviewComment {
                id: "rev-1".into(),
                parent_id: None,
                author: "Director 김감독".into(),
                frame: 48,
                timecode: "00:00:02:00".into(),
                duration_frames: 24,
                text: "Tighten horizon, \"boom\" shadow in top-right\r\n=check exposure".into(),
                status: ReviewStatus::InProgress,
                annotations: vec![
                    TelestratorAnnotation {
                        shape: TelestratorShape::Freehand { points: vec![[0.1, 0.2], [0.25, 0.28], [0.4, 0.35]] },
                        color: [1.0, 0.2, 0.2, 1.0],
                        stroke_width: 3.0,
                    },
                    TelestratorAnnotation {
                        shape: TelestratorShape::Arrow { start: [0.8, 0.1], end: [0.65, 0.22] },
                        color: [1.0, 0.85, 0.1, 1.0],
                        stroke_width: 4.0,
                    },
                    TelestratorAnnotation {
                        shape: TelestratorShape::Rect { min: [0.55, 0.05], max: [0.92, 0.30] },
                        color: [0.2, 0.9, 0.4, 0.9],
                        stroke_width: 2.5,
                    },
                    TelestratorAnnotation {
                        shape: TelestratorShape::Ellipse { center: [0.5, 0.5], radii: [0.15, 0.10] },
                        color: [0.2, 0.6, 1.0, 1.0],
                        stroke_width: 2.0,
                    },
                ],
            },
            FrameIoReviewComment {
                id: "rev-2".into(),
                parent_id: Some("rev-1".into()),
                author: "Colorist 박컬러".into(),
                frame: 48,
                timecode: "00:00:02:00".into(),
                duration_frames: 0,
                text: "Painted out & matched grade.".into(),
                status: ReviewStatus::Approved,
                annotations: Vec::new(),
            },
        ]
    }

    #[test]
    fn json_and_csv_round_trip_preserve_threads_statuses_and_telestrator_shapes() {
        let comments = sample_comments();
        let rate = FrameRate::FPS_24;

        let json_str = export_frameio_json(&comments, "Edit_v3", rate, false, 0).expect("export json");
        let from_json = import_frameio_json(&json_str, rate, false, 0).expect("import json");
        assert_eq!(from_json, comments);

        let csv_str = export_frameio_csv(&comments).expect("export csv");
        let from_csv = import_frameio_csv(&csv_str, rate, false, 0).expect("import csv");
        assert_eq!(from_csv, comments);

        let roots = root_comments(&from_csv);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].id, "rev-1");
        let replies = replies_to(&from_csv, "rev-1");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, "rev-2");
    }

    #[test]
    fn rejects_out_of_bounds_telestrator_coordinates_and_unterminated_csv() {
        let mut bad = sample_comments();
        bad[0].annotations[1] =
            TelestratorAnnotation { shape: TelestratorShape::Arrow { start: [-0.1, 0.5], end: [1.2, 0.5] }, color: [1.0, 0.0, 0.0, 1.0], stroke_width: 3.0 };
        assert!(export_frameio_json(&bad, "Cut", FrameRate::FPS_24, false, 0).is_err());
        assert!(import_frameio_csv("ID,Comment\r\n\"rev-1,\"unclosed", FrameRate::FPS_24, false, 0).is_err());
    }
}
