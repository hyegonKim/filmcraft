//! Apple Compressor-style delivery packaging & batch automation for **FilmCraft**:
//! - **HTTP Live Streaming (HLS, RFC 8216)** multi-bitrate Master Playlist (`master.m3u8`) and
//!   segmented Media Playlist (`prog_index.m3u8`) generation with RFC 6381 codec strings and
//!   discrete `EXT-X-MEDIA:TYPE=AUDIO` renditions.
//! - **IMF (Interoperable Master Format, SMPTE ST 2067-2 / ST 2067-3 / ST 429-8 / ST 429-9)**
//!   package XML builder (`CPL.xml`, `PKL.xml`, `ASSETMAP.xml`, `VOLINDEX.xml`) for Netflix /
//!   theatrical / broadcast studio delivery alongside FilmCraft's MXF exporter.
//! - **`DestinationBundle` & `WatchFolderRule`** multi-output batch presets and watch-folder
//!   matching rules.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::{ExportSettings, Format};

// ============================================================================
// 1. HTTP Live Streaming (HLS, RFC 8216) Multi-Bitrate Playlist Builder
// ============================================================================

/// A single video rendition in an HLS Adaptive Bitrate (ABR) ladder (`#EXT-X-STREAM-INF`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HlsRendition {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub frame_rate_fps: f64,
    /// Peak bitrate in bits per second (`BANDWIDTH`).
    pub bandwidth_bps: u64,
    /// Average bitrate in bits per second (`AVERAGE-BANDWIDTH`).
    pub average_bandwidth_bps: u64,
    /// RFC 6381 codec string (e.g. `avc1.640028,mp4a.40.2` or `hvc1.2.4.L150.B0,mp4a.40.2`).
    pub codecs: String,
    /// Optional reference to an `#EXT-X-MEDIA:TYPE=AUDIO` group id.
    pub audio_group_id: Option<String>,
    /// Relative URI of this rendition's media playlist (e.g. `1080p/prog_index.m3u8`).
    pub playlist_uri: String,
}

impl HlsRendition {
    /// Standard 4-rung H.264/HEVC ABR ladder (360p, 720p, 1080p, 2160p) like Apple Compressor's
    /// "HTTP Live Streaming" destination bundle.
    pub fn standard_abr_ladder(fps: f64, audio_group: Option<&str>) -> Vec<Self> {
        let fps = if fps.is_finite() && fps > 0.0 {
            fps
        } else {
            29.97
        };
        let ag = audio_group.map(ToString::to_string);
        vec![
            Self {
                name: "360p SDR".into(),
                width: 640,
                height: 360,
                frame_rate_fps: fps,
                bandwidth_bps: 990_000,
                average_bandwidth_bps: 800_000,
                codecs: "avc1.4d401e,mp4a.40.2".into(),
                audio_group_id: ag.clone(),
                playlist_uri: "v_360p/prog_index.m3u8".into(),
            },
            Self {
                name: "720p HD".into(),
                width: 1280,
                height: 720,
                frame_rate_fps: fps,
                bandwidth_bps: 3_400_000,
                average_bandwidth_bps: 2_800_000,
                codecs: "avc1.64001f,mp4a.40.2".into(),
                audio_group_id: ag.clone(),
                playlist_uri: "v_720p/prog_index.m3u8".into(),
            },
            Self {
                name: "1080p Full HD".into(),
                width: 1920,
                height: 1080,
                frame_rate_fps: fps,
                bandwidth_bps: 6_800_000,
                average_bandwidth_bps: 5_500_000,
                codecs: "avc1.640028,mp4a.40.2".into(),
                audio_group_id: ag.clone(),
                playlist_uri: "v_1080p/prog_index.m3u8".into(),
            },
            Self {
                name: "2160p 4K UHD".into(),
                width: 3840,
                height: 2160,
                frame_rate_fps: fps,
                bandwidth_bps: 18_500_000,
                average_bandwidth_bps: 15_000_000,
                codecs: "hvc1.2.4.L150.B0,mp4a.40.2".into(),
                audio_group_id: ag,
                playlist_uri: "v_2160p/prog_index.m3u8".into(),
            },
        ]
    }
}

