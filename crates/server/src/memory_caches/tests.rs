use super::*;

fn config(toml: &str) -> ServerConfig {
    toml::from_str(&format!(
        "[server]\nhost = \"127.0.0.1\"\nport = 8000\n{toml}"
    ))
    .unwrap()
}

/// No variable set.
fn unset(_: &str) -> Option<String> {
    None
}

/// #1313: `[memory.caches]` values by key; an unknown key fails the load,
/// naming it and every valid key.
#[test]
fn an_unknown_key_fails_the_load_naming_the_valid_ones() {
    let settings = CacheSettings::of(&config(
        "[memory.caches]\nmetatile = 8192\npvol_pixel = { min = 2048, max = 8192 }\n",
    ))
    .unwrap();
    assert_eq!(settings.get("metatile"), Some(Size::Pinned(8192)));
    assert_eq!(
        settings.get("pvol_pixel"),
        Some(Size::Bounded(Bounds::mib(2048, 8192)))
    );
    assert_eq!(settings.get("rendered"), None);
    assert_eq!(
        CacheSettings::of(&config("")).unwrap(),
        CacheSettings::default()
    );

    let typo = config("[memory.caches]\nmetatile_cache = 1\npvol_pixels = 2\nrendered = 3\n");
    let error = CacheSettings::of(&typo).unwrap_err();
    assert!(
        error.starts_with(
            "[memory.caches]: unknown keys metatile_cache, pvol_pixels; the keys are "
        ),
        "{error}"
    );
    for key in keys::KEYS {
        assert!(error.contains(key.key), "{error} lacks {}", key.key);
    }
    // Past the load, only the known keys.
    assert_eq!(
        CacheSettings::known(&typo).get("rendered"),
        Some(Size::Pinned(3))
    );
}

/// #1313: a cache set in two places logs one WARN naming the place that
/// sizes it and the one ignored; a deprecated variable alone logs one
/// naming its key, with its value.
#[test]
fn a_size_set_twice_warns_naming_the_winner() {
    let env = |var: &str| match var {
        "MC_PVOL_PIXEL_CACHE_MB" => Some("4096".to_string()),
        "MC_RENDER_MEMORY_MB" => Some("2G".to_string()),
        _ => None,
    };
    let both = config(
        "metatile_cache_mb = 1024\n\
         [memory.caches]\nmetatile = 8192\npvol_pixel = 2048\n",
    );
    let settings = CacheSettings::of(&both).unwrap();
    assert_eq!(
        conflict_warnings(&both, &settings, &env),
        [
            "metatile_cache is set in 2 places: [memory.caches] metatile sizes it; [server] \
             metatile_cache_mb is ignored, remove it",
            "pvol_pixel_cache is set in 2 places: [memory.caches] pvol_pixel sizes it; \
             MC_PVOL_PIXEL_CACHE_MB is ignored, remove it",
        ]
    );
    // A variable whose key is set is in the WARN above, not deprecated twice.
    assert_eq!(
        variable_warnings(&settings, &env),
        [(
            "MC_RENDER_MEMORY_MB",
            "MC_RENDER_MEMORY_MB is deprecated since 0.16.0 and ignored from 0.18.0: set \
             [memory.caches] render_budget in config.toml instead"
                .to_string()
        )]
    );
    let alone = CacheSettings::default();
    let warnings = variable_warnings(&alone, &env);
    assert_eq!(warnings.len(), 2);
    assert_eq!(
        warnings[0].1,
        "MC_PVOL_PIXEL_CACHE_MB is deprecated since 0.16.0 and ignored from 0.18.0: set \
         [memory.caches] pvol_pixel = 4096 in config.toml instead"
    );
    assert!(conflict_warnings(&config(""), &alone, &env).is_empty());
    assert!(variable_warnings(&alone, &unset).is_empty());
}

/// The rendered-image cache keeps its `[wms]` deprecation warnings while
/// `[memory.caches] rendered` is unset; set, it names every alias ignored.
#[test]
fn the_rendered_cache_names_every_alias_it_wins_over() {
    let wms = "[[collections]]\nid = \"a\"\ntitle = \"t\"\ndescription = \"d\"\n\
               apis = [\"wms\"]\n[collections.wms]\nrendered_cache_mb = 512\n";
    let deprecated = config(wms);
    let warnings = conflict_warnings(&deprecated, &CacheSettings::default(), &unset);
    assert_eq!(warnings, deprecated.rendered_cache_mb().1);
    assert!(warnings[0].contains("[wms] rendered_cache_mb is deprecated"));

    let keyed = config(&format!(
        "rendered_cache_mb = 128\n[memory.caches]\nrendered = 256\n{wms}"
    ));
    let settings = CacheSettings::of(&keyed).unwrap();
    assert_eq!(
        conflict_warnings(&keyed, &settings, &unset),
        [
            "rendered_cache is set in 3 places: [memory.caches] rendered sizes it; [server] \
          rendered_cache_mb and the deprecated [wms] rendered_cache_mb are ignored, remove them"
        ]
    );
}

/// The INFO block names each size [`apply`] changed and its source, with a
/// range's floor and ceiling, and what stopped short.
#[test]
fn the_changes_block_names_each_size_and_its_source() {
    use ds_cache::plan::{Entry, Scope};
    let change = |source, from: u64, to: u64, configured: u64| Configured {
        entry: Entry::cache("pvol_pixel_cache", Scope::Engine("odim-volume"), source),
        from_bytes: from * MIB,
        to_bytes: to * MIB,
        configured_bytes: configured * MIB,
    };
    assert_eq!(changes_block(&[]), None);
    let block = changes_block(&[
        change(
            SizeSource::Config("[memory.caches] pvol_pixel"),
            1024,
            4096,
            4096,
        ),
        change(
            SizeSource::Bounds("[memory.caches] pvol_pixel", Bounds::mib(64, 128)),
            4096,
            4096,
            128,
        ),
        change(SizeSource::Default, 4096, 1100, 1024),
    ])
    .unwrap();
    assert_eq!(
        block,
        "[memory.caches] applied: 3 sizes changed\n  \
         pvol_pixel_cache 1024 MiB → 4096 MiB (config:[memory.caches] pvol_pixel)\n  \
         pvol_pixel_cache kept at 4096 MiB for the memory plan (bounds:[memory.caches] \
         pvol_pixel, floor 64 MiB, ceiling 128 MiB)\n  \
         pvol_pixel_cache 4096 MiB → 1100 MiB (default; toward 1024 MiB: its resize stopped \
         short)"
    );
}
