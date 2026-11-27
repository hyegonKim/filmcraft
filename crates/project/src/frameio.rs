//! Frame.io (`FFC_framemob`) team review comments, approval states, reply threading, and
//! on-frame Telestrator vector annotations.

use serde::{Deserialize, Serialize};

use crate::Label;

/// Review & approval status for a Frame.io comment thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewStatus {
    #[default]
    #[serde(alias = "NeedsReview", alias = "needs_review")]
    NeedsReview,
    #[serde(alias = "InProgress", alias = "in_progress")]
    InProgress,
    #[serde(alias = "Approved", alias = "approved")]
    Approved,
}

impl ReviewStatus {
    pub const ALL: [ReviewStatus; 3] = [ReviewStatus::NeedsReview, ReviewStatus::InProgress, ReviewStatus::Approved];

    pub fn as_str(self) -> &'static str {
        match self {
            ReviewStatus::NeedsReview => "needsReview",
            ReviewStatus::InProgress => "inProgress",
            ReviewStatus::Approved => "approved",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ReviewStatus::NeedsReview => "Needs Review",
            ReviewStatus::InProgress => "In Progress",
            ReviewStatus::Approved => "Approved",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace([' ', '_', '-'], "").as_str() {
            "needsreview" | "open" | "pending" | "todo" => Some(ReviewStatus::NeedsReview),
            "inprogress" | "working" | "wip" => Some(ReviewStatus::InProgress),
            "approved" | "done" | "resolved" | "completed" => Some(ReviewStatus::Approved),
            _ => None,
        }
    }

    /// Timeline marker color corresponding to this review status.
    pub fn marker_label(self) -> Label {
        match self {
            ReviewStatus::NeedsReview => Label::Mango,
            ReviewStatus::InProgress => Label::Caribbean,
            ReviewStatus::Approved => Label::Forest,
        }
    }
}

/// Normalized `[0, 1]` on-frame vector Telestrator geometry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TelestratorShape {
    #[serde(alias = "Freehand")]
    Freehand { points: Vec<[f32; 2]> },
    #[serde(alias = "Arrow")]
    Arrow { start: [f32; 2], end: [f32; 2] },
    #[serde(alias = "Rect")]
    Rect { min: [f32; 2], max: [f32; 2] },
    #[serde(alias = "Ellipse")]
    Ellipse { center: [f32; 2], radii: [f32; 2] },
}

impl TelestratorShape {
    pub fn kind_name(&self) -> &'static str {
        match self {
            TelestratorShape::Freehand { .. } => "freehand",
            TelestratorShape::Arrow { .. } => "arrow",
            TelestratorShape::Rect { .. } => "rect",
            TelestratorShape::Ellipse { .. } => "ellipse",
        }
    }
}

/// On-frame vector Telestrator stroke with normalized `[0, 1]` coordinates and RGBA color.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelestratorAnnotation {
    pub shape: TelestratorShape,
    /// Straight sRGB RGBA stroke color in `[0.0, 1.0]`.
    pub color: [f32; 4],
    /// Stroke thickness in logical points (`0.5..=64.0`).
    #[serde(default = "default_stroke_width")]
    pub stroke_width: f32,
}

fn default_stroke_width() -> f32 {
    3.0
}

fn valid_unit_pt(p: [f32; 2]) -> bool {
    p[0].is_finite() && p[1].is_finite() && (0.0..=1.0).contains(&p[0]) && (0.0..=1.0).contains(&p[1])
}

impl TelestratorAnnotation {
    pub const MAX_FREEHAND_POINTS: usize = 4096;

