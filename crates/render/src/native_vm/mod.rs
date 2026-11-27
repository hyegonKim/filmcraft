//! Memory-safe, zero-JIT cross-platform C++ binary plugin loader, dual-ISA (`x86_64` + `AArch64`)
//! user-mode CPU emulator, and Adobe Host SDK runtime for `.8bf`, `.prm`, `.aex`, and `.aip`
//! compiled C++ plugins across **iOS, Android, macOS, Linux, Windows, and WebAssembly**.

pub mod aarch64;
pub mod crt;
pub mod host_8bf;
pub mod host_ae_prm;
pub mod host_aip;
pub mod macho;
pub mod mem;
pub mod pe;
pub mod pipl;
pub mod x86_64;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use host_8bf::{Filter8bfHost, ImageBuffer8bf, PixelDepth8bf};
pub use host_ae_prm::{AeParamKind, AeParamSpec, AePrmHost, RgbaFrame};
pub use host_aip::{AipBezierSegment, AipHost, AipSubpath};
pub use pipl::PiplMetadata;

/// Errors returned by the cross-platform C++ binary plugin loader and VM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginHostError {
    InvalidBinary(String),
    UnsupportedArch(String),
    MissingEntrypoint(String),
    MemoryFault(String),
    CpuTrap(String),
    Limit(String),
    Abi(String),
    PluginError(i32),
}

impl std::fmt::Display for PluginHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBinary(msg) => write!(f, "invalid binary plugin image: {msg}"),
            Self::UnsupportedArch(msg) => write!(f, "unsupported binary architecture: {msg}"),
            Self::MissingEntrypoint(msg) => write!(f, "plugin entrypoint not found: {msg}"),
            Self::MemoryFault(msg) => write!(f, "guest memory fault: {msg}"),
            Self::CpuTrap(msg) => write!(f, "CPU trap: {msg}"),
            Self::Limit(msg) => write!(f, "resource limit exceeded: {msg}"),
            Self::Abi(msg) => write!(f, "host SDK ABI error: {msg}"),
            Self::PluginError(code) => write!(f, "plugin returned error code {code}"),
        }
    }
}

impl std::error::Error for PluginHostError {}

pub type Result<T> = std::result::Result<T, PluginHostError>;

/// Resource caps enforced on untrusted third-party C++ binary plugins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_binary_bytes: usize,
    pub max_memory_bytes: usize,
    pub instruction_budget: u64,
    pub max_call_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_binary_bytes: 64 << 20,  // 64 MiB binary cap
            max_memory_bytes: 256 << 20, // 256 MiB guest address space cap
            instruction_budget: 25_000_000,
            max_call_depth: 128,
        }
    }
}

/// CPU instruction set of the compiled C++ binary plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestArch {
    X86_64,
    AArch64,
}

/// Original target OS ABI of the compiled C++ binary plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestOs {
    Windows,
    MacOs,
}

/// Executable container format of the C++ binary plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryFormat {
    Pe64,
    MachO64,
}

/// Adobe third-party C++ binary plugin family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginFormat {
    Photoshop8bf,
    PremierePrm,
    AfterEffectsAex,
    IllustratorAip,
}

impl PluginFormat {
    pub fn from_extension(path_or_name: &str) -> Option<Self> {
        let lower = path_or_name.trim().to_ascii_lowercase();
        if lower.ends_with(".8bf") {
            Some(Self::Photoshop8bf)
        } else if lower.ends_with(".prm") {
            Some(Self::PremierePrm)
        } else if lower.ends_with(".aex") {
            Some(Self::AfterEffectsAex)
        } else if lower.ends_with(".aip") {
            Some(Self::IllustratorAip)
        } else {
            None
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Photoshop8bf => "8bf",
            Self::PremierePrm => "prm",
            Self::AfterEffectsAex => "aex",
            Self::IllustratorAip => "aip",
        }
    }
}

/// One imported dynamic symbol slot (`IAT` entry on PE32+, `__got` / `__stubs` on Mach-O).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportSymbol {
    pub library: String,
    pub name: String,
    pub slot_addr: u64,
}

/// Mapped binary image summary inside guest memory.
#[derive(Clone, Debug)]
pub struct LoadedImageInfo {
    pub format: BinaryFormat,
    pub arch: GuestArch,
    pub os: GuestOs,
    pub image_base: u64,
    pub dll_entry: Option<u64>,
    pub exports: BTreeMap<String, u64>,
    pub imports: Vec<ImportSymbol>,
}

/// Static inspection summary for any `.8bf`, `.prm`, `.aex`, or `.aip` binary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDescriptor {
    pub plugin_format: PluginFormat,
    pub binary_format: BinaryFormat,
    pub arch: GuestArch,
    pub guest_os: GuestOs,
    pub entrypoint_symbol: String,
    pub entrypoint_addr: u64,
    pub pipl: PiplMetadata,
    pub exported_symbols: Vec<String>,
}