/// Alternate or discrete audio rendition (`#EXT-X-MEDIA:TYPE=AUDIO`) in an HLS Master Playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HlsAudioGroup {
    pub group_id: String,
    pub name: String,
    pub language: String,
    pub channels: u8,
    pub uri: String,
    pub is_default: bool,
    pub autoselect: bool,
}

/// A single media segment (`#EXTINF`) in an HLS Media Playlist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HlsSegment {
    pub duration_sec: f64,
    pub uri: String,
    pub title: String,
    /// Optional `#EXT-X-BYTERANGE:<length>@<offset>` for single-file fragmented MP4 segments.
    pub byte_range: Option<(u64, u64)>,
}

/// Build an RFC 8216 HLS Master Playlist (`master.m3u8`) from video [`HlsRendition`]s and
/// optional [`HlsAudioGroup`] renditions.
pub fn build_hls_master_playlist(
    renditions: &[HlsRendition],
    audio_groups: &[HlsAudioGroup],
) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:7\n");
    out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    if !audio_groups.is_empty() {
        out.push('\n');
        for ag in audio_groups {
            let def = if ag.is_default { "YES" } else { "NO" };
            let auto = if ag.autoselect { "YES" } else { "NO" };
            let _ = writeln!(
                out,
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{}\",NAME=\"{}\",LANGUAGE=\"{}\",CHANNELS=\"{}\",DEFAULT={def},AUTOSELECT={auto},URI=\"{}\"",
                escape_m3u8_quoted(&ag.group_id),
                escape_m3u8_quoted(&ag.name),
                escape_m3u8_quoted(&ag.language),
                ag.channels.max(1),
                escape_m3u8_quoted(&ag.uri),
            );
        }
    }

    out.push('\n');
    for r in renditions {
        let bw = r.bandwidth_bps.max(1);
        let avg = r.average_bandwidth_bps.max(1);
        let w = r.width.max(1);
        let h = r.height.max(1);
        let fps = if r.frame_rate_fps.is_finite() && r.frame_rate_fps > 0.0 {
            r.frame_rate_fps
        } else {
            29.97
        };
        let _ = write!(
            out,
            "#EXT-X-STREAM-INF:BANDWIDTH={bw},AVERAGE-BANDWIDTH={avg},RESOLUTION={w}x{h},FRAME-RATE={fps:.3},CODECS=\"{}\"",
            escape_m3u8_quoted(&r.codecs),
        );
        if let Some(ag) = &r.audio_group_id {
            let _ = write!(out, ",AUDIO=\"{}\"", escape_m3u8_quoted(ag));
        }
        out.push('\n');
        let _ = writeln!(out, "{}", r.playlist_uri.trim());
    }

    out
}

/// Build an RFC 8216 HLS Media Playlist (`prog_index.m3u8`) for a single rendition's segments.
pub fn build_hls_media_playlist(
    target_duration_sec: u32,
    media_sequence: u64,
    segments: &[HlsSegment],
    end_list: bool,
) -> String {
    let max_seg = segments
        .iter()
        .map(|s| s.duration_sec.ceil() as u32)
        .max()
        .unwrap_or(target_duration_sec)
        .max(target_duration_sec)
        .max(1);

    let mut out = String::with_capacity(1024);
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:7\n");
    out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    let _ = writeln!(out, "#EXT-X-TARGETDURATION:{max_seg}");
    let _ = writeln!(out, "#EXT-X-MEDIA-SEQUENCE:{media_sequence}");

    for seg in segments {
        let dur = if seg.duration_sec.is_finite() && seg.duration_sec > 0.0 {
            seg.duration_sec
        } else {
            f64::from(target_duration_sec.max(1))
        };
        let _ = writeln!(out, "#EXTINF:{dur:.6},{}", seg.title);
        if let Some((len, offset)) = seg.byte_range {
            let _ = writeln!(out, "#EXT-X-BYTERANGE:{len}@{offset}");
        }
        let _ = writeln!(out, "{}", seg.uri.trim());
    }

    if end_list {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    out
}

// ============================================================================
// 2. IMF (SMPTE ST 2067-2 / ST 2067-3 / ST 429-8 / ST 429-9) Package Generator
// ============================================================================

/// Track essence role inside an IMF Composition Playlist (`CPL.xml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImfTrackKind {
    MainImage,
    MainAudio,
    Subtitles,
}

