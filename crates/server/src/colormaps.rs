//! Palette registry construction: built-ins + `[[colormaps]]` +
//! `colormaps_dir` files.
//!
//! Built once per config load (startup and every reload) and handed to
//! `ds_render::StyleContext`, so user-defined colormaps are referencable
//! anywhere a built-in name is accepted. Duplicate user names are a config
//! error; a user palette shadowing a built-in name is allowed and logged as
//! a warning (it deliberately restyles that name deployment-wide).

use std::path::{Path, PathBuf};

use ds_core::config::{ColormapDef, ServerConfig};
use ds_core::error::DataServerError;
use ds_render::{Interpolation, Palette, PaletteInsert, PaletteRegistry};

/// Build the palette registry for a config: built-ins, then `[[colormaps]]`
/// entries, then `colormaps_dir` files (sorted by filename). `config_dir` is
/// the parent directory of the main config file — a relative
/// `colormaps_dir` resolves against it, mirroring `collections_dir`.
pub fn build_palette_registry(
    config: &ServerConfig,
    config_dir: Option<&Path>,
) -> Result<PaletteRegistry, DataServerError> {
    let mut registry = PaletteRegistry::with_builtins();

    for (i, def) in config.colormaps.iter().enumerate() {
        let owner = format!("[[colormaps]] entry {}", i + 1);
        // `ServerConfig::validate()` requires `name` for config.toml entries.
        let name = def.name.clone().unwrap_or_default();
        let palette = def_to_palette(&owner, &name, def)?;
        insert_logged(&mut registry, palette, &owner)?;
    }

    if let Some(dir) = &config.server.colormaps_dir {
        let dir_path = resolve_dir(dir, config_dir);
        load_colormaps_dir(&mut registry, &dir_path)?;
    }

    Ok(registry)
}

/// Convert a validated [`ColormapDef`] into a [`Palette`].
fn def_to_palette(owner: &str, name: &str, def: &ColormapDef) -> Result<Palette, DataServerError> {
    let interpolation = match def.interpolation.as_deref() {
        Some("step") => Interpolation::Step,
        _ => Interpolation::Linear,
    };
    let mut stops = Vec::with_capacity(def.color_stops.len());
    for stop in &def.color_stops {
        let color = ds_render::parse_hex_color(&stop.color)
            .map_err(|e| DataServerError::Config(format!("{owner}: colormap '{name}': {e}")))?;
        stops.push(ds_render::ColorStop {
            value: stop.value,
            color,
        });
    }
    let mut palette = Palette::new(name, stops, interpolation);
    palette.title = def.title.clone();
    if let Some(nd) = &def.nodata_color {
        let color = ds_render::parse_hex_color(nd).map_err(|e| {
            DataServerError::Config(format!("{owner}: colormap '{name}': nodata_color: {e}"))
        })?;
        palette.nodata_color = Some(color);
    }
    Ok(palette)
}

fn resolve_dir(dir: &str, config_dir: Option<&Path>) -> PathBuf {
    let p = Path::new(dir);
    if p.is_relative() {
        if let Some(base) = config_dir {
            return base.join(p);
        }
    }
    p.to_path_buf()
}

/// Load every palette file in `dir` (non-recursive, sorted by filename).
/// Extensions: `.toml` (ColormapDef), `.cpt` (GMT), `.txt`/`.clr` (GDAL
/// color-relief), `.sld` (SLD ColorMap). Other extensions are skipped, so
/// `.disabled` works like it does for collections. A missing directory is a
/// hard error (a typo'd path must not silently load nothing); an empty one
/// logs a warning.
fn load_colormaps_dir(registry: &mut PaletteRegistry, dir: &Path) -> Result<(), DataServerError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        DataServerError::Config(format!(
            "colormaps_dir '{}' cannot be read: {e}",
            dir.display()
        ))
    })?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();

    let mut loaded = 0usize;
    for path in &files {
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        let parse = |what: &str, r: Result<Palette, String>| -> Result<Palette, DataServerError> {
            r.map_err(|e| {
                DataServerError::Config(format!("colormaps_dir {filename} ({what}): {e}"))
            })
        };
        let palette = match ext.as_str() {
            "toml" => {
                let text = read(path)?;
                let def: ColormapDef = toml::from_str(&text).map_err(|e| {
                    DataServerError::Config(format!("colormaps_dir {filename}: {e}"))
                })?;
                let name = def.name.clone().unwrap_or_else(|| stem.to_string());
                if def.name.is_some() && name != stem {
                    tracing::warn!(
                        "colormaps_dir {filename}: filename stem '{stem}' differs from \
                         colormap name '{name}'"
                    );
                }
                def.validate(&format!("colormaps_dir {filename}"), &name)?;
                def_to_palette(&format!("colormaps_dir {filename}"), &name, &def)?
            }
            "cpt" => parse("GMT cpt", ds_render::parse_cpt(&stem, &read(path)?))?,
            "txt" | "clr" => parse(
                "GDAL color-relief",
                ds_render::parse_gdal_txt(&stem, &read(path)?),
            )?,
            "sld" => parse(
                "SLD ColorMap",
                crate::sld::parse_sld_colormap(&stem, &read(path)?),
            )?,
            _ => continue,
        };
        insert_logged(registry, palette, &format!("colormaps_dir {filename}"))?;
        loaded += 1;
    }

    if loaded == 0 {
        tracing::warn!(
            "colormaps_dir '{}' contains no palette files (.toml/.cpt/.txt/.clr/.sld)",
            dir.display()
        );
    } else {
        tracing::info!("Loaded {loaded} colormap(s) from '{}'", dir.display());
    }
    Ok(())
}

