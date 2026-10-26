use super::*;
use ds_core::{deadline, error::DataServerError};
use serde_json::json;
use zarrs::{
    array::{data_type, Array, ArrayBuilder},
    filesystem::FilesystemStore,
    plugin::ExtensionName,
};

fn v3(metadata: serde_json::Value) -> MetadataV3 {
    serde_json::from_value(metadata).unwrap()
}

fn v2(metadata: serde_json::Value) -> MetadataV2 {
    serde_json::from_value(metadata).unwrap()
}

/// The codec a metadata entry creates, which must be this crate's.
fn ours(codec: Codec) -> Arc<dyn BytesToBytesCodecTraits> {
    let Codec::BytesToBytes(codec) = codec else {
        panic!("zstd is a bytes-to-bytes codec");
    };
    assert!(
        codec.as_any().is::<ZstdCodec>(),
        "\"zstd\" must resolve to engine-zarr's codec"
    );
    codec
}

fn configuration(codec: &dyn BytesToBytesCodecTraits, version: ZarrVersion) -> serde_json::Value {
    serde_json::to_value(
        codec
            .configuration(version, &CodecMetadataOptions::default())
            .unwrap(),
    )
    .unwrap()
}

/// zarrs' own codec (its `zstd` feature) decodes without a bound. It must
/// not be registered next to ours: one plugin per Zarr version answers to
/// "zstd", and it creates this crate's type.
#[test]
fn exactly_one_zstd_plugin_is_registered_for_each_zarr_version() {
    let v3_plugins = inventory::iter::<CodecPluginV3>
        .into_iter()
        .filter(|plugin| plugin.match_name("zstd"))
        .count();
    let v2_plugins = inventory::iter::<CodecPluginV2>
        .into_iter()
        .filter(|plugin| plugin.match_name("zstd"))
        .count();
    assert_eq!((v3_plugins, v2_plugins), (1, 1));
    ours(
        Codec::from_metadata(&v3(
            json!({"name": "zstd", "configuration": {"level": 1, "checksum": false}}),
        ))
        .unwrap(),
    );
    ours(Codec::from_metadata(&v2(json!({"id": "zstd", "level": 1}))).unwrap());
}

