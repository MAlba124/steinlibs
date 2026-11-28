//! The checked in SPIR-V matches its GLSL. Recompiles with the flake's
//! glslang when it is on PATH and skips otherwise.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::process::Command;

const SHADERS: [&str; 4] = ["quad.vert", "quad.frag", "fill.comp", "volume.comp"];

#[test]
fn checked_in_spirv_is_current() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders");
    let tmp = std::env::temp_dir().join(format!("stein-gpu-spv-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    for name in SHADERS {
        let out = tmp.join(format!("{name}.spv"));
        let run = Command::new("glslangValidator")
            .args(["-V", "--target-env", "vulkan1.3", "-o"])
            .arg(&out)
            .arg(dir.join(name))
            .output();
        let Ok(run) = run else {
            eprintln!("skipped: glslangValidator not on PATH, run inside `nix develop`");
            return;
        };
        assert!(
            run.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&run.stdout)
        );
        let fresh = std::fs::read(&out).unwrap();
        let kept = std::fs::read(dir.join("spv").join(format!("{name}.spv"))).unwrap();
        assert!(
            fresh == kept,
            "shaders/spv/{name}.spv is stale, rebuild it with: glslangValidator -V --target-env vulkan1.3 -o spv/{name}.spv {name}"
        );
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
