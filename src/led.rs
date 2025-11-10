use std::path::Path;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy)]
pub enum LedMode {
    On,
    Heartbeat,
}

#[derive(Clone, Copy)]
pub enum LedColor {
    Red,
    Green,
    Blue,
    Yellow,
    White,
    Purple,
}

struct LedState {
    color: LedColor,
    mode: LedMode,
}

pub struct LedManager {
    brightness: u8, // 1–100
    state: LedState,
    override_enabled: bool,
    override_state: Option<LedState>,
}

impl LedManager {
    pub fn new(brightness: u8) -> Self {
        LedManager {
            brightness: brightness.clamp(1, 100),
            state: LedState {
                color: LedColor::Red,
                mode: LedMode::On,
            },
            override_enabled: false,
            override_state: None,
        }
    }

    pub async fn set_led(&mut self, color: LedColor, mode: LedMode) {
        self.state = LedState { color, mode };
        if !self.override_enabled {
            self.apply_led(color, mode).await;
        }
    }

    pub async fn override_led(&mut self, color: LedColor, mode: LedMode) {
        self.override_enabled = true;
        self.override_state = Some(LedState { color, mode });
        self.apply_led(color, mode).await;
    }

    pub async fn clear_override(&mut self) {
        self.override_enabled = false;
        self.apply_led(self.state.color, self.state.mode).await;
    }

    async fn apply_led(&self, color: LedColor, mode: LedMode) {
        let trigger = match mode {
            LedMode::On => "default-on",
            LedMode::Heartbeat => "heartbeat",
        };

        let led_off = "none";

        match color {
            LedColor::Red => {
                self.write_led("rgb-red", trigger, self.brightness).await;
                self.write_led("rgb-green", led_off, 0).await;
                self.write_led("rgb-blue", led_off, 0).await;
            }
            LedColor::Green => {
                self.write_led("rgb-red", led_off, 0).await;
                self.write_led("rgb-green", trigger, self.brightness).await;
                self.write_led("rgb-blue", led_off, 0).await;
            }
            LedColor::Blue => {
                self.write_led("rgb-red", led_off, 0).await;
                self.write_led("rgb-green", led_off, 0).await;
                self.write_led("rgb-blue", trigger, self.brightness).await;
            }
            LedColor::Yellow => {
                self.write_led("rgb-red", trigger, self.brightness).await;
                self.write_led("rgb-green", trigger, self.brightness).await;
                self.write_led("rgb-blue", led_off, 0).await;
            }
            LedColor::White => {
                self.write_led("rgb-red", trigger, self.brightness).await;
                self.write_led("rgb-green", trigger, self.brightness).await;
                self.write_led("rgb-blue", trigger, self.brightness).await;
            }
            LedColor::Purple => {
                self.write_led("rgb-red", trigger, self.brightness).await;
                self.write_led("rgb-green", led_off, 0).await;
                self.write_led("rgb-blue", trigger, self.brightness).await;
            }
        }
    }

    async fn write_led(&self, led_name: &str, trigger: &str, brightness: u8) {
        let base_path = format!("/sys/class/leds/{}", led_name);

        // Write trigger
        let _ = write_to_file(format!("{}/trigger", base_path), trigger).await;

        // Read max brightness
        let max_brightness_str = read_from_file(format!("{}/max_brightness", base_path)).await;
        let max_brightness: u32 = max_brightness_str.trim().parse().unwrap_or(255);

        // Calculate and write brightness
        let scaled_brightness =
            ((brightness as f32 / 100.0) * max_brightness as f32).round() as u32;
        let _ = write_to_file(
            format!("{}/brightness", base_path),
            &scaled_brightness.to_string(),
        )
        .await;
    }
}

async fn write_to_file<P: AsRef<Path>>(path: P, data: &str) -> tokio::io::Result<()> {
    if let Ok(mut file) = OpenOptions::new().write(true).open(path.as_ref()).await {
        let _ = file.write_all(data.as_bytes()).await;
    }
    Ok(())
}

async fn read_from_file<P: AsRef<Path>>(path: P) -> String {
    if let Ok(mut file) = File::open(path).await {
        let mut contents = String::new();
        if file.read_to_string(&mut contents).await.is_ok() {
            return contents;
        }
    }
    "255".to_string()
}