/// Fingerprint of everything that can change how styles resolve: the
/// `[[colormaps]]` definitions, style bundles, per-collection `[wms]`
/// blocks, and the raw bytes of every `colormaps_dir` palette file (sorted
/// by name). Compared across reloads to decide whether the rendered /
/// meta-tile caches can be reused — their keys carry no style content, so
/// reusing them across a style change serves stale colors. Only ever
/// compared within one process (previous load vs reload), so `Debug`
/// formatting and `DefaultHasher` are acceptable serializations.
pub fn style_config_fingerprint(config: &ServerConfig, config_dir: Option<&Path>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{:?}", config.colormaps).hash(&mut h);
    format!("{:?}", config.style_bundles).hash(&mut h);
    for c in &config.collections {
        c.id.hash(&mut h);
        format!("{:?}", c.wms).hash(&mut h);
    }
    if let Some(dir) = &config.server.colormaps_dir {
        let dir_path = resolve_dir(dir, config_dir);
        if let Ok(entries) = std::fs::read_dir(&dir_path) {
            let mut files: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect();
            files.sort();
            for f in &files {
                f.file_name().unwrap_or_default().hash(&mut h);
                std::fs::read(f).unwrap_or_default().hash(&mut h);
            }
        }
    }
    h.finish()
}

fn read(path: &Path) -> Result<String, DataServerError> {
    std::fs::read_to_string(path)
        .map_err(|e| DataServerError::Config(format!("Failed to read {}: {e}", path.display())))
}

fn insert_logged(
    registry: &mut PaletteRegistry,
    palette: Palette,
    owner: &str,
) -> Result<(), DataServerError> {
    let name = palette.name.clone();
    match registry.insert(palette) {
        Ok(PaletteInsert::Added) => Ok(()),
        Ok(PaletteInsert::ShadowedBuiltin) => {
            tracing::warn!(
                "{owner}: colormap '{name}' shadows the built-in palette of the same \
                 name — every collection using '{name}' now renders with the custom palette"
            );
            Ok(())
        }
        Err(e) => Err(DataServerError::Config(format!("{owner}: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(colormaps_toml: &str) -> ServerConfig {
        let src = format!("[server]\nhost = \"127.0.0.1\"\nport = 8000\n{colormaps_toml}");
        toml::from_str(&src).expect("test config parses")
    }

    #[test]
    fn config_colormaps_register_and_resolve() {
        let config = config_with(
            r##"
            [[colormaps]]
            name = "house"
            title = "House Style"
            interpolation = "step"
            color_stops = [
                { value = 0.0, color = "#00000000" },
                { value = 10.0, color = "#FF0000" },
            ]
            nodata_color = "#01020304"
            "##,
        );
        config.validate().expect("valid config");
        let reg = build_palette_registry(&config, None).unwrap();
        let p = reg.get("house").expect("registered");
        assert_eq!(p.title.as_deref(), Some("House Style"));
        assert_eq!(p.interpolation, Interpolation::Step);
        assert_eq!(p.stops.len(), 2);
        assert_eq!(p.nodata_color, Some([1, 2, 3, 4]));
        // Built-ins still present.
        assert!(reg.get("radar_dbz").is_some());
    }

    #[test]
    fn duplicate_config_colormap_is_error() {
        let config = config_with(
            r##"
            [[colormaps]]
            name = "dup"
            color_stops = [ { value = 0.0, color = "#000000" } ]
            [[colormaps]]
            name = "dup"
            color_stops = [ { value = 0.0, color = "#FFFFFF" } ]
            "##,
        );
        // Caught in ds-core validate() already…
        assert!(config.validate().is_err());
        // …and defense-in-depth at registry build.
        assert!(build_palette_registry(&config, None).is_err());
    }

    #[test]
    fn shadowing_builtin_is_allowed() {
        let config = config_with(
            r##"
            [[colormaps]]
            name = "temperature"
            color_stops = [
                { value = 0.0, color = "#000000" },
                { value = 1.0, color = "#FFFFFF" },
            ]
            "##,
        );
        config.validate().expect("shadowing validates");
        let reg = build_palette_registry(&config, None).unwrap();
        // The user palette wins.
        assert_eq!(reg.get("temperature").unwrap().stops.len(), 2);
    }

    #[test]
    fn colormaps_dir_loads_by_extension_and_missing_dir_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ramp.toml"),
            r##"
            color_stops = [
                { value = 0.0, color = "#000000" },
                { value = 5.0, color = "#FFFFFF" },
            ]
            "##,
        )
        .unwrap();
        std::fs::write(dir.path().join("ignored.disabled"), "junk").unwrap();
        std::fs::write(
            dir.path().join("relief.txt"),
            "0 0 0 0\n100 255 255 255\nnv 0 0 0 0\n",
        )
        .unwrap();

        let mut config = config_with("");
        config.server.colormaps_dir = Some(dir.path().to_string_lossy().to_string());
        let reg = build_palette_registry(&config, None).unwrap();
        // .toml name defaults to the file stem.
        assert!(reg.get("ramp").is_some());
        assert!(reg.get("relief").is_some());
        assert_eq!(reg.get("relief").unwrap().nodata_color, Some([0, 0, 0, 0]));

        let mut missing = config_with("");
        missing.server.colormaps_dir = Some("/nonexistent/colormaps.d".to_string());
        assert!(build_palette_registry(&missing, None).is_err());
    }

    #[test]
    fn relative_dir_resolves_against_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("colormaps.d");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(
            sub.join("x.toml"),
            r##"color_stops = [ { value = 0.0, color = "#000000" } ]"##,
        )
        .unwrap();
        let mut config = config_with("");
        config.server.colormaps_dir = Some("colormaps.d".to_string());
        let reg = build_palette_registry(&config, Some(dir.path())).unwrap();
        assert!(reg.get("x").is_some());
    }
}