impl ImfTrackKind {
    pub fn cpl_sequence_tag(self) -> &'static str {
        match self {
            Self::MainImage => "cc:MainImageSequence",
            Self::MainAudio => "cc:MainAudioSequence",
            Self::Subtitles => "cc:SubtitlesSequence",
        }
    }

    pub fn pkl_mime_type(self) -> &'static str {
        match self {
            Self::MainImage | Self::MainAudio => "application/mxf",
            Self::Subtitles => "application/ttml+xml",
        }
    }
}

/// A single MXF or IMSC1 track asset referenced by an [`ImfPackageSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImfAssetTrack {
    /// UUID string (without `urn:uuid:` prefix, e.g. `11111111-2222-3333-4444-555555555555`).
    pub uuid: String,
    /// Relative path inside the IMP folder (e.g. `VIDEO_01.mxf`).
    pub file_path: String,
    pub kind: ImfTrackKind,
    /// Edit rate `(numerator, denominator)` (e.g. `(24000, 1001)` or `(48000, 1)`).
    pub edit_rate: (u32, u32),
    pub intrinsic_duration: u64,
    pub entry_point: u64,
    pub source_duration: u64,
    /// Base64 SHA-1 digest of the essence file for `<PackingList>`.
    pub hash_base64: String,
    /// File size in bytes for `<PackingList>`.
    pub byte_size: u64,
}

/// Specification for generating an IMF Interoperable Master Package (IMP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImfPackageSpec {
    pub cpl_uuid: String,
    pub pkl_uuid: String,
    pub assetmap_uuid: String,
    pub annotation: String,
    pub issuer: String,
    pub creator: String,
    pub content_title: String,
    /// ContentKind (e.g. `feature`, `episode`, `trailer`, `advertisement`).
    pub content_kind: String,
    pub edit_rate: (u32, u32),
    pub tracks: Vec<ImfAssetTrack>,
}

/// Generated XML files constituting an IMF Interoperable Master Package (IMP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImfPackageXmls {
    pub cpl_xml: String,
    pub pkl_xml: String,
    pub assetmap_xml: String,
    pub volindex_xml: String,
}

/// Build the 4 SMPTE IMF XML descriptors (`CPL.xml`, `PKL.xml`, `ASSETMAP.xml`, `VOLINDEX.xml`)
/// from `spec`.
pub fn build_imf_package_xmls(spec: &ImfPackageSpec) -> ImfPackageXmls {
    let cpl_xml = build_imf_cpl(spec);
    let pkl_xml = build_imf_pkl(spec, cpl_xml.len() as u64);
    let assetmap_xml = build_imf_assetmap(spec);
    let volindex_xml = build_imf_volindex();
    ImfPackageXmls {
        cpl_xml,
        pkl_xml,
        assetmap_xml,
        volindex_xml,
    }
}

