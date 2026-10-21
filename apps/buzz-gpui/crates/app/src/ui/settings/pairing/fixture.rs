//! Development only: `BUZZ_DEV_PAIRING_FIXTURE=<file>` replaces the
//! native session with buzz-app's browser fixture
//! (`tests/browser/pairing-fixture.tsx`), so every step can be shown
//! without a phone. The file holds the status as buzz-app's IPC sends it,
//! with the QR's URI in place of its SVG, e.g. `{"phase":"code",
//! "code":"001234","codeEntry":true}` or `{"phase":"qr","uri":"..."}`.
//! `{"phase":"unavailable"}` shows the card as pairing-unavailable.
//! Writing the file moves the card along; the fixture writes it too,
//! as buzz-app's does on start, confirm, deny and cancel.

use super::client::Native;
use buzz_pair_mobile::{QrMatrix, Status};
use futures::FutureExt;
use futures::future::BoxFuture;
use gpui_kit::App;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

/// Shown when the file names no URI.
const URI: &str = "nostrpair://3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d\
                   ?secret=8c3a6f1e2d4b5a69788796a5b4c3d2e1f0a1b2c3d4e5f60718293a4b5c6d7e8f\
                   &relay=wss%3A%2F%2Flocalhost%3A53443%2Fpair&v=1";

pub(super) struct Fixture {
    file: PathBuf,
    /// The last code drawn, so polling keeps one matrix (and one reveal).
    qr: RefCell<Option<(String, Arc<QrMatrix>)>>,
}

impl Fixture {
    pub(super) fn from_env() -> Option<Self> {
        let file = std::env::var_os("BUZZ_DEV_PAIRING_FIXTURE").filter(|file| !file.is_empty())?;
        Some(Self {
            file: file.into(),
            qr: RefCell::new(None),
        })
    }

    fn read(&self) -> Value {
        std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| json!({"phase": "connecting"}))
    }

    fn write(&self, value: Value) {
        let _ = std::fs::write(&self.file, value.to_string());
    }

    fn phase(&self) -> String {
        self.read()["phase"].as_str().unwrap_or_default().to_owned()
    }

    fn matrix(&self, uri: &str) -> Arc<QrMatrix> {
        let mut cached = self.qr.borrow_mut();
        match &*cached {
            Some((shown, matrix)) if shown == uri => matrix.clone(),
            _ => {
                let matrix = Arc::new(QrMatrix::encode(uri).expect("fixture URI encodes"));
                *cached = Some((uri.to_owned(), matrix.clone()));
                matrix
            }
        }
    }
}

impl Native for Fixture {
    fn available(&self) -> bool {
        self.phase() != "unavailable"
    }

    fn account(&self, cx: &App) -> Result<String, String> {
        super::client::account(cx)
    }

    /// Shows a QR, unless the file holds setup at "connecting".
    fn start(&self, _: &str, _: &str, _: &str, _: &App) -> Result<(), String> {
        if self.phase() != "connecting" {
            self.write(json!({"phase": "qr", "uri": URI}));
        }
        Ok(())
    }

    fn status(&self, _: &str) -> Result<Status, String> {
        let value = self.read();
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_owned();
        Ok(match value["phase"].as_str().unwrap_or_default() {
            "qr" => {
                let uri = value["uri"].as_str().unwrap_or(URI);
                Status::Qr {
                    matrix: self.matrix(uri),
                    uri: uri.to_owned(),
                }
            }
            "code" => Status::Code {
                code: text("code"),
                code_entry: value["codeEntry"].as_bool().unwrap_or(true),
            },
            "transferring" => Status::Transferring,
            "cancelling" => Status::Cancelling,
            "complete" => Status::Complete,
            "expired" => Status::Expired,
            "uncertain" => Status::Uncertain,
            "error" => Status::Error {
                message: text("message"),
            },
            "cancelled" => Status::Cancelled,
            "fail" => return Err("Fixture status failed.".into()),
            _ => Status::Connecting,
        })
    }

    fn confirm(&self, _: &str) -> Result<(), String> {
        self.write(json!({"phase": "transferring"}));
        Ok(())
    }

    fn deny(&self, _: &str) -> Result<(), String> {
        self.write(json!({"phase": "cancelled"}));
        Ok(())
    }

    fn cancel(&self, _: &str, _: &App) -> BoxFuture<'static, Result<Status, String>> {
        let phase = self.phase();
        if phase == "transferring" {
            self.write(json!({"phase": "uncertain"}));
        } else if !["uncertain", "complete", "error", "unavailable"].contains(&phase.as_str()) {
            self.write(json!({"phase": "cancelled"}));
        }
        let status = self.status("");
        futures::future::ready(status).boxed()
    }
}
