#![no_std]

extern crate alloc;

use alloc::borrow::Cow;
use core::str;

use oxdock_buildtime_macros::embed;

embed! {
    name: FirmwareAssets,
    script: {
        WORKDIR /
        MKDIR firmware
        WRITE firmware/version.txt 1.0.0-no-std
    },
    out_dir: "prebuilt",
}

embed! {
    name: BrandingAssets,
    script: {
        WORKDIR /
        MKDIR branding
        WRITE branding/note.txt OxDock-no-std
    },
    out_dir: "prebuilt_branding",
}

pub fn firmware_version() -> &'static str {
    let file = FirmwareAssets::get("firmware/version.txt").expect("embedded version file");
    match file.data {
        Cow::Borrowed(bytes) => str::from_utf8(bytes).expect("utf8 contents"),
        Cow::Owned(_) => panic!("embedded assets should be borrowed"),
    }
}

pub fn branding_note() -> &'static str {
    let file = BrandingAssets::get("branding/note.txt").expect("embedded note file");
    match file.data {
        Cow::Borrowed(bytes) => str::from_utf8(bytes).expect("utf8 contents"),
        Cow::Owned(_) => panic!("embedded assets should be borrowed"),
    }
}

pub fn asset_count() -> usize {
    FirmwareAssets::iter().count() + BrandingAssets::iter().count()
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_embedded_assets() {
        assert_eq!(firmware_version().trim(), "1.0.0-no-std");
        assert_eq!(branding_note().trim(), "OxDock-no-std");
        assert!(asset_count() >= 2);
    }
}
