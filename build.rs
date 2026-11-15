// Build script (MSVC only):
//   * embeds overlay.manifest (Common Controls v6 for the dialog);
//   * embeds assets/icon.ico as the exe icon (resource id 1, also used for the tray and the
//     dialog). The .res file is written by hand below, so no resource compiler (rc.exe)
//     or extra crate is needed;
//   * makes the exe's own DLL imports resolve from System32 only.
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=overlay.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{dir}\\overlay.manifest");
    // Resolve the exe's own DLL imports (pdh.dll, ...) from System32 only, never from the
    // exe's folder: this process usually runs elevated. (0x800 = LOAD_LIBRARY_SEARCH_SYSTEM32)
    println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");

    let ico = fs::read(PathBuf::from(&dir).join("assets").join("icon.ico")).expect("assets/icon.ico");
    let res = icon_res(&ico);
    let path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("icon.res");
    fs::write(&path, res).expect("write icon.res");
    println!("cargo:rustc-link-arg-bins={}", path.display());
}

// ------------------------------------------------------------------ .res writer

fn u16le(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn u32le(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

/// Appends one resource (numeric type and name ids) in .res format.
fn resource(out: &mut Vec<u8>, rtype: u16, id: u16, data: &[u8]) {
    u32le(out, data.len() as u32); // DataSize
    u32le(out, 32); // HeaderSize: 8 + type(4) + name(4) + 16
    u16le(out, 0xFFFF);
    u16le(out, rtype);
    u16le(out, 0xFFFF);
    u16le(out, id);
    u32le(out, 0); // DataVersion
    u16le(out, 0x1030); // MemoryFlags: MOVEABLE | PURE | DISCARDABLE
    u16le(out, 0x0409); // Language: en-US
    u32le(out, 0); // Version
    u32le(out, 0); // Characteristics
    out.extend_from_slice(data);
    pad4(out);
}

/// Converts an .ico file into RT_ICON entries plus one RT_GROUP_ICON (id 1 = the exe icon).
fn icon_res(ico: &[u8]) -> Vec<u8> {
    let rd16 = |o: usize| u16::from_le_bytes([ico[o], ico[o + 1]]);
    let rd32 = |o: usize| u32::from_le_bytes([ico[o], ico[o + 1], ico[o + 2], ico[o + 3]]);
    assert!(rd16(0) == 0 && rd16(2) == 1, "not an .ico file");
    let count = rd16(4) as usize;

    let mut out = Vec::new();
    resource(&mut out, 0, 0, &[]); // mandatory empty first resource (the .res signature)

    let mut group = Vec::new();
    u16le(&mut group, 0); // reserved
    u16le(&mut group, 1); // type: icon
    u16le(&mut group, count as u16);
    for i in 0..count {
        let e = 6 + i * 16; // ICONDIRENTRY
        let size = rd32(e + 8) as usize;
        let offset = rd32(e + 12) as usize;
        let id = (i + 1) as u16;
        resource(&mut out, 3, id, &ico[offset..offset + size]); // RT_ICON
        group.extend_from_slice(&ico[e..e + 12]); // width, height, colors, reserved, planes, bpp, size
        u16le(&mut group, id); // GRPICONDIRENTRY ends with the RT_ICON id (not the offset)
    }
    resource(&mut out, 14, 1, &group); // RT_GROUP_ICON
    out
}
