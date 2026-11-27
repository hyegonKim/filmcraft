//! Engine commands for Frame.io (`FFC_framemob`) Team Review & Approval Workflow:
//! - `review.importFrameIo`: import Frame.io JSON or CSV comments + Telestrator vector annotations
//!   into the active sequence and synchronize timeline markers.
//! - `review.exportFrameIo`: export review comments, reply threads, and Telestrator annotations
//!   (plus any unsynced sequence markers) to Frame.io JSON or CSV.
//! - `review.addComment`: add a frame-accurate review comment or threaded reply (`parentId`).
//! - `review.setStatus`: set `needsReview`, `inProgress`, or `approved` on a comment thread.
//! - `review.addAnnotation`: attach normalized `[0, 1]` Telestrator vector shapes (`freehand`,
//!   `arrow`, `rect`, `ellipse`) with RGBA stroke to a review comment.
//! - `review.list`: query the active sequence's threaded review comments.

use filmcraft_interchange::frameio::{
    FrameIoFormat, FrameIoReviewComment, ReviewStatus, TelestratorAnnotation, TelestratorShape, export_frameio_csv, export_frameio_json, frame_to_timecode,
    import_frameio_csv, import_frameio_json, timecode_to_frame,
};
use filmcraft_project::{ItemId, Marker, MarkerId, MarkerKind, Project, Sequence};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, NamedKeys, bad, bool_p, has_seq, str_p};
use crate::{EngineError, Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

const MARKER_TAG_PREFIX: &str = "frameio://comment/";

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, enabled: Enabled, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled, run, journal: true, named: NamedKeys::NONE }
}

fn query(id: &'static str, label: &'static str, params: &'static str, enabled: Enabled, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled, run, journal: false, named: NamedKeys::NONE }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "review.importFrameIo",
            "Import Frame.io Review…",
            &["Markers"],
            r#"{"path":str?,"data":str?,"format":str?,"replace":bool?}"#,
            has_seq,
            import_frameio,
        ),
        query("review.exportFrameIo", "Export Frame.io Review…", r#"{"path":str?,"format":str?}"#, has_seq, export_frameio),
        spec(
            "review.addComment",
            "Add Review Comment",
            &["Markers"],
            r#"{"text":str,"author":str?,"frame":int?,"timecode":str?,"durationFrames":int?,"parentId":str?,"status":str?,"id":str?}"#,
            has_seq,
            add_comment,
        ),
        spec("review.setStatus", "Set Review Comment Status", &[], r#"{"id":str,"status":str}"#, has_seq, set_status),
        spec(
            "review.addAnnotation",
            "Add Review Telestrator Annotation",
            &[],
            r#"{"id":str,"kind":str,"points":[[n,n]]?,"start":[n,n]?,"end":[n,n]?,"min":[n,n]?,"max":[n,n]?,"center":[n,n]?,"radii":[n,n]?,"color":[n,n,n,n]?,"strokeWidth":n?}"#,
            has_seq,
            add_annotation,
        ),
        query("review.list", "List Review Comments", "{}", has_seq, list_comments),
    ]
}

fn marker_tag_url(comment_id: &str) -> String {
    format!("{MARKER_TAG_PREFIX}{comment_id}")
}

fn comment_summary_note(c: &FrameIoReviewComment, reply_count: usize) -> String {
    let mut note = format!("[{}] {}", c.status.label(), c.text);
    if !c.annotations.is_empty() {
        note.push_str(&format!(" ({} drawing(s))", c.annotations.len()));
    }
    if reply_count > 0 {
        note.push_str(&format!(" · {reply_count} repl{}", if reply_count == 1 { "y" } else { "ies" }));
    }
    note
}