/// numcodecs writes `{"level": N}`; recent numcodecs and zarr-python add
/// `checksum`. Both open; the level-only form has no checksum.
#[test]
fn v2_level_only_and_checksum_forms_create_the_codec() {
    for (metadata, expected) in [
        (
            json!({"id": "zstd", "level": 6}),
            json!({"level": 6, "checksum": false}),
        ),
        (
            json!({"id": "zstd", "level": 6, "checksum": false}),
            json!({"level": 6, "checksum": false}),
        ),
        (
            json!({"id": "zstd", "level": -3, "checksum": true}),
            json!({"level": -3, "checksum": true}),
        ),
    ] {
        let codec = ours(Codec::from_metadata(&v2(metadata.clone())).unwrap());
        assert_eq!(
            configuration(&*codec, ZarrVersion::V2),
            expected,
            "{metadata}"
        );
    }
    for invalid in [
        json!({"id": "zstd", "level": 23}),
        json!({"id": "zstd", "level": 1, "checksum": true, "extra": 1}),
    ] {
        assert!(
            Codec::from_metadata(&v2(invalid.clone())).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn configuration_round_trips_for_v2_and_v3() {
    // V3: name and configuration as parsed, and stable when parsed again.
    for configuration_v3 in [
        json!({"level": 5, "checksum": true}),
        json!({"level": 0, "checksum": false}),
    ] {
        let codec = ours(
            Codec::from_metadata(&v3(
                json!({"name": "zstd", "configuration": configuration_v3}),
            ))
            .unwrap(),
        );
        assert_eq!(codec.name(ZarrVersion::V3).as_deref(), Some("zstd"));
        assert_eq!(configuration(&*codec, ZarrVersion::V3), configuration_v3);
    }
    // V2: the written form opens to the same codec again.
    for metadata in [
        json!({"id": "zstd", "level": 6}),
        json!({"id": "zstd", "level": 2, "checksum": true}),
    ] {
        let codec = ours(Codec::from_metadata(&v2(metadata)).unwrap());
        assert_eq!(codec.name(ZarrVersion::V2).as_deref(), Some("zstd"));
        let written = configuration(&*codec, ZarrVersion::V2);
        let mut again = json!({"id": "zstd"});
        again
            .as_object_mut()
            .unwrap()
            .extend(written.as_object().unwrap().clone());
        let reopened = ours(Codec::from_metadata(&v2(again)).unwrap());
        assert_eq!(configuration(&*reopened, ZarrVersion::V2), written);
    }
    // A V3 array written with the codec stores zarrs' metadata and reopens
    // with the same codec chain and values.
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(FilesystemStore::new(dir.path()).unwrap());
    let writer = ArrayBuilder::new(vec![4], vec![4], data_type::float32(), 0f32)
        .bytes_to_bytes_codecs(vec![Arc::new(ZstdCodec::new(5, true))])
        .build(storage.clone(), "/a")
        .unwrap();
    writer.store_metadata().unwrap();
    writer.store_chunk(&[0], vec![1f32, 2., 3., 4.]).unwrap();
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("a/zarr.json")).unwrap()).unwrap();
    assert_eq!(
        stored["codecs"][1],
        json!({"name": "zstd", "configuration": {"level": 5, "checksum": true}})
    );
    let reader = Array::open(storage, "/a").unwrap();
    let options = CodecMetadataOptions::default();
    assert_eq!(
        reader.codecs().create_metadatas(&options),
        writer.codecs().create_metadatas(&options)
    );
    let values: Vec<f32> = reader.retrieve_array_subset(&reader.subset_all()).unwrap();
    assert_eq!(values, [1., 2., 3., 4.]);
}

#[test]
fn encoder_honours_level_and_checksum() {
    let raw: Vec<u8> = (0..10_000u32)
        .flat_map(|n| (n % 97).to_le_bytes())
        .collect();
    for (level, checksum) in [(1, false), (1, true), (19, false), (-5, true)] {
        let encoded = ZstdCodec::new(level, checksum)
            .encode(Cow::Borrowed(&raw), &CodecOptions::default())
            .unwrap();
        let mut reference = zstd::bulk::Compressor::new(level).unwrap();
        reference.include_checksum(checksum).unwrap();
        assert_eq!(*encoded, reference.compress(&raw).unwrap(), "level {level}");
        // Frame header descriptor, bit 2: Content_Checksum_flag (RFC 8878).
        assert_eq!(encoded[4] & 0b100 != 0, checksum, "level {level}");
        assert_eq!(zstd::bulk::decompress(&encoded, raw.len()).unwrap(), raw);
    }
}

/// zarrs' codec ignored the representation and decoded whatever the frame
/// held. Ours, used bare (coordinate arrays are never wrapped), is bounded.
#[test]
fn the_bare_codec_decodes_bounded_and_typed() {
    let codec = ZstdCodec::new(1, false);
    let options = CodecOptions::default();
    let bomb = codec
        .encode(Cow::Owned(vec![0; 1024 * 1024]), &options)
        .unwrap();
    assert!(bomb.len() < 8192);
    for representation in [
        BytesRepresentation::FixedSize(4),
        BytesRepresentation::BoundedSize(4),
        BytesRepresentation::UnboundedSize,
    ] {
        assert!(codec
            .decode(bomb.clone(), &representation, &options)
            .is_err());
    }
    let encoded = codec.encode(Cow::Owned(vec![3; 100]), &options).unwrap();
    assert_eq!(
        *codec
            .decode(
                encoded.clone(),
                &BytesRepresentation::FixedSize(100),
                &options
            )
            .unwrap(),
        [3; 100]
    );
    let _deadline = deadline::enter(Some(std::time::Instant::now()));
    let error = codec
        .decode(encoded, &BytesRepresentation::FixedSize(100), &options)
        .unwrap_err();
    assert!(matches!(
        crate::catalog::chunk_read_error(error.into()),
        DataServerError::DeadlineExceeded
    ));
}