fn build_imf_cpl(spec: &ImfPackageSpec) -> String {
    let mut xml = String::with_capacity(2048);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<CompositionPlaylist xmlns=\"http://www.smpte-ra.org/schemas/2067-3/2016\" xmlns:cc=\"http://www.smpte-ra.org/schemas/2067-2/2016\">\n");
    let _ = writeln!(xml, "  <Id>urn:uuid:{}</Id>", escape_xml(&spec.cpl_uuid));
    let _ = writeln!(
        xml,
        "  <Annotation>{}</Annotation>",
        escape_xml(&spec.annotation)
    );
    xml.push_str("  <IssueDate>2026-01-01T00:00:00Z</IssueDate>\n");
    let _ = writeln!(xml, "  <Issuer>{}</Issuer>", escape_xml(&spec.issuer));
    let _ = writeln!(xml, "  <Creator>{}</Creator>", escape_xml(&spec.creator));
    let _ = writeln!(
        xml,
        "  <ContentTitle>{}</ContentTitle>",
        escape_xml(&spec.content_title)
    );
    let _ = writeln!(
        xml,
        "  <ContentKind>{}</ContentKind>",
        escape_xml(&spec.content_kind)
    );
    let _ = writeln!(
        xml,
        "  <EditRate>{} {}</EditRate>",
        spec.edit_rate.0.max(1),
        spec.edit_rate.1.max(1)
    );
    xml.push_str("  <SegmentList>\n");
    xml.push_str("    <Segment>\n");
    let _ = writeln!(
        xml,
        "      <Id>urn:uuid:{}-seg1</Id>",
        escape_xml(&spec.cpl_uuid)
    );
    xml.push_str("      <SequenceList>\n");

    for (idx, tr) in spec.tracks.iter().enumerate() {
        let tag = tr.kind.cpl_sequence_tag();
        let _ = writeln!(xml, "        <{tag}>");
        let _ = writeln!(
            xml,
            "          <Id>urn:uuid:{}-seq{}</Id>",
            escape_xml(&tr.uuid),
            idx + 1
        );
        let _ = writeln!(
            xml,
            "          <TrackId>urn:uuid:{}</TrackId>",
            escape_xml(&tr.uuid)
        );
        xml.push_str("          <ResourceList>\n");
        xml.push_str("            <Resource xsi:type=\"TrackFileResourceType\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n");
        let _ = writeln!(
            xml,
            "              <Id>urn:uuid:{}-res</Id>",
            escape_xml(&tr.uuid)
        );
        let _ = writeln!(
            xml,
            "              <EditRate>{} {}</EditRate>",
            tr.edit_rate.0.max(1),
            tr.edit_rate.1.max(1)
        );
        let _ = writeln!(
            xml,
            "              <IntrinsicDuration>{}</IntrinsicDuration>",
            tr.intrinsic_duration
        );
        let _ = writeln!(xml, "              <EntryPoint>{}</EntryPoint>", tr.entry_point);
        let _ = writeln!(
            xml,
            "              <SourceDuration>{}</SourceDuration>",
            tr.source_duration
        );
        let _ = writeln!(
            xml,
            "              <TrackFileId>urn:uuid:{}</TrackFileId>",
            escape_xml(&tr.uuid)
        );
        xml.push_str("            </Resource>\n");
        xml.push_str("          </ResourceList>\n");
        let _ = writeln!(xml, "        </{tag}>");
    }

    xml.push_str("      </SequenceList>\n");
    xml.push_str("    </Segment>\n");
    xml.push_str("  </SegmentList>\n");
    xml.push_str("</CompositionPlaylist>\n");
    xml
}

fn build_imf_pkl(spec: &ImfPackageSpec, cpl_size: u64) -> String {
    let mut xml = String::with_capacity(1536);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<PackingList xmlns=\"http://www.smpte-ra.org/schemas/429-8/2007/PKL\">\n");
    let _ = writeln!(xml, "  <Id>urn:uuid:{}</Id>", escape_xml(&spec.pkl_uuid));
    let _ = writeln!(
        xml,
        "  <AnnotationText>{}</AnnotationText>",
        escape_xml(&spec.annotation)
    );
    xml.push_str("  <IssueDate>2026-01-01T00:00:00Z</IssueDate>\n");
    let _ = writeln!(xml, "  <Issuer>{}</Issuer>", escape_xml(&spec.issuer));
    let _ = writeln!(xml, "  <Creator>{}</Creator>", escape_xml(&spec.creator));
    xml.push_str("  <AssetList>\n");

    for tr in &spec.tracks {
        xml.push_str("    <Asset>\n");
        let _ = writeln!(xml, "      <Id>urn:uuid:{}</Id>", escape_xml(&tr.uuid));
        let _ = writeln!(
            xml,
            "      <Hash>{}</Hash>",
            escape_xml(&tr.hash_base64)
        );
        let _ = writeln!(xml, "      <Size>{}</Size>", tr.byte_size.max(1));
        let _ = writeln!(xml, "      <Type>{}</Type>", tr.kind.pkl_mime_type());
        let _ = writeln!(
            xml,
            "      <OriginalFileName>{}</OriginalFileName>",
            escape_xml(&tr.file_path)
        );
        xml.push_str("    </Asset>\n");
    }

    xml.push_str("    <Asset>\n");
    let _ = writeln!(
        xml,
        "      <Id>urn:uuid:{}</Id>",
        escape_xml(&spec.cpl_uuid)
    );
    xml.push_str("      <Hash>AAAAAAAAAAAAAAAAAAAAAAAAAAA=</Hash>\n");
    let _ = writeln!(xml, "      <Size>{}</Size>", cpl_size.max(1));
    xml.push_str("      <Type>text/xml</Type>\n");
    let _ = writeln!(
        xml,
        "      <OriginalFileName>CPL_{}.xml</OriginalFileName>",
        escape_xml(&spec.cpl_uuid)
    );
    xml.push_str("    </Asset>\n");
    xml.push_str("  </AssetList>\n");
    xml.push_str("</PackingList>\n");
    xml
}