/// Synchronize root `FrameIoReviewComment` entries into `Sequence::markers` so every review thread
/// appears on the timeline ruler and Markers panel at its exact `Tick` position.
fn sync_sequence_markers(pr: &mut Project, seq_id: ItemId) {
    let comments = pr.review_comments.get(&seq_id).cloned().unwrap_or_default();
    let Some(seq_meta) = pr.sequence(seq_id) else { return };
    let rate = seq_meta.settings.frame_rate;

    let ids: std::collections::BTreeSet<&str> = comments.iter().map(|c| c.id.as_str()).collect();
    let mut desired: Vec<(&FrameIoReviewComment, usize)> = comments
        .iter()
        .filter(|c| c.parent_id.as_deref().is_none_or(|pid| !ids.contains(pid)))
        .map(|root| {
            let replies = comments.iter().filter(|r| r.parent_id.as_deref() == Some(root.id.as_str())).count();
            (root, replies)
        })
        .collect();
    desired.sort_by(|(a, _), (b, _)| a.frame.cmp(&b.frame).then_with(|| a.id.cmp(&b.id)));

    let wanted_urls: std::collections::BTreeSet<String> = desired.iter().map(|(c, _)| marker_tag_url(&c.id)).collect();

    // Allocate enough fresh MarkerIds ahead of borrowing `seq_mut`.
    let existing_urls: std::collections::BTreeSet<String> =
        seq_meta.markers.iter().filter(|m| m.url.starts_with(MARKER_TAG_PREFIX)).map(|m| m.url.clone()).collect();
    let needed_ids = desired.iter().filter(|(c, _)| !existing_urls.contains(&marker_tag_url(&c.id))).count();
    let mut fresh_ids: Vec<MarkerId> = (0..needed_ids).map(|_| MarkerId(pr.alloc_id())).collect();

    let Some(seq) = pr.sequence_mut(seq_id) else { return };
    seq.markers.retain(|m| !m.url.starts_with(MARKER_TAG_PREFIX) || wanted_urls.contains(&m.url));

    for (c, reply_count) in desired {
        let tag = marker_tag_url(&c.id);
        let start = rate.tick_of(c.frame.max(0));
        let duration = rate.tick_of(c.duration_frames.max(0));
        let name = if c.author.trim().is_empty() { format!("Frame.io · {}", c.status.label()) } else { format!("{} ({})", c.author, c.status.label()) };
        let comment = comment_summary_note(c, reply_count);
        let color = c.status.marker_label();
        if let Some(existing) = seq.markers.iter_mut().find(|m| m.url == tag) {
            existing.start = start;
            existing.duration = duration;
            existing.name = name;
            existing.comment = comment;
            existing.author.clone_from(&c.author);
            existing.color = color;
            existing.kind = MarkerKind::Comment;
        } else if let Some(id) = fresh_ids.pop() {
            seq.markers.push(Marker {
                url: tag,
                author: c.author.clone(),
                custom_color: None,
                id,
                start,
                duration,
                name,
                comment,
                kind: MarkerKind::Comment,
                color,
            });
        }
    }
    seq.markers.sort_by_key(|m| (m.start, m.id));
}

/// Include any ordinary timeline markers that aren't already represented by `review_comments` when exporting.
fn merged_comments_for_export(seq: &Sequence, stored: &[FrameIoReviewComment]) -> Vec<FrameIoReviewComment> {
    let mut out = stored.to_vec();
    let rate = seq.settings.frame_rate;
    for m in &seq.markers {
        if m.url.starts_with(MARKER_TAG_PREFIX) {
            continue;
        }
        let cid = format!("marker-{}", m.id.0);
        if out.iter().any(|c| c.id == cid) {
            continue;
        }
        let frame = rate.frame_at(m.start).max(0);
        let duration_frames = rate.frame_at(m.duration).max(0);
        let text = if !m.comment.trim().is_empty() {
            m.comment.clone()
        } else if !m.name.trim().is_empty() {
            m.name.clone()
        } else {
            "Timeline Marker".into()
        };
        out.push(FrameIoReviewComment {
            id: cid,
            parent_id: None,
            author: if m.author.trim().is_empty() { "Editor".into() } else { m.author.clone() },
            frame,
            timecode: frame_to_timecode(frame, rate, seq.settings.drop_frame, seq.start_timecode),
            duration_frames,
            text,
            status: ReviewStatus::NeedsReview,
            annotations: Vec::new(),
        });
    }
    out.sort_by(|a, b| a.frame.cmp(&b.frame).then_with(|| a.id.cmp(&b.id)));
    out
}

