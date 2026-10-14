fn chunks(png: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut at = 8;
    let mut out = Vec::new();
    while at < png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
        let kind = String::from_utf8(png[at + 4..at + 8].to_vec()).unwrap();
        out.push((kind, png[at + 8..at + 8 + len].to_vec()));
        at += 12 + len;
    }
    out
}

#[test]
fn ticket_code_becomes_a_scannable_png() {
    let png = crabgate::qr::png(b"NWKR-WWQ2-NFES-9ATT").unwrap();
    assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let chunks = chunks(&png);
    assert_eq!(
        chunks
            .iter()
            .map(|(kind, _)| kind.as_str())
            .collect::<Vec<_>>(),
        ["IHDR", "IDAT", "IEND"]
    );
    let header = &chunks[0].1;
    assert_eq!(u32::from_be_bytes(header[..4].try_into().unwrap()), 232);
    assert_eq!(u32::from_be_bytes(header[4..8].try_into().unwrap()), 232);
    assert_eq!(&header[8..], &[1, 0, 0, 0, 0]);

    let stream = &chunks[1].1;
    assert_eq!(&stream[..2], &[0x78, 0x01]);
    assert_eq!(stream[2], 1);
    let len = u16::from_le_bytes([stream[3], stream[4]]) as usize;
    let rows = &stream[7..7 + len];
    assert_eq!(len, 232 * 30);
    let pixel_is_dark = |x: usize, y: usize| rows[y * 30 + 1 + x / 8] & (0x80 >> (x % 8)) == 0;
    assert!(!pixel_is_dark(0, 0));
    assert!(!pixel_is_dark(31, 31));
    assert!(pixel_is_dark(32, 32));
    assert!(pixel_is_dark(87, 32));
    assert!(!pixel_is_dark(40, 40));
    assert!(pixel_is_dark(48, 48));
}
