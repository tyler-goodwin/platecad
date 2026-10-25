//! The app icon: `assets/plate.ico` (drawn by `assets/make_icon.py`) and its
//! embedding into `plate.exe` by `build.rs`.

use std::path::PathBuf;

fn asset(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// (size, image bytes) of each entry in an .ico.
fn ico_entries(ico: &[u8]) -> Vec<(u32, &[u8])> {
    let u16_at = |i: usize| u16::from_le_bytes([ico[i], ico[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes(ico[i..i + 4].try_into().unwrap());
    assert_eq!((u16_at(0), u16_at(2)), (0, 1), "not an icon file");
    (0..u16_at(4) as usize)
        .map(|i| {
            let entry = 6 + 16 * i;
            let size = match ico[entry] {
                0 => 256,
                n => n as u32,
            };
            let (len, offset) = (u32_at(entry + 8) as usize, u32_at(entry + 12) as usize);
            (size, &ico[offset..offset + len])
        })
        .collect()
}

#[test]
fn icon_has_every_size_windows_asks_for_as_png() {
    let ico = asset("plate.ico");
    let entries = ico_entries(&ico);
    let sizes: Vec<u32> = entries.iter().map(|(size, _)| *size).collect();
    assert_eq!(sizes, [16, 24, 32, 48, 64, 128, 256]);
    for (size, image) in entries {
        assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"), "{size} px entry is not a PNG");
    }
    // plate.png (for docs) is the 256 px entry, so the two can't drift apart.
    assert_eq!(ico_entries(&ico)[6].1, asset("plate.png"));
}

#[cfg(windows)]
#[test]
fn plate_exe_embeds_the_icon_and_version_details() {
    let exe = std::fs::read(env!("CARGO_BIN_EXE_plate")).unwrap();
    let contains = |needle: &[u8]| exe.windows(needle.len()).any(|w| w == needle);
    for (size, image) in ico_entries(&asset("plate.ico")) {
        assert!(contains(image), "{size} px icon missing from plate.exe");
    }
    let description: Vec<u8> = "platecad: code-first CAD"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert!(contains(&description), "version details missing from plate.exe");
}