fn import_frameio(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "review.importFrameIo";
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let seq = s.project.sequence(seq_id).ok_or(EngineError::NoSequence)?;
    let rate = seq.settings.frame_rate;
    let drop_frame = seq.settings.drop_frame;
    let start_tc = seq.start_timecode;

    let path_opt = str_p(p, "path").map(str::trim).filter(|v| !v.is_empty());
    let payload = if let Some(inline) = str_p(p, "data") {
        inline.to_string()
    } else if let Some(path) = path_opt {
        if path.contains('\0') {
            return Err(bad(CMD, "`path` must not contain NUL characters"));
        }
        let bytes = s.services.read_file(path).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
        String::from_utf8(bytes).map_err(|e| bad(CMD, format!("{path}: invalid UTF-8: {e}")))?
    } else {
        return Err(bad(CMD, "provide `path` or inline `data`"));
    };

    let fmt = match str_p(p, "format") {
        Some(f) => FrameIoFormat::parse(f).ok_or_else(|| bad(CMD, "format must be `json` or `csv`"))?,
        None => FrameIoFormat::detect(&payload, path_opt),
    };
    let replace = bool_p(p, "replace").unwrap_or(false);

    let imported = match fmt {
        FrameIoFormat::Json => import_frameio_json(&payload, rate, drop_frame, start_tc),
        FrameIoFormat::Csv => import_frameio_csv(&payload, rate, drop_frame, start_tc),
    }
    .map_err(|e| bad(CMD, e.to_string()))?;

    let imported_count = imported.len();
    let mut total_count = 0_usize;
    s.edit("Import Frame.io Review", |pr, _| {
        let list = pr.review_comments.entry(seq_id).or_default();
        if replace {
            *list = imported;
        } else {
            for item in imported {
                if let Some(existing) = list.iter_mut().find(|c| c.id == item.id) {
                    *existing = item;
                } else {
                    list.push(item);
                }
            }
        }
        total_count = list.len();
        sync_sequence_markers(pr, seq_id);
        Ok(())
    })?;

    Ok(json!({
        "format": fmt.as_str(),
        "imported": imported_count,
        "total": total_count,
    }))
}

fn export_frameio(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "review.exportFrameIo";
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let seq = s.project.sequence(seq_id).ok_or(EngineError::NoSequence)?;
    seq.settings.validate().map_err(|e| bad(CMD, e))?;
    let seq_name = s.project.item(seq_id).map(|i| i.name.as_str()).unwrap_or("Sequence");

    let path_opt = match p.get("path") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let path = v.as_str().filter(|s| !s.trim().is_empty() && !s.contains('\0')).ok_or_else(|| bad(CMD, "`path` must be a non-empty string"))?;
            Some(path)
        }
    };
    let fmt = match str_p(p, "format") {
        Some(f) => FrameIoFormat::parse(f).ok_or_else(|| bad(CMD, "format must be `json` or `csv`"))?,
        None => path_opt.and_then(|pt| pt.rsplit_once('.').and_then(|(_, ext)| FrameIoFormat::parse(ext))).unwrap_or(FrameIoFormat::Json),
    };

    let stored = s.project.review_comments.get(&seq_id).map(Vec::as_slice).unwrap_or(&[]);
    let comments = merged_comments_for_export(seq, stored);
    let data = match fmt {
        FrameIoFormat::Json => export_frameio_json(&comments, seq_name, seq.settings.frame_rate, seq.settings.drop_frame, seq.start_timecode),
        FrameIoFormat::Csv => export_frameio_csv(&comments),
    }
    .map_err(|e| bad(CMD, e.to_string()))?;

    if let Some(path) = path_opt {
        s.services.write_file(path, data.as_bytes()).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    }

    Ok(json!({
        "format": fmt.as_str(),
        "count": comments.len(),
        "bytes": data.len(),
        "path": path_opt,
        "data": data,
    }))
}

