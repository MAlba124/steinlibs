//! Finding the one Vulkan driver a device needs, so the loader does not
//! load them all.
//!
//! ```text
//! dev_t ──/sys/dev/char──► kernel driver (i915) ──table──► manifest prefix (intel_icd)
//!       ──icd.d dirs──► manifest json ──library_path──► libvulkan_intel.so
//! ```
//!
//! Loading every installed driver costs about 117 ms on the laptop this was
//! written on, 99 of them NVIDIA's; the Intel driver alone takes 13. The
//! vk module dlopens what this finds and hands it to the loader through
//! VK_LUNARG_direct_driver_loading. Anything missing here (an unknown
//! kernel driver, no manifest, an override in the environment) returns no
//! candidates and the loader scans as usual.

use std::path::{Path, PathBuf};

/// Library candidates for the device with DRM node `major:minor`, primary
/// or render. Empty means scan every driver as usual.
pub(crate) fn candidates(major: u32, minor: u32) -> Vec<PathBuf> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let Some(dirs) = manifest_dirs(env) else {
        return Vec::new();
    };
    let Some(prefix) = kernel_driver(major, minor).and_then(|k| manifest_prefix(&k)) else {
        return Vec::new();
    };
    dirs.iter().flat_map(|d| libraries_in(d, prefix)).collect()
}

/// The kernel driver bound to a DRM node's device, `i915` for example.
fn kernel_driver(major: u32, minor: u32) -> Option<String> {
    let link = std::fs::read_link(format!("/sys/dev/char/{major}:{minor}/device/driver")).ok()?;
    Some(link.file_name()?.to_str()?.to_owned())
}

/// The manifest file name prefix of the Vulkan driver for a kernel driver.
fn manifest_prefix(kernel: &str) -> Option<&'static str> {
    Some(match kernel {
        "i915" | "xe" => "intel_icd",
        "amdgpu" | "radeon" => "radeon_icd",
        "nouveau" => "nouveau_icd",
        "nvidia" => "nvidia_icd",
        "virtio_gpu" | "virtio-pci" => "virtio_icd",
        "msm" => "freedreno_icd",
        "panfrost" | "panthor" => "panfrost_icd",
        "asahi" => "asahi_icd",
        "v3d" | "vc4" => "broadcom_icd",
        "powervr" => "powervr_mesa_icd",
        _ => return None,
    })
}

/// Where the loader looks for driver manifests, in its order. None when the
/// environment overrides the search, which this does not try to replicate.
fn manifest_dirs(env: impl Fn(&str) -> Option<String>) -> Option<Vec<PathBuf>> {
    let overrides = ["VK_DRIVER_FILES", "VK_ICD_FILENAMES", "VK_ADD_DRIVER_FILES", "VK_LOADER_DRIVERS_SELECT"];
    if overrides.iter().any(|k| env(k).is_some()) {
        return None;
    }
    let config = env("XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".into());
    let data = env("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let home = env("XDG_DATA_HOME").or_else(|| env("HOME").map(|h| format!("{h}/.local/share")));
    let mut roots: Vec<String> = config.split(':').map(str::to_owned).collect();
    roots.push("/etc".into());
    roots.extend(home);
    roots.extend(data.split(':').map(str::to_owned));
    // NixOS puts the drivers of the running system here and patches its
    // loader to look.
    roots.push("/run/opengl-driver/share".into());
    let dirs = roots
        .into_iter()
        .filter(|r| !r.is_empty())
        .map(|r| Path::new(&r).join("vulkan/icd.d"))
        .collect();
    Some(dirs)
}

/// The libraries named by manifests in `dir` whose file names start with
/// `prefix`, 64 bit ones first.
fn libraries_in(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with(prefix) && name.ends_with(".json")
        })
        .collect();
    // i686 manifests would only fail to dlopen, but try x86_64 first.
    names.sort_by_key(|p| !p.to_string_lossy().contains("x86_64"));
    names
        .iter()
        .filter_map(|m| {
            let json = std::fs::read_to_string(m).ok()?;
            resolve(m, &library_path(&json)?)
        })
        .collect()
}

/// A manifest's library path: absolute as is, relative to the manifest's
/// directory when it has a slash, else a bare name for dlopen to search.
fn resolve(manifest: &Path, lib: &str) -> Option<PathBuf> {
    let p = Path::new(lib);
    if p.is_absolute() || !lib.contains('/') {
        return Some(p.to_path_buf());
    }
    Some(manifest.parent()?.join(p))
}

/// The `library_path` string of a driver manifest. Not a JSON parser: it
/// finds the key and reads the string after it, with the escapes paths use.
fn library_path(json: &str) -> Option<String> {
    let rest = &json[json.find("\"library_path\"")? + "\"library_path\"".len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            c => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests;
