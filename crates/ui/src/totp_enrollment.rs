//! Trusted, local-only authenticator enrollment presentation. No remote QR
//! service, data URL, seed-bearing HTML attribute, or downloaded image is used.
use a206_core::{ErrorKind, ErrorReport};
use fc_dioxus::prelude::*;
use qrcode::{Color, QrCode};
use zeroize::Zeroizing;

/// Geometry is sensitive too: it encodes the pending authenticator seed.
/// It is held only with the pending enrollment and never implements Debug.
pub(crate) struct Graphic {
    path: Zeroizing<String>,
    size: usize,
}
impl Graphic {
    pub(crate) fn encode(uri: &str) -> Result<Self, ErrorReport> {
        let code = QrCode::new(uri.as_bytes()).map_err(|error| {
            let report = ErrorReport::new(
                ErrorKind::InvalidRequest,
                "Could not generate authenticator QR code",
            )
            .caused_by(ErrorReport::new(
                ErrorKind::InvalidRequest,
                error.to_string(),
            ));
            crate::error::record("vault_totp_qr", &report);
            report
        })?;
        let mut path = String::new();
        for y in 0..code.width() {
            for x in 0..code.width() {
                if code[(x, y)] == Color::Dark {
                    // Four modules of quiet zone on every side. All coordinates
                    // come from the encoder; no URI text enters SVG attributes.
                    path.push_str(&format!("M{},{}h1v1h-1z", x + 4, y + 4));
                }
            }
        }
        Ok(Self {
            path: Zeroizing::new(path),
            size: code.width() + 8,
        })
    }
}
pub(crate) fn view(uri: &str, manual_key: &str, graphic: &Result<Graphic, ErrorReport>) -> Element {
    let terminal = crate::terminal_mode();
    rsx! {
        if !terminal {
            p { "Scan this QR code with your authenticator app, then enter the code it shows." }
            match graphic {
                Ok(graphic) => rsx! {
                    svg { class: "a206-totp-qr", role: "img", "aria-label": "Authenticator enrollment QR code", view_box: "0 0 {graphic.size} {graphic.size}",
                        // QR is a machine-readable graphic. Fixed black on white
                        // preserves scan contrast in dark and forced-color themes.
                        rect { width: "{graphic.size}", height: "{graphic.size}", fill: "white" }
                        path { d: "{graphic.path.as_str()}", fill: "black" }
                    }
                },
                Err(report) => rsx! { crate::error::ErrorNotice { report: report.clone(), title: "QR code unavailable; use the setup key below".to_owned() } },
            }
        }
        Disclosure { summary: rsx! { "Can't scan? Enter a setup key instead" }, open: terminal || graphic.is_err(), boxed: true,
            p { "In your authenticator, add a time-based account named Agent 206 with this key. Codes have six digits and refresh every 30 seconds." }
            DescriptionList { layout: DlLayout::Stacked,
                DescriptionItem { term: rsx! { "Setup key" }, code { class: "a206-secret-text", "{manual_key}" } }
            }
            Disclosure { summary: rsx! { "Authenticator app setup URI" },
                p { "Use this URI only in your authenticator app. It contains the same secret as the QR code." }
                code { class: "a206-secret-text", "{uri}" }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rendered_geometry_decodes_to_the_complete_authenticator_uri() {
        let seed = uuid::Uuid::new_v4().simple().to_string().to_uppercase();
        let uri = format!(
            "otpauth://totp/Agent%20206:test?secret={seed}&issuer=Agent%20206&algorithm=SHA1&digits=6&period=30"
        );
        let graphic = Graphic::encode(&uri).unwrap();
        assert!(!graphic.path.contains(&seed));
        // Rasterize the actual SVG path, then decode with an independent QR
        // decoder. This catches broken geometry, quiet zones and URI encoding.
        let mut dark = std::collections::HashSet::new();
        for command in graphic.path.split('M').skip(1) {
            let (x, y) = command
                .strip_suffix("h1v1h-1z")
                .unwrap()
                .split_once(',')
                .unwrap();
            dark.insert((x.parse::<usize>().unwrap(), y.parse::<usize>().unwrap()));
        }
        let scale = 4;
        let mut image = rqrr::PreparedImage::prepare_from_bitmap(
            graphic.size * scale,
            graphic.size * scale,
            |x, y| dark.contains(&(x / scale, y / scale)),
        );
        let grids = image.detect_grids();
        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].decode().unwrap().1, uri);
    }
    #[test]
    fn oversized_uri_fails_without_exposing_its_content() {
        let text = uuid::Uuid::new_v4().to_string().repeat(1000);
        let report = Graphic::encode(&text).err().unwrap();
        assert!(!report.detail().contains(&text));
    }
}