fn add_comment(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "review.addComment";
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let seq = s.project.sequence(seq_id).ok_or(EngineError::NoSequence)?;
    let rate = seq.settings.frame_rate;
    let drop_frame = seq.settings.drop_frame;
    let start_tc = seq.start_timecode;

    let text = str_p(p, "text").map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| bad(CMD, "need non-empty `text`"))?.to_string();
    let author = str_p(p, "author").map(str::trim).filter(|a| !a.is_empty()).unwrap_or("Reviewer").to_string();
    let parent_id = match p.get("parentId") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let pid = v.as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad(CMD, "`parentId` must be a non-empty string"))?;
            let exists = s.project.review_comments.get(&seq_id).is_some_and(|list| list.iter().any(|c| c.id == pid));
            if !exists {
                return Err(bad(CMD, format!("parent comment `{pid}` does not exist in the active sequence")));
            }
            Some(pid.to_string())
        }
    };

    let frame = if let Some(f_val) = p.get("frame").filter(|v| !v.is_null()) {
        let f = f_val.as_i64().ok_or_else(|| bad(CMD, "`frame` must be an integer"))?;
        if f < 0 {
            return Err(bad(CMD, "`frame` must be non-negative"));
        }
        f
    } else if let Some(tc_str) = str_p(p, "timecode") {
        timecode_to_frame(tc_str, rate, start_tc).ok_or_else(|| bad(CMD, format!("invalid timecode `{tc_str}`")))?
    } else if let Some(pid) = &parent_id {
        s.project
            .review_comments
            .get(&seq_id)
            .and_then(|list| list.iter().find(|c| &c.id == pid))
            .map_or_else(|| rate.frame_at(s.playhead()).max(0), |parent| parent.frame)
    } else {
        rate.frame_at(s.playhead()).max(0)
    };

    let duration_frames = match p.get("durationFrames").filter(|v| !v.is_null()) {
        None => 0,
        Some(v) => v.as_i64().filter(|d| (0..=10_000_000).contains(d)).ok_or_else(|| bad(CMD, "`durationFrames` must be 0..=10000000"))?,
    };
    let status = match str_p(p, "status") {
        None => ReviewStatus::NeedsReview,
        Some(st) => ReviewStatus::parse(st).ok_or_else(|| bad(CMD, "status must be `needsReview`, `inProgress`, or `approved`"))?,
    };
    let custom_id = match p.get("id") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad(CMD, "`id` must be a non-empty string"))?.to_string()),
    };

    let timecode = frame_to_timecode(frame, rate, drop_frame, start_tc);
    let mut created_id = String::new();

    s.edit("Add Review Comment", |pr, _| {
        let id = match custom_id {
            Some(cid) => {
                if pr.review_comments.get(&seq_id).is_some_and(|l| l.iter().any(|c| c.id == cid)) {
                    return Err(bad(CMD, format!("review comment `{cid}` already exists")));
                }
                cid
            }
            None => format!("rev-{}", pr.alloc_id()),
        };
        let comment = FrameIoReviewComment {
            id: id.clone(),
            parent_id: parent_id.clone(),
            author: author.clone(),
            frame,
            timecode: timecode.clone(),
            duration_frames,
            text: text.clone(),
            status,
            annotations: Vec::new(),
        };
        comment.validate().map_err(|e| bad(CMD, e))?;
        pr.review_comments.entry(seq_id).or_default().push(comment);
        sync_sequence_markers(pr, seq_id);
        created_id = id;
        Ok(())
    })?;

    Ok(json!({
        "id": created_id,
        "frame": frame,
        "timecode": timecode,
        "status": status.as_str(),
        "parentId": parent_id,
    }))
}

fn set_status(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "review.setStatus";
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let id = str_p(p, "id").map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad(CMD, "need comment `id`"))?.to_string();
    let status_raw = str_p(p, "status").ok_or_else(|| bad(CMD, "need `status`"))?;
    let status = ReviewStatus::parse(status_raw).ok_or_else(|| bad(CMD, "status must be `needsReview`, `inProgress`, or `approved`"))?;

    let exists = s.project.review_comments.get(&seq_id).is_some_and(|list| list.iter().any(|c| c.id == id));
    if !exists {
        return Err(bad(CMD, format!("no review comment `{id}` in the active sequence")));
    }

    s.edit("Set Review Status", |pr, _| {
        if let Some(list) = pr.review_comments.get_mut(&seq_id)
            && let Some(c) = list.iter_mut().find(|c| c.id == id)
        {
            c.status = status;
        }
        sync_sequence_markers(pr, seq_id);
        Ok(())
    })?;

    Ok(json!({ "id": id, "status": status.as_str() }))
}

