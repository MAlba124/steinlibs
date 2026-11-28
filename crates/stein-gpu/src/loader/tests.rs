use super::*;

#[test]
fn the_library_path_is_read_with_its_escapes() {
    let json = r#"{ "ICD": { "api_version": "1.4", "library_path" : "/nix/store/x\/lib/libvulkan_intel.so" } }"#;
    assert_eq!(library_path(json).as_deref(), Some("/nix/store/x/lib/libvulkan_intel.so"));
    assert_eq!(library_path(r#"{"library_path":"a\"b"}"#).as_deref(), Some("a\"b"));
    assert_eq!(library_path(r#"{"library_path": 7}"#), None);
    assert_eq!(library_path(r#"{"library_path": "unterminated"#), None);
    assert_eq!(library_path("{}"), None);
}

#[test]
fn relative_paths_resolve_against_the_manifest() {
    let m = Path::new("/usr/share/vulkan/icd.d/intel_icd.x86_64.json");
    assert_eq!(resolve(m, "/lib/a.so"), Some(PathBuf::from("/lib/a.so")));
    assert_eq!(resolve(m, "libvulkan_intel.so"), Some(PathBuf::from("libvulkan_intel.so")));
    assert_eq!(
        resolve(m, "../../../lib/libvulkan_intel.so"),
        Some(PathBuf::from("/usr/share/vulkan/icd.d/../../../lib/libvulkan_intel.so"))
    );
}

#[test]
fn kernel_drivers_map_to_their_vulkan_drivers() {
    assert_eq!(manifest_prefix("i915"), Some("intel_icd"));
    assert_eq!(manifest_prefix("xe"), Some("intel_icd"));
    assert_eq!(manifest_prefix("amdgpu"), Some("radeon_icd"));
    assert_eq!(manifest_prefix("nvidia"), Some("nvidia_icd"));
    assert_eq!(manifest_prefix("simpledrm"), None);
}

#[test]
fn search_dirs_follow_the_loader_and_overrides_turn_this_off() {
    let env = |k: &str| match k {
        "HOME" => Some("/home/u".to_owned()),
        "XDG_DATA_DIRS" => Some("/a:/b".to_owned()),
        _ => None,
    };
    let dirs = manifest_dirs(env).unwrap();
    let want = [
        "/etc/xdg/vulkan/icd.d",
        "/etc/vulkan/icd.d",
        "/home/u/.local/share/vulkan/icd.d",
        "/a/vulkan/icd.d",
        "/b/vulkan/icd.d",
        "/run/opengl-driver/share/vulkan/icd.d",
    ];
    assert_eq!(dirs, want.map(PathBuf::from));
    let overridden = |k: &str| (k == "VK_DRIVER_FILES").then(|| "/x.json".to_owned());
    assert_eq!(manifest_dirs(overridden), None);
}

#[test]
fn manifests_are_matched_by_prefix_x86_64_first() {
    let dir = std::env::temp_dir().join(format!("stein-gpu-icd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, lib: &str| {
        let json = format!(r#"{{"ICD": {{"library_path": "{lib}"}}}}"#);
        std::fs::write(dir.join(name), json).unwrap();
    };
    write("intel_icd.i686.json", "/lib32/libvulkan_intel.so");
    write("intel_icd.x86_64.json", "/lib/libvulkan_intel.so");
    write("intel_hasvk_icd.x86_64.json", "/lib/libvulkan_intel_hasvk.so");
    write("radeon_icd.x86_64.json", "/lib/libvulkan_radeon.so");
    let found = libraries_in(&dir, "intel_icd");
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        found,
        [PathBuf::from("/lib/libvulkan_intel.so"), PathBuf::from("/lib32/libvulkan_intel.so")]
    );
}
