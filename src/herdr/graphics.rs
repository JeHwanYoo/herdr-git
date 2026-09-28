use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

const SOCKET_TIMEOUT: Duration = Duration::from_millis(700);
const RESPONSE_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphicsPlacement {
    pub column: u16,
    pub row: u16,
    pub columns: u16,
    pub rows: u16,
}

pub struct GraphicsSurface {
    socket: PathBuf,
    pane: String,
    cell: (u32, u32),
    stream: Option<UnixStream>,
}

impl GraphicsSurface {
    pub fn connect() -> Result<Self, String> {
        let socket = env::var_os("HERDR_SOCKET_PATH")
            .ok_or("HERDR_SOCKET_PATH is not set")?
            .into();
        let pane = env::var("HERDR_PANE_ID").map_err(|_| "HERDR_PANE_ID is not set")?;
        let mut surface = Self {
            socket,
            pane,
            cell: (0, 0),
            stream: None,
        };
        surface.refresh_cell_size()?;
        Ok(surface)
    }

    pub fn cell_size(&self) -> (u32, u32) {
        self.cell
    }

    pub fn refresh_cell_size(&mut self) -> Result<(), String> {
        let mut stream = self.open()?;
        let info = request(
            &mut stream,
            "pane.graphics.info",
            json!({ "pane_id": self.pane }),
        )?;
        self.cell = cell_size(&info)?;
        Ok(())
    }

    pub fn show_png(
        &mut self,
        png: &[u8],
        size: (u32, u32),
        placement: GraphicsPlacement,
    ) -> Result<(), String> {
        if self.stream.is_none() {
            let mut stream = self.open()?;
            request(
                &mut stream,
                "pane.graphics.stream",
                json!({ "pane_id": self.pane }),
            )?;
            self.stream = Some(stream);
        }
        let mut frame = frame_header(png.len(), size, placement);
        frame.extend_from_slice(png);
        let stream = self.stream.as_mut().expect("graphics stream is open");
        if let Err(error) = stream.write_all(&frame) {
            self.stream = None;
            return Err(format!("Herdr graphics stream failed: {error}"));
        }
        Ok(())
    }

    pub fn hide(&mut self) {
        self.stream = None;
    }

    fn open(&self) -> Result<UnixStream, String> {
        let stream = UnixStream::connect(&self.socket)
            .map_err(|error| format!("Could not connect to Herdr: {error}"))?;
        stream
            .set_read_timeout(Some(SOCKET_TIMEOUT))
            .and_then(|_| stream.set_write_timeout(Some(SOCKET_TIMEOUT)))
            .map_err(|error| error.to_string())?;
        Ok(stream)
    }
}

fn request(stream: &mut UnixStream, method: &str, params: Value) -> Result<Value, String> {
    let mut line = serde_json::to_vec(&json!({
        "id": "herdr-git",
        "method": method,
        "params": params,
    }))
    .map_err(|error| error.to_string())?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|error| format!("Herdr {method} failed: {error}"))?;
    let mut response = String::new();
    BufReader::new(stream.take(RESPONSE_LIMIT))
        .read_line(&mut response)
        .map_err(|error| format!("Herdr {method} failed: {error}"))?;
    parse_response(method, &response)
}

fn parse_response(method: &str, response: &str) -> Result<Value, String> {
    let mut response: Value = serde_json::from_str(response)
        .map_err(|error| format!("Herdr {method} returned invalid JSON: {error}"))?;
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| error.to_string(), str::to_owned);
        return Err(format!("Herdr {method}: {message}"));
    }
    response
        .get_mut("result")
        .map(Value::take)
        .ok_or_else(|| format!("Herdr {method} returned no result"))
}

fn cell_size(info: &Value) -> Result<(u32, u32), String> {
    let pixels = |key| {
        info.get(key)
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0)
    };
    let cell = (pixels("cell_width_px"), pixels("cell_height_px"));
    if (1..=128).contains(&cell.0) && (1..=256).contains(&cell.1) {
        Ok(cell)
    } else {
        Err("Herdr did not report the terminal cell size".to_owned())
    }
}

fn frame_header(length: usize, size: (u32, u32), placement: GraphicsPlacement) -> Vec<u8> {
    let mut header = json!({
        "format": "png",
        "image_width": size.0,
        "image_height": size.1,
        "data_length": length,
        "placement": {
            "viewport_col": placement.column,
            "viewport_row": placement.row,
            "grid_cols": placement.columns,
            "grid_rows": placement.rows,
        },
    })
    .to_string()
    .into_bytes();
    header.push(b'\n');
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_results_and_reports_disabled_graphics() {
        let info = parse_response(
            "pane.graphics.info",
            r#"{"id":"herdr-git","result":{"type":"pane_graphics_info","cell_width_px":9,"cell_height_px":20}}"#,
        )
        .unwrap();
        assert_eq!(cell_size(&info), Ok((9, 20)));
        assert_eq!(
            parse_response(
                "pane.graphics.info",
                r#"{"id":"t","error":{"code":"feature_disabled","message":"pane graphics require experimental.kitty_graphics"}}"#,
            ),
            Err(
                "Herdr pane.graphics.info: pane graphics require experimental.kitty_graphics"
                    .to_owned()
            )
        );
        assert!(cell_size(&json!({ "cell_width_px": 0, "cell_height_px": 20 })).is_err());
    }

    #[test]
    fn frames_carry_the_image_size_and_grid_placement() {
        let header = frame_header(
            42,
            (18, 80),
            GraphicsPlacement {
                column: 3,
                row: 7,
                columns: 2,
                rows: 4,
            },
        );
        assert_eq!(header.last(), Some(&b'\n'));
        let value: Value = serde_json::from_slice(&header).unwrap();
        assert_eq!(value["data_length"], 42);
        assert_eq!(value["image_height"], 80);
        assert_eq!(value["placement"]["viewport_row"], 7);
        assert_eq!(value["placement"]["grid_cols"], 2);
    }
}
