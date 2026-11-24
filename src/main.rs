// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::sync::Arc;

use aws_iot_mqtt_client::app::MqttApp;

fn main() -> eframe::Result<()> {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("aws-iot-mqtt-worker")
            .build()
            .expect("failed to start tokio runtime"),
    );

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("AWS IoT MQTT Client")
        .with_inner_size([1200.0, 820.0])
        .with_min_inner_size([820.0, 520.0]);
    // Window, taskbar, and (when not run from the .app bundle) Dock icon.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/icon.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "AWS IoT MQTT Client",
        options,
        Box::new(move |cc| Ok(Box::new(MqttApp::new(cc, runtime)))),
    )
}