fn build_imf_assetmap(spec: &ImfPackageSpec) -> String {
    let mut xml = String::with_capacity(1536);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<AssetMap xmlns=\"http://www.smpte-ra.org/schemas/429-9/2007/AM\">\n");
    let _ = writeln!(
        xml,
        "  <Id>urn:uuid:{}</Id>",
        escape_xml(&spec.assetmap_uuid)
    );
    let _ = writeln!(xml, "  <Creator>{}</Creator>", escape_xml(&spec.creator));
    xml.push_str("  <VolumeCount>1</VolumeCount>\n");
    xml.push_str("  <IssueDate>2026-01-01T00:00:00Z</IssueDate>\n");
    let _ = writeln!(xml, "  <Issuer>{}</Issuer>", escape_xml(&spec.issuer));
    xml.push_str("  <AssetList>\n");

    let cpl_file = format!("CPL_{}.xml", spec.cpl_uuid);
    write_assetmap_entry(&mut xml, &spec.cpl_uuid, &cpl_file, false);

    let pkl_file = format!("PKL_{}.xml", spec.pkl_uuid);
    write_assetmap_entry(&mut xml, &spec.pkl_uuid, &pkl_file, true);

    for tr in &spec.tracks {
        write_assetmap_entry(&mut xml, &tr.uuid, &tr.file_path, false);
    }

    xml.push_str("  </AssetList>\n");
    xml.push_str("</AssetMap>\n");
    xml
}

fn write_assetmap_entry(xml: &mut String, uuid: &str, path: &str, is_packing_list: bool) {
    xml.push_str("    <Asset>\n");
    let _ = writeln!(xml, "      <Id>urn:uuid:{}</Id>", escape_xml(uuid));
    if is_packing_list {
        xml.push_str("      <PackingList>true</PackingList>\n");
    }
    xml.push_str("      <ChunkList>\n");
    xml.push_str("        <Chunk>\n");
    let _ = writeln!(xml, "          <Path>{}</Path>", escape_xml(path));
    xml.push_str("          <VolumeIndex>1</VolumeIndex>\n");
    xml.push_str("        </Chunk>\n");
    xml.push_str("      </ChunkList>\n");
    xml.push_str("    </Asset>\n");
}

fn build_imf_volindex() -> String {
    concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<VolumeIndex xmlns=\"http://www.smpte-ra.org/schemas/429-9/2007/AM\">\n",
        "  <Index>1</Index>\n",
        "</VolumeIndex>\n"
    )
    .to_string()
}

// ============================================================================
// 3. Compressor-Style Destination Bundles & Watch Folders
// ============================================================================

/// Automated post-encode action attached to a [`DestinationOutput`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PostExportAction {
    #[default]
    SaveOnly,
    /// Generate an HLS `.m3u8` media playlist alongside the encoded file.
    GenerateHlsManifest { segment_duration_sec: u32 },
    /// Generate SMPTE ST 2067-2 IMF `CPL.xml` / `PKL.xml` / `ASSETMAP.xml` alongside MXF outputs.
    GenerateImfPackage {
        issuer: String,
        content_kind: String,
    },
    /// Move or copy the rendered output into an archive directory after verification.
    ArchiveCopy { archive_dir: String },
}

/// One target encoding inside a [`DestinationBundle`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DestinationOutput {
    pub name: String,
    pub settings: ExportSettings,
    /// Filename pattern supporting `{sequence}`, `{format}`, `{width}`, `{height}` tokens.
    pub filename_template: String,
    pub post_action: PostExportAction,
}

