use mars_xlog_core::compress::{
    decompress_raw_zlib, decompress_zstd_frames, ZlibStreamCompressor, ZstdLevel,
    ZstdStreamCompressor,
};

#[test]
fn zlib_stream_roundtrip() {
    let mut compressor = ZlibStreamCompressor::default();
    let mut out = Vec::new();

    compressor.write_plain(b"hello", &mut out).unwrap();
    compressor.write_plain(b" world", &mut out).unwrap();
    compressor.finish_into(&mut out).unwrap();

    let plain = decompress_raw_zlib(&out).unwrap();
    assert_eq!(plain, b"hello world");
}

#[test]
fn zstd_frames_roundtrip() {
    let mut compressor = ZstdStreamCompressor::new(ZstdLevel::try_from(3).unwrap()).unwrap();
    let mut out = Vec::new();

    compressor.write_plain(b"mars", &mut out).unwrap();
    compressor.write_plain(b" xlog", &mut out).unwrap();
    compressor.finish_into(&mut out).unwrap();

    let plain = decompress_zstd_frames(&out).unwrap();
    assert_eq!(plain, b"mars xlog");
}
