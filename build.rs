// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! Embeds the app icon in the Windows executable, so Explorer and the taskbar show it. Other
//! platforms get their icon from the window (src/main.rs) and the macOS app bundle
//! (scripts/package.py). Regenerate assets/icon/icon.ico with `scripts/make_icons.py`.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/icon.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon/icon.ico");
        resource
            .compile()
            .expect("embedding the Windows icon failed (needs the Windows SDK's rc.exe)");
    }
}