impl DestinationOutput {
    /// Expand `{sequence}`, `{format}`, `{width}`, `{height}` tokens in `filename_template`.
    pub fn expand_filename(&self, sequence_name: &str) -> String {
        let safe_seq: String = sequence_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let (w, h) = self.settings.frame_size.unwrap_or((1920, 1080));
        self.filename_template
            .replace("{sequence}", &safe_seq)
            .replace("{format}", self.settings.format.id())
            .replace("{width}", &w.to_string())
            .replace("{height}", &h.to_string())
    }
}

/// Apple Compressor-style multi-output [`DestinationBundle`] that encodes a single timeline into
/// multiple delivery formats (e.g. ProRes 422 HQ Archival Master + H.264 1080p Web + WAV Audio Stem)
/// in one batch submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DestinationBundle {
    pub id: String,
    pub name: String,
    pub outputs: Vec<DestinationOutput>,
}

impl DestinationBundle {
    /// Built-in bundle: **ProRes 422 HQ Archival Master + Web H.264 1080p + Broadcast WAV Stem**.
    pub fn broadcast_and_web_bundle() -> Self {
        let prores = ExportSettings {
            format: Format::ProRes,
            frame_size: Some((3840, 2160)),
            ..ExportSettings::default()
        };
        let web_h264 = ExportSettings {
            format: Format::H264,
            frame_size: Some((1920, 1080)),
            ..ExportSettings::default()
        };
        let wav_stem = ExportSettings {
            format: Format::Wav,
            ..ExportSettings::default()
        };
        Self {
            id: "broadcast_and_web".into(),
            name: "ProRes 4K Master + Web 1080p H.264 + WAV Stem".into(),
            outputs: vec![
                DestinationOutput {
                    name: "ProRes 422 HQ 4K Master".into(),
                    settings: prores,
                    filename_template: "{sequence}_{width}x{height}_master.mov".into(),
                    post_action: PostExportAction::SaveOnly,
                },
                DestinationOutput {
                    name: "Web 1080p H.264".into(),
                    settings: web_h264,
                    filename_template: "{sequence}_{width}x{height}_web.mp4".into(),
                    post_action: PostExportAction::GenerateHlsManifest {
                        segment_duration_sec: 6,
                    },
                },
                DestinationOutput {
                    name: "Full Mix Stereo WAV".into(),
                    settings: wav_stem,
                    filename_template: "{sequence}_mix.wav".into(),
                    post_action: PostExportAction::SaveOnly,
                },
            ],
        }
    }
}

/// Automated Watch Folder rule that triggers a [`DestinationBundle`] whenever a matching media file
/// lands in `watch_dir`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchFolderRule {
    pub watch_dir: String,
    pub output_dir: String,
    /// Accepted lowercase file extensions without leading dot (e.g. `["mov", "mp4", "mxf"]`).
    pub extensions: Vec<String>,
    pub bundle: DestinationBundle,
}

impl WatchFolderRule {
    /// Returns `true` if `candidate_path` lies inside `self.watch_dir` and has an accepted extension.
    pub fn matches_path(&self, candidate_path: &str) -> bool {
        let norm_watch = self.watch_dir.trim_end_matches('/');
        if !candidate_path.starts_with(norm_watch) {
            return false;
        }
        let Some((_, ext)) = candidate_path.rsplit_once('.') else {
            return false;
        };
        self.extensions
            .iter()
            .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(ext))
    }
}