fn parse_pair(v: Option<&Value>, key: &str, cmd: &str) -> Result<[f32; 2]> {
    let arr = v.and_then(Value::as_array).filter(|a| a.len() == 2).ok_or_else(|| bad(cmd, format!("`{key}` must be a [x, y] pair")))?;
    let x = arr.first().and_then(Value::as_f64).filter(|n| n.is_finite()).ok_or_else(|| bad(cmd, format!("`{key}[0]` must be finite")))? as f32;
    let y = arr.get(1).and_then(Value::as_f64).filter(|n| n.is_finite()).ok_or_else(|| bad(cmd, format!("`{key}[1]` must be finite")))? as f32;
    Ok([x, y])
}

fn parse_annotation(p: &Value, cmd: &str) -> Result<TelestratorAnnotation> {
    let kind = str_p(p, "kind").ok_or_else(|| bad(cmd, "need `kind` (`freehand`, `arrow`, `rect`, or `ellipse`)"))?;
    let shape = match kind.trim().to_ascii_lowercase().as_str() {
        "freehand" => {
            let pts_val = p.get("points").and_then(Value::as_array).ok_or_else(|| bad(cmd, "`freehand` requires `points`: [[x, y], ...]"))?;
            let mut points = Vec::with_capacity(pts_val.len());
            for pt in pts_val {
                points.push(parse_pair(Some(pt), "points[]", cmd)?);
            }
            TelestratorShape::Freehand { points }
        }
        "arrow" => TelestratorShape::Arrow { start: parse_pair(p.get("start"), "start", cmd)?, end: parse_pair(p.get("end"), "end", cmd)? },
        "rect" | "rectangle" => {
            let a = parse_pair(p.get("min"), "min", cmd)?;
            let b = parse_pair(p.get("max"), "max", cmd)?;
            TelestratorShape::Rect { min: [a[0].min(b[0]), a[1].min(b[1])], max: [a[0].max(b[0]), a[1].max(b[1])] }
        }
        "ellipse" | "circle" => {
            TelestratorShape::Ellipse { center: parse_pair(p.get("center"), "center", cmd)?, radii: parse_pair(p.get("radii"), "radii", cmd)? }
        }
        _ => return Err(bad(cmd, format!("unknown annotation kind `{kind}` (expected freehand, arrow, rect, or ellipse)"))),
    };
    let color = match p.get("color").filter(|v| !v.is_null()) {
        None => [1.0, 0.25, 0.25, 1.0],
        Some(v) => {
            let arr = v.as_array().filter(|a| a.len() == 4).ok_or_else(|| bad(cmd, "`color` must be [r, g, b, a] in [0, 1]"))?;
            let mut rgba = [0.0_f32; 4];
            for (dst, item) in rgba.iter_mut().zip(arr) {
                let f = item.as_f64().filter(|n| n.is_finite()).ok_or_else(|| bad(cmd, "`color` components must be finite"))? as f32;
                *dst = f;
            }
            rgba
        }
    };
    let stroke_width = match p.get("strokeWidth").filter(|v| !v.is_null()) {
        None => 3.0,
        Some(v) => v.as_f64().filter(|n| n.is_finite()).ok_or_else(|| bad(cmd, "`strokeWidth` must be finite"))? as f32,
    };
    let ann = TelestratorAnnotation { shape, color, stroke_width };
    ann.validate().map_err(|e| bad(cmd, e))?;
    Ok(ann)
}

fn add_annotation(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "review.addAnnotation";
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let id = str_p(p, "id").map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad(CMD, "need comment `id`"))?.to_string();
    let ann = parse_annotation(p, CMD)?;

    let exists = s.project.review_comments.get(&seq_id).is_some_and(|list| list.iter().any(|c| c.id == id));
    if !exists {
        return Err(bad(CMD, format!("no review comment `{id}` in the active sequence")));
    }

    let mut count = 0_usize;
    let kind_name = ann.shape.kind_name();
    s.edit("Add Review Annotation", |pr, _| {
        if let Some(list) = pr.review_comments.get_mut(&seq_id)
            && let Some(c) = list.iter_mut().find(|c| c.id == id)
        {
            c.annotations.push(ann);
            count = c.annotations.len();
        }
        sync_sequence_markers(pr, seq_id);
        Ok(())
    })?;

    Ok(json!({ "id": id, "kind": kind_name, "annotations": count }))
}

