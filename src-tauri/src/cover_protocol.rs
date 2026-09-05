use crate::{
    app_state::AppState,
    commands::baidu_commands::remote_body_dir,
    services::{CloudManifestService, GameLibraryService},
};
use std::{fs, path::Path};
use tauri::{
    http::{header, Response, StatusCode},
    Manager, UriSchemeContext,
};

pub fn handle_cover_request<R: tauri::Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: tauri::http::Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let app = ctx.app_handle();
    let uri = request.uri();
    let path = uri.path().trim_start_matches('/');
    let not_found = || {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Vec::new())
            .unwrap_or_default()
    };

    let Some((kind, raw_identifier)) = path.split_once('/') else {
        return not_found();
    };
    let decoded_identifier = percent_decode(raw_identifier);
    let identifier = decoded_identifier.trim();
    if identifier.is_empty() {
        return not_found();
    }

    let file_path = match kind {
        "game" => {
            let state = app.state::<AppState>();
            let Ok(store) = state.store.lock() else {
                return not_found();
            };
            let cover = GameLibraryService::find(&store, identifier).and_then(|g| g.cover);
            let Some(cover) = cover else {
                return not_found();
            };
            let Ok(root) = state.library_root_path() else {
                return not_found();
            };
            let rel = Path::new(&cover.display_path);
            if rel.is_absolute() {
                return not_found();
            }
            root.join(rel)
        }
        "cloud" => {
            let Ok(base_data_dir) = app.path().app_data_dir() else {
                return not_found();
            };
            let cache_root = base_data_dir.join("cloud-manifest-cache");
            let mut resolved_cover = None;

            // 1. 标准远程路径映射
            if let Ok(remote_dir) = remote_body_dir(identifier) {
                let candidate = cache_root
                    .join(CloudManifestService::cache_folder_name(&remote_dir))
                    .join("cover.jpg");
                if candidate.is_file() {
                    resolved_cover = Some(candidate);
                }
            }

            // 2. 常见目录名前缀直接匹配
            if resolved_cover.is_none() {
                let candidate = cache_root
                    .join(format!("apps_GameSaver_games_{identifier}_body"))
                    .join("cover.jpg");
                if candidate.is_file() {
                    resolved_cover = Some(candidate);
                }
            }

            // 3. 容错遍历：检查 game.json / manifest.json 匹配 gameKey 或 gameUid
            if resolved_cover.is_none() && cache_root.is_dir() {
                if let Ok(entries) = fs::read_dir(&cache_root) {
                    for entry in entries.flatten() {
                        let dir = entry.path();
                        if !dir.is_dir() {
                            continue;
                        }
                        let cover_file = dir.join("cover.jpg");
                        if !cover_file.is_file() {
                            continue;
                        }
                        let matches_key = ["game.json", "manifest.json"].iter().any(|name| {
                            let file = dir.join(name);
                            if let Ok(content) = fs::read_to_string(&file) {
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
                                {
                                    if val.get("gameKey").and_then(|v| v.as_str())
                                        == Some(identifier)
                                        || val.get("gameUid").and_then(|v| v.as_str())
                                            == Some(identifier)
                                    {
                                        return true;
                                    }
                                }
                            }
                            false
                        });
                        if matches_key {
                            resolved_cover = Some(cover_file);
                            break;
                        }
                    }
                }
            }

            let Some(cover) = resolved_cover else {
                return not_found();
            };
            cover
        }
        _ => return not_found(),
    };

    if !file_path.is_file() {
        return not_found();
    }

    let Ok(bytes) = fs::read(&file_path) else {
        return not_found();
    };

    let mime = detect_mime(&bytes);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "public, max-age=604800, immutable")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(bytes)
        .unwrap_or_else(|_| not_found())
}

pub(crate) fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(h1), Some(h2)) = (h1, h2) {
                let hex_slice = [h1, h2];
                if let Ok(hex_str) = std::str::from_utf8(&hex_slice) {
                    if let Ok(byte) = u8::from_str_radix(hex_str, 16) {
                        bytes.push(byte);
                        continue;
                    }
                }
                bytes.push(b'%');
                bytes.push(h1);
                bytes.push(h2);
            } else {
                bytes.push(b'%');
                if let Some(h1) = h1 {
                    bytes.push(h1);
                }
            }
        } else if b == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

pub(crate) fn detect_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn detects_mime_from_image_magic_bytes() {
        let jpeg = &[0xff, 0xd8, 0xff, 0xe0];
        assert_eq!(super::detect_mime(jpeg), "image/jpeg");

        let png = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        assert_eq!(super::detect_mime(png), "image/png");

        let webp = b"RIFFxxxxWEBPxxxx";
        assert_eq!(super::detect_mime(webp), "image/webp");

        let unknown = b"unknown_bytes";
        assert_eq!(super::detect_mime(unknown), "image/jpeg");
    }

    #[test]
    fn percent_decode_decodes_cjk_and_symbols() {
        // "被囚禁的莉莉丝"
        let encoded_cjk = "%E8%A2%AB%E5%9B%9A%E7%A6%81%E7%9A%84%E8%8E%89%E8%8E%89%E4%B8%9D";
        assert_eq!(super::percent_decode(encoded_cjk), "被囚禁的莉莉丝");

        // "[g20240404]black market"
        let encoded_brackets = "%5Bg20240404%5Dblack%20market";
        assert_eq!(
            super::percent_decode(encoded_brackets),
            "[g20240404]black market"
        );

        // Plain text remains unchanged
        let plain = "test_game_key_123";
        assert_eq!(super::percent_decode(plain), "test_game_key_123");
    }
}