fn escape_m3u8_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_hls_master_and_media_playlists_rfc8216() {
        let ladder = HlsRendition::standard_abr_ladder(23.976, Some("audio-stereo"));
        let audio = vec![HlsAudioGroup {
            group_id: "audio-stereo".into(),
            name: "English Stereo".into(),
            language: "en".into(),
            channels: 2,
            uri: "audio_en/prog_index.m3u8".into(),
            is_default: true,
            autoselect: true,
        }];
        let master = build_hls_master_playlist(&ladder, &audio);
        assert!(master.starts_with("#EXTM3U\n#EXT-X-VERSION:7\n"));
        assert!(master.contains("#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-stereo\""));
        assert!(master.contains("RESOLUTION=1920x1080,FRAME-RATE=23.976,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"audio-stereo\""));
        assert!(master.contains("RESOLUTION=3840x2160,FRAME-RATE=23.976,CODECS=\"hvc1.2.4.L150.B0,mp4a.40.2\""));

        let segments = vec![
            HlsSegment {
                duration_sec: 6.006,
                uri: "seg_00000.m4s".into(),
                title: "Segment 0".into(),
                byte_range: None,
            },
            HlsSegment {
                duration_sec: 4.5045,
                uri: "seg_00001.m4s".into(),
                title: "Segment 1".into(),
                byte_range: Some((524_288, 0)),
            },
        ];
        let media = build_hls_media_playlist(6, 0, &segments, true);
        assert!(media.contains("#EXT-X-TARGETDURATION:7"));
        assert!(media.contains("#EXTINF:6.006000,Segment 0"));
        assert!(media.contains("#EXT-X-BYTERANGE:524288@0"));
        assert!(media.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn builds_imf_smpte_st2067_package_xmls_and_destination_bundle() {
        let spec = ImfPackageSpec {
            cpl_uuid: "11111111-2222-3333-4444-555555555555".into(),
            pkl_uuid: "22222222-3333-4444-5555-666666666666".into(),
            assetmap_uuid: "33333333-4444-5555-6666-777777777777".into(),
            annotation: "Feature Master IMP".into(),
            issuer: "ArtCraft Studios".into(),
            creator: "FilmCraft 0.17".into(),
            content_title: "THE_GREAT_ESCAPE_IMP".into(),
            content_kind: "feature".into(),
            edit_rate: (24000, 1001),
            tracks: vec![
                ImfAssetTrack {
                    uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    file_path: "VIDEO_01.mxf".into(),
                    kind: ImfTrackKind::MainImage,
                    edit_rate: (24000, 1001),
                    intrinsic_duration: 2400,
                    entry_point: 0,
                    source_duration: 2400,
                    hash_base64: "dGhlX3ZpZGVvX2hhc2hfc2hhMQ==".into(),
                    byte_size: 104_857_600,
                },
                ImfAssetTrack {
                    uuid: "bbbbbbbb-cccc-dddd-eeee-ffffffffffff".into(),
                    file_path: "AUDIO_01.mxf".into(),
                    kind: ImfTrackKind::MainAudio,
                    edit_rate: (48000, 1),
                    intrinsic_duration: 4_804_800,
                    entry_point: 0,
                    source_duration: 4_804_800,
                    hash_base64: "dGhlX2F1ZGlvX2hhc2hfc2hhMQ==".into(),
                    byte_size: 28_828_800,
                },
            ],
        };

        let imp = build_imf_package_xmls(&spec);
        assert!(imp.cpl_xml.contains("<CompositionPlaylist"));
        assert!(imp.cpl_xml.contains("<cc:MainImageSequence>"));
        assert!(imp.cpl_xml.contains("<cc:MainAudioSequence>"));
        assert!(imp.pkl_xml.contains("<PackingList"));
        assert!(imp.pkl_xml.contains("<OriginalFileName>VIDEO_01.mxf</OriginalFileName>"));
        assert!(imp.assetmap_xml.contains("<AssetMap"));
        assert!(imp.assetmap_xml.contains("<PackingList>true</PackingList>"));
        assert!(imp.volindex_xml.contains("<VolumeIndex"));

        let bundle = DestinationBundle::broadcast_and_web_bundle();
        assert_eq!(bundle.outputs.len(), 3);
        assert_eq!(
            bundle.outputs[0].expand_filename("Reel 1: Final"),
            "Reel_1__Final_3840x2160_master.mov"
        );

        let watch = WatchFolderRule {
            watch_dir: "/var/watch/ingest".into(),
            output_dir: "/var/watch/out".into(),
            extensions: vec!["mov".into(), "mxf".into()],
            bundle,
        };
        assert!(watch.matches_path("/var/watch/ingest/camera_a.MOV"));
        assert!(!watch.matches_path("/var/watch/ingest/notes.txt"));
    }
}