fn list_comments(s: &mut Session, _: &Value) -> Result<Value> {
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let comments = s.project.review_comments.get(&seq_id).cloned().unwrap_or_default();
    Ok(json!({ "comments": comments }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frameio_review_workflow_syncs_markers_threads_telestrator_and_round_trips() {
        let mut s = Session::default();
        s.execute("file.newSequence", json!({ "name": "FrameIo Cut", "fps": 24 })).expect("new sequence");

        // Add root comment at frame 24 (00:00:01:00)
        let root = s
            .execute(
                "review.addComment",
                json!({
                    "author": "Client",
                    "frame": 24,
                    "durationFrames": 12,
                    "text": "Brighten product hero shot",
                }),
            )
            .expect("add root comment");
        let root_id = root["id"].as_str().expect("root id").to_string();
        assert_eq!(root["timecode"], json!("00:00:01:00"));

        // Add threaded reply
        let reply = s
            .execute(
                "review.addComment",
                json!({
                    "author": "Colorist",
                    "parentId": root_id,
                    "text": "Lifted midtones +0.35 EV",
                }),
            )
            .expect("add reply");
        assert_eq!(reply["parentId"], json!(root_id));

        // Attach all 4 Telestrator vector shapes
        s.execute(
            "review.addAnnotation",
            json!({ "id": root_id, "kind": "freehand", "points": [[0.1, 0.1], [0.2, 0.25], [0.35, 0.3]], "color": [1.0, 0.2, 0.2, 1.0] }),
        )
        .expect("freehand");
        s.execute("review.addAnnotation", json!({ "id": root_id, "kind": "arrow", "start": [0.75, 0.2], "end": [0.55, 0.4] })).expect("arrow");
        s.execute("review.addAnnotation", json!({ "id": root_id, "kind": "rect", "min": [0.3, 0.2], "max": [0.7, 0.8] })).expect("rect");
        let ann_res =
            s.execute("review.addAnnotation", json!({ "id": root_id, "kind": "ellipse", "center": [0.5, 0.5], "radii": [0.18, 0.12] })).expect("ellipse");
        assert_eq!(ann_res["annotations"], json!(4));

        // Approve thread and verify sequence marker sync
        s.execute("review.setStatus", json!({ "id": root_id, "status": "approved" })).expect("setStatus");
        let seq = s.active_sequence().expect("sequence");
        assert_eq!(seq.markers.len(), 1);
        assert_eq!(seq.markers[0].color, filmcraft_project::Label::Forest);
        assert!(seq.markers[0].comment.contains("4 drawing(s)"));
        assert!(seq.markers[0].comment.contains("1 reply"));

        // Export CSV & JSON and re-import into a clean sequence
        let csv_out = s.execute("review.exportFrameIo", json!({ "format": "csv" })).expect("export csv");
        let json_out = s.execute("review.exportFrameIo", json!({ "format": "json" })).expect("export json");
        assert_eq!(csv_out["count"], json!(2));
        assert_eq!(json_out["count"], json!(2));

        s.execute("file.newSequence", json!({ "name": "Imported Cut", "fps": 24 })).expect("second sequence");
        let imp = s.execute("review.importFrameIo", json!({ "data": csv_out["data"], "format": "csv", "replace": true })).expect("import csv");
        assert_eq!(imp["total"], json!(2));
        let listed = s.execute("review.list", json!({})).expect("list");
        assert_eq!(listed["comments"].as_array().expect("comments").len(), 2);

        // Undo/redo restores both review_comments and timeline markers
        assert!(s.undo().is_some());
        assert!(s.active_sequence().expect("seq").markers.is_empty());
        assert!(s.redo().is_some());
        assert_eq!(s.active_sequence().expect("seq").markers.len(), 1);
    }
}
