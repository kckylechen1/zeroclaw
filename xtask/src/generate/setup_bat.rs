//! setup.bat renderer. The hand-written imperative glue (toolchain bootstrap,
//! PATH, copy, quickstart) stays in setup.bat; only the
//! drift-prone data lives in sentinel-delimited regions this renderer owns.
//! setup.bat has two such zones - the build-mode menu/routing and the per-mode

use super::spec::{self, Selection};
use std::path::Path;

/// A named generated zone, delimited by id-tagged sentinels so multiple regions
/// can coexist in one file and the splicer targets each precisely.
fn begin(id: &str) -> String {
    format!(":: >>> generated:{id} by `cargo generate installers` - do not edit <<<")
}
fn end(id: &str) -> String {
    format!(":: >>> end generated:{id} <<<")
}

const ZONE_MENU: &str = "menu";
const ZONE_PRESETS: &str = "presets";

/// Render source-build routing from the existing canonical selections. An
/// omitted mode chooses the recommended distribution without an install-method
/// prompt; bootstrap has no prebuilt branch in this fork.
fn render_menu(_manifest_dir: &Path) -> String {
    let mut out = String::new();
    out.push_str(":choose_mode\n");
    out.push_str(&format!(
        "if \"%MODE%\"==\"\" goto :build_{}\n",
        Selection::Dist.id()
    ));
    for sel in Selection::menu() {
        out.push_str(&format!(
            "if \"%MODE%\"==\"{id}\" goto :build_{id}\n",
            id = sel.id()
        ));
    }
    out.push_str("echo   %RED%Unknown source build mode.%RESET%\n");
    out.push_str("goto :error_exit");
    out
}

/// Render the presets zone body: one `:build_<id>` per menu selection, FEATURES
/// from the canonical resolved flags, description from the selection.
fn render_presets(manifest_dir: &Path) -> anyhow::Result<String> {
    let mut out = String::new();
    let menu = Selection::menu();
    for (i, sel) in menu.iter().enumerate() {
        let flags = spec::resolve_flags(manifest_dir, sel)?;
        out.push_str(&format!(":build_{}\n", sel.id()));
        out.push_str(&format!("set \"FEATURES={flags}\"\n"));
        out.push_str(&format!(
            "set \"BUILD_DESC={} ({})\"\n",
            sel.id(),
            sel.describe()
        ));
        out.push_str("goto :do_build");
        if i + 1 < menu.len() {
            out.push_str("\n\n");
        }
    }
    Ok(out)
}

/// Splice both generated zones into the current setup.bat, leaving all
/// hand-written glue untouched. Errors if either zone's sentinels are missing.
pub fn render_file(manifest_dir: &Path, current: &str) -> anyhow::Result<String> {
    let with_menu = splice(current, ZONE_MENU, &render_menu(manifest_dir))?;
    splice(&with_menu, ZONE_PRESETS, &render_presets(manifest_dir)?)
}

fn splice(current: &str, zone: &str, body: &str) -> anyhow::Result<String> {
    let b = begin(zone);
    let e = end(zone);
    let begin_at = current.find(&b).ok_or_else(|| {
        anyhow::Error::msg(format!("setup.bat missing generated:{zone} BEGIN sentinel"))
    })?;
    let after_begin = begin_at + b.len();
    let end_rel = current[after_begin..].find(&e).ok_or_else(|| {
        anyhow::Error::msg(format!("setup.bat missing generated:{zone} END sentinel"))
    })?;
    let end_at = after_begin + end_rel;

    let mut out = String::new();
    out.push_str(&current[..after_begin]);
    out.push('\n');
    out.push_str(body);
    out.push('\n');
    out.push_str(&current[end_at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn presets_have_a_label_per_menu_selection() {
        let p = render_presets(&root()).unwrap();
        for sel in Selection::menu() {
            assert!(
                p.contains(&format!(":build_{}", sel.id())),
                "missing :build_{}",
                sel.id()
            );
        }
    }

    #[test]
    fn dist_preset_ships_lean_release_channels() {
        let presets = render_presets(&root()).unwrap();
        let (_, dist_and_rest) = presets.split_once(":build_dist").unwrap();
        let (dist, _) = dist_and_rest.split_once("goto :do_build").unwrap();
        assert!(!dist.contains("channel-matrix"));
        assert!(dist.contains("whatsapp-web"));
        assert!(!dist.contains("channel-slack"));
    }

    #[test]
    fn real_setup_bat_help_describes_dist_as_lean() {
        let setup = std::fs::read_to_string(root().join("setup.bat")).unwrap();
        assert!(setup.contains("--dist        Build lean standard distribution"));
        assert!(!setup.contains("--dist        Build all channels"));
    }

    #[test]
    fn source_routing_defaults_to_recommended_dist_and_keeps_every_selection() {
        let m = render_menu(&root());
        assert!(m.contains("if \"%MODE%\"==\"\" goto :build_dist"));
        assert!(!m.contains("prebuilt"));
        assert!(!m.contains("set /p"));
        for sel in Selection::menu() {
            assert!(m.contains(&format!("goto :build_{}", sel.id())));
        }
    }

    #[test]
    fn splice_targets_named_zone_only() {
        let cur = format!(
            "A\n{}\nOLD\n{}\nB\n{}\nKEEP\n{}\nC\n",
            begin(ZONE_MENU),
            end(ZONE_MENU),
            begin(ZONE_PRESETS),
            end(ZONE_PRESETS)
        );
        let out = splice(&cur, ZONE_MENU, "NEW").unwrap();
        assert!(out.contains("NEW") && !out.contains("OLD"));
        assert!(out.contains("KEEP"), "other zone untouched");
    }

    #[test]
    fn splice_errors_without_zone() {
        assert!(splice("nothing", ZONE_MENU, "x").is_err());
    }
}