/// Inspects a compiled `.8bf`, `.prm`, `.aex`, or `.aip` binary on any platform without executing it.
pub fn inspect_plugin_binary(bytes: &[u8], file_hint: Option<&str>) -> Result<PluginDescriptor> {
    if bytes.len() > Limits::default().max_binary_bytes {
        return Err(PluginHostError::Limit(format!("binary size {} exceeds limit", bytes.len())));
    }
    let mut mem = mem::PagedMemory::new(64 << 20);
    let (info, rsrc_blobs) = load_any_image(bytes, &mut mem)?;
    let pipl = pipl::parse_pipl_from_blobs(&rsrc_blobs, bytes);
    let hint_fmt = file_hint.and_then(PluginFormat::from_extension);
    let plugin_format = pipl.format_hint.or(hint_fmt).unwrap_or_else(|| infer_format_from_exports(&info.exports));
    let (ep_name, ep_addr) = resolve_plugin_entrypoint(&info, &pipl, plugin_format)?;
    Ok(PluginDescriptor {
        plugin_format,
        binary_format: info.format,
        arch: info.arch,
        guest_os: info.os,
        entrypoint_symbol: ep_name,
        entrypoint_addr: ep_addr,
        pipl,
        exported_symbols: info.exports.keys().cloned().collect(),
    })
}

pub(crate) fn load_any_image<'a>(bytes: &'a [u8], mem: &mut mem::PagedMemory) -> Result<(LoadedImageInfo, Vec<&'a [u8]>)> {
    if pe::is_pe_binary(bytes) {
        pe::load_pe64(bytes, mem)
    } else if macho::is_macho_or_fat(bytes) {
        macho::load_macho64(bytes, mem)
    } else {
        Err(PluginHostError::InvalidBinary("expected 64-bit PE32+ (.8bf/.prm/.aex/.aip) or Mach-O/Universal (.plugin/.bundle) binary".into()))
    }
}

fn infer_format_from_exports(exports: &BTreeMap<String, u64>) -> PluginFormat {
    if exports.contains_key("EntryPointFunc") || exports.contains_key("EffectMain") || exports.contains_key("MainEntry") {
        PluginFormat::AfterEffectsAex
    } else if exports.contains_key("xFilter") || exports.contains_key("xGPUFilterEntry") {
        PluginFormat::PremierePrm
    } else {
        PluginFormat::Photoshop8bf
    }
}

pub(crate) fn resolve_plugin_entrypoint(info: &LoadedImageInfo, pipl: &PiplMetadata, format: PluginFormat) -> Result<(String, u64)> {
    let mut candidates: Vec<&str> = Vec::new();
    match (info.os, info.arch) {
        (GuestOs::Windows, _) => {
            if let Some(s) = pipl.entry_win64.as_deref() {
                candidates.push(s);
            }
        }
        (GuestOs::MacOs, GuestArch::AArch64) => {
            if let Some(s) = pipl.entry_mac_arm64.as_deref() {
                candidates.push(s);
            }
            if let Some(s) = pipl.entry_mac_x86_64.as_deref() {
                candidates.push(s);
            }
        }
        (GuestOs::MacOs, GuestArch::X86_64) => {
            if let Some(s) = pipl.entry_mac_x86_64.as_deref() {
                candidates.push(s);
            }
            if let Some(s) = pipl.entry_mac_arm64.as_deref() {
                candidates.push(s);
            }
        }
    }
    let defaults: &[&str] = match format {
        PluginFormat::Photoshop8bf => &["PluginMain", "_PluginMain", "FilterMain"],
        PluginFormat::PremierePrm => &["EffectMain", "EntryPointFunc", "MainEntry", "xFilter", "PluginMain"],
        PluginFormat::AfterEffectsAex => &["EntryPointFunc", "EffectMain", "MainEntry", "PluginMain"],
        PluginFormat::IllustratorAip => &["PluginMain", "_PluginMain", "Main"],
    };
    candidates.extend_from_slice(defaults);
    for cand in candidates {
        if let Some(&addr) = info.exports.get(cand) {
            return Ok((cand.to_string(), addr));
        }
        let underscored = format!("_{cand}");
        if let Some(&addr) = info.exports.get(&underscored) {
            return Ok((cand.to_string(), addr));
        }
    }
    if let Some((first_name, &first_addr)) = info.exports.iter().find(|(k, _)| !k.starts_with("PluginDataEntry") && !k.starts_with("NvOptimus")) {
        return Ok((first_name.clone(), first_addr));
    }
    Err(PluginHostError::MissingEntrypoint(format!("no entrypoint found for {format:?}")))
}
