//! Reading files the user picks, on desktop and in the browser alike.
use dioxus::html::FileData;

/// The file's bytes, or `None` when it is larger than `limit`.
///
/// Browser File objects are immutable and report their exact size. The Dioxus
/// 0.7 web byte_stream implementation casts an ArrayBuffer to Uint8Array and
/// panics; read_bytes uses FileReader and constructs the typed array correctly.
pub(crate) async fn read_bounded(
    file: &FileData,
    limit: usize,
) -> Result<Option<Vec<u8>>, dioxus::CapturedError> {
    if file.size() > limit as u64 {
        return Ok(None);
    }
    #[cfg(target_arch = "wasm32")]
    let bytes = file.read_bytes().await?.to_vec();
    #[cfg(not(target_arch = "wasm32"))]
    let bytes = {
        use futures_util::StreamExt;
        let mut stream = file.byte_stream();
        let mut bytes = Vec::new();
        while let Some(part) = stream.next().await {
            let part = part?;
            if bytes.len() + part.len() > limit {
                return Ok(None);
            }
            bytes.extend_from_slice(&part);
        }
        bytes
    };
    Ok((bytes.len() <= limit).then_some(bytes))
}