    pub fn validate(&self) -> Result<(), String> {
        for c in self.color {
            if !c.is_finite() || !(0.0..=1.0).contains(&c) {
                return Err("annotation RGBA color components must be finite numbers in [0, 1]".into());
            }
        }
        if !self.stroke_width.is_finite() || !(0.25..=64.0).contains(&self.stroke_width) {
            return Err("annotation strokeWidth must be between 0.25 and 64.0".into());
        }
        match &self.shape {
            TelestratorShape::Freehand { points } => {
                if points.len() < 2 || points.len() > Self::MAX_FREEHAND_POINTS {
                    return Err(format!("freehand annotation needs 2..={} normalized points", Self::MAX_FREEHAND_POINTS));
                }
                if !points.iter().copied().all(valid_unit_pt) {
                    return Err("freehand points must have normalized coordinates in [0, 1]".into());
                }
            }
            TelestratorShape::Arrow { start, end } => {
                if !valid_unit_pt(*start) || !valid_unit_pt(*end) {
                    return Err("arrow start and end must have normalized coordinates in [0, 1]".into());
                }
            }
            TelestratorShape::Rect { min, max } => {
                if !valid_unit_pt(*min) || !valid_unit_pt(*max) {
                    return Err("rect min and max must have normalized coordinates in [0, 1]".into());
                }
                if min[0] > max[0] || min[1] > max[1] {
                    return Err("rect min must not exceed max".into());
                }
            }
            TelestratorShape::Ellipse { center, radii } => {
                if !valid_unit_pt(*center) {
                    return Err("ellipse center must have normalized coordinates in [0, 1]".into());
                }
                if !radii[0].is_finite() || !radii[1].is_finite() || !(0.0..=1.0).contains(&radii[0]) || !(0.0..=1.0).contains(&radii[1]) {
                    return Err("ellipse radii must be finite numbers in [0, 1]".into());
                }
            }
        }
        Ok(())
    }
}

/// A Frame.io review comment or threaded reply anchored to a timeline frame / timecode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameIoReviewComment {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub author: String,
    pub frame: i64,
    pub timecode: String,
    #[serde(default)]
    pub duration_frames: i64,
    pub text: String,
    #[serde(default)]
    pub status: ReviewStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<TelestratorAnnotation>,
}

impl FrameIoReviewComment {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() || self.id.len() > 256 || self.id.chars().any(char::is_control) {
            return Err("review comment id must be 1..=256 printable characters".into());
        }
        if let Some(pid) = &self.parent_id
            && (pid.trim().is_empty() || pid == &self.id || pid.len() > 256 || pid.chars().any(char::is_control))
        {
            return Err("parentId must name a distinct printable comment id".into());
        }
        if self.author.len() > 512 || self.author.chars().any(char::is_control) {
            return Err("author must be at most 512 printable bytes".into());
        }
        if self.frame < 0 || self.duration_frames < 0 || self.duration_frames > 10_000_000 {
            return Err("frame and durationFrames must be non-negative within supported bounds".into());
        }
        if self.text.trim().is_empty() || self.text.len() > 65_536 {
            return Err("comment text must be 1..=65536 bytes".into());
        }
        if self.annotations.len() > 256 {
            return Err("a single comment may carry at most 256 annotations".into());
        }
        for ann in &self.annotations {
            ann.validate()?;
        }
        Ok(())
    }
}

/// Top-level review comments sorted by `(frame, id)`.
pub fn root_comments(comments: &[FrameIoReviewComment]) -> Vec<&FrameIoReviewComment> {
    let ids: std::collections::BTreeSet<&str> = comments.iter().map(|c| c.id.as_str()).collect();
    let mut roots: Vec<&FrameIoReviewComment> = comments.iter().filter(|c| c.parent_id.as_deref().is_none_or(|pid| !ids.contains(pid))).collect();
    roots.sort_by(|a, b| a.frame.cmp(&b.frame).then_with(|| a.id.cmp(&b.id)));
    roots
}

/// Direct replies to `parent_id` in chronological/id order.
pub fn replies_to<'a>(comments: &'a [FrameIoReviewComment], parent_id: &str) -> Vec<&'a FrameIoReviewComment> {
    let mut out: Vec<&FrameIoReviewComment> = comments.iter().filter(|c| c.parent_id.as_deref() == Some(parent_id)).collect();
    out.sort_by(|a, b| a.frame.cmp(&b.frame).then_with(|| a.id.cmp(&b.id)));
    out
}
