//! `cargo generate installers` - render install surfaces (setup.bat,
//! native packages, ...) from the canonical spec.
//! install.sh@HEAD is the behavioral reference. The spec is the single source
//! of truth; surfaces are derived and drift-checked. Surfaces are registered in

pub mod flake;
pub mod install_sh;
pub mod packaging;
pub mod setup_bat;
pub mod spec;

use spec::Selection as Sel;
use std::path::{Path, PathBuf};

/// A render: given the workspace root and the file's current content, produce
/// the regenerated content (splicing only sentinel zones).
type Render = fn(&Path, &str) -> anyhow::Result<String>;

/// One registered surface: a canonical name and the file it owns + how to
/// render it. The registry is the single list of "what we generate".
struct Surface {
    name: &'static str,
    file: &'static str,
    render: Render,
}

/// The surface registry. Adding a generated surface = one row here.
fn registry() -> Vec<Surface> {
    vec![
        Surface {
            name: "install-sh",
            file: "install.sh",
            render: |root, cur| install_sh::render_file(root, cur),
        },
        Surface {
            name: "setup-bat",
            file: "setup.bat",
            render: |root, cur| setup_bat::render_file(root, cur),
        },
        Surface {
            name: "pkgbuild",
            file: "dist/aur/PKGBUILD",
            render: |root, cur| packaging::render_pkgbuild(root, cur),
        },
        Surface {
            name: "scoop",
            file: "dist/scoop/zeroclaw.json",
            render: |root, cur| packaging::render_scoop(root, cur),
        },
        Surface {
            name: "flake",
            file: "flake.nix",
            render: |root, cur| flake::render_file(root, cur),
        },
    ]
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn features(
    selection_id: &str,
    target: Option<&str>,
    excluded: &[String],
) -> anyhow::Result<()> {
    let selection = Sel::from_id(selection_id).ok_or_else(|| {
        anyhow::Error::msg(format!(
            "unknown selection `{selection_id}` (known: {})",
            Sel::named()
                .iter()
                .map(Sel::id)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let list = spec::exclude_features(
        spec::resolve_feature_list_for_target(&workspace_root(), &selection, target)?,
        excluded,
    )?;
    println!("{}", list.join(","));
    Ok(())
}

pub fn run(targets: &[String], check: bool) -> anyhow::Result<()> {
    let all = registry();
    let selected: Vec<&Surface> = if targets.is_empty() {
        all.iter().collect()
    } else {
        let mut out = Vec::new();
        for t in targets {
            let s = all
                .iter()
                .find(|s| s.name == t || s.file == t)
                .ok_or_else(|| {
                    anyhow::Error::msg(format!(
                        "unknown target `{t}` (known: {})",
                        all.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
                    ))
                })?;
            out.push(s);
        }
        out
    };

    let root = workspace_root();
    let mut drift = false;

    for s in selected {
        let path = root.join(s.file);
        let current = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::Error::msg(format!("{}: {e}", path.display())))?;
        let rendered = (s.render)(&root, &current)?;
        if check {
            if current != rendered {
                eprintln!("DRIFT: {} is out of sync with the spec", s.name);
                drift = true;
            } else {
                println!("ok: {} in sync", s.name);
            }
        } else if current != rendered {
            std::fs::write(&path, rendered)?;
            println!("wrote {}", path.display());
        } else {
            println!("unchanged {}", path.display());
        }
    }

    if check && drift {
        anyhow::bail!("one or more installers drifted; run `cargo generate installers`");
    }
    Ok(())
}
