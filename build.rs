#[path = "src/art.rs"]
#[allow(dead_code)]
mod art;

use std::{env, fs, path::PathBuf};

const ICON_SIZES: [u32; 7] = [16, 20, 24, 32, 40, 48, 64];

fn main() {
    println!("cargo:rerun-if-changed=src/art.rs");
    println!("cargo:rerun-if-changed=app.manifest");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ico = out.join("app.ico");
    fs::write(&ico, build_ico()).unwrap();

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("app.manifest");
    let rc = out.join("app.rc");
    let esc = |p: &PathBuf| p.display().to_string().replace('\\', "\\\\");
    fs::write(&rc, format!("1 ICON \"{}\"\n1 24 \"{}\"\n", esc(&ico), esc(&manifest))).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE).manifest_required().unwrap();
}

/// Multi-size .ico file (32-bit BMP entries).
fn build_ico() -> Vec<u8> {
    let images: Vec<Vec<u8>> = ICON_SIZES.iter().map(|&s| ico_bitmap(s)).collect();
    let mut buf = Vec::new();
    buf.extend_from_slice(&[0, 0, 1, 0]);
    buf.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, img) in ICON_SIZES.iter().zip(&images) {
        buf.extend_from_slice(&[*size as u8, *size as u8, 0, 0]);
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&32u16.to_le_bytes());
        buf.extend_from_slice(&(img.len() as u32).to_le_bytes());
        buf.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        buf.extend_from_slice(&img);
    }
    buf
}

fn ico_bitmap(size: u32) -> Vec<u8> {
    let pixels = art::app_icon(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut buf = Vec::new();
    for v in [40u32, size, size * 2] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&32u16.to_le_bytes());
    for v in [0u32, size * size * 4 + mask_row * size, 0, 0, 0, 0] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    // rows bottom-up, in BGRA
    for row in (0..size).rev() {
        for px in &pixels[(row * size) as usize..((row + 1) * size) as usize] {
            buf.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    buf.resize(buf.len() + (mask_row * size) as usize, 0);
    buf
}
