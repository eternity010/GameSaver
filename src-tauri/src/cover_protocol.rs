use crate::{
    app_state::AppState,
    commands::{baidu_commands::remote_body_dir, game_commands::safe_cover_path},
    domain::is_safe_path_segment,
    services::{CloudManifestService, CoverCaptureService, GameLibraryService},
};
use std::{
    fs,
    path::{Path, PathBuf},
};
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

    let Some((kind, raw_identifier)) = parse_cover_request_path(path) else {
        return not_found();
    };
    let identifier = raw_identifier.as_str();

    let file_path = match kind {
        "game" => {
            let state = app.state::<AppState>();
            // 只在取封面字段时持锁，后面的路径解析不必占着 store。
            let display_path = {
                let Ok(store) = state.store.lock() else {
                    return not_found();
                };
                let Some(cover) =
                    GameLibraryService::find(&store, identifier).and_then(|g| g.cover)
                else {
                    return not_found();
                };
                cover.display_path
            };
            let Ok(root) = state.library_root_path() else {
                return not_found();
            };
            let Some(path) = resolve_game_cover_path(&root, identifier, &display_path) else {
                return not_found();
            };
            path
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
        "capture" => {
            let Some(path) = CoverCaptureService::capture_path(identifier) else {
                return not_found();
            };
            path
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
        .header(
            header::CACHE_CONTROL,
            if kind == "capture" {
                "no-store"
            } else {
                "public, max-age=604800, immutable"
            },
        )
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(bytes)
        .unwrap_or_else(|_| not_found())
}

/// 从请求路径解析出 `(kind, identifier)`；路径形状或标识符不合法时返回 `None`。
///
/// 三个分支都会把 `identifier` 拼进路径（库根、缓存目录名），所以段里的目录分隔符
/// 与 `..` 必须在这里一次拦掉 —— 百分号编码的 `%2F` 解出来同样是分隔符，一并按此
/// 处理。校验收在这一处，免得每个分支各写一份、强度还不一致。
fn parse_cover_request_path(path: &str) -> Option<(&str, String)> {
    let (kind, raw_identifier) = path.split_once('/')?;
    let decoded = percent_decode(raw_identifier);
    let identifier = decoded.trim();
    if !is_safe_path_segment(identifier) {
        return None;
    }
    Some((kind, identifier.to_string()))
}

/// 解析 `game` 分支要读的封面文件路径。
///
/// 单独抽出来是为了能被单测覆盖：`handle_cover_request` 依赖 `UriSchemeContext`，
/// 单测里构造不出来，而「`..` 必须被拒」恰恰是这里最不该只靠人眼看的一条。
///
/// 守卫走命令层同一份 `safe_cover_path`，避免同一个不变量出现两种强度。
fn resolve_game_cover_path(root: &Path, game_uid: &str, display_path: &str) -> Option<PathBuf> {
    safe_cover_path(root, game_uid, display_path).ok()
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
    fn request_path_rejects_identifiers_that_could_walk_out_of_a_root() {
        // 正常形态照旧通过，中文与空格是合法的 gameKey。
        assert_eq!(
            super::parse_cover_request_path("game/409fedc3-aee4-4e6d-a584-f9ef01fa9b5b"),
            Some(("game", "409fedc3-aee4-4e6d-a584-f9ef01fa9b5b".to_string()))
        );
        assert_eq!(
            super::parse_cover_request_path("cloud/%E8%82%89%E9%81%8A%E3%81%B3%20ver1.0.7"),
            Some(("cloud", "肉遊び ver1.0.7".to_string()))
        );

        // 百分号编码的分隔符解出来就是分隔符，必须与裸分隔符同样拒掉。
        for evil in [
            "game/..",
            "game/../other",
            "game/%2E%2E%2Fother",
            "cloud/a%2Fb",
            "game/a%5Cb",
            "game/",
            "game",
            "game/%20",
        ] {
            assert!(
                super::parse_cover_request_path(evil).is_none(),
                "accepted unsafe request path: {evil:?}"
            );
        }
    }

    #[test]
    fn rejects_cover_paths_that_escape_the_library_root() {
        use std::path::Path;

        let root = Path::new("C:/GameSaverLibrary");
        let uid = "409fedc3-aee4-4e6d-a584-f9ef01fa9b5b";
        let valid = "covers/409fedc3-aee4-4e6d-a584-f9ef01fa9b5b/86ccfbac/display.jpg";

        let resolved =
            super::resolve_game_cover_path(root, uid, valid).expect("valid cover path rejected");
        assert!(
            resolved.starts_with(root),
            "resolved path left the library root: {resolved:?}"
        );

        for evil in [
            "../../../Windows/win.ini",
            "..\\..\\Windows\\win.ini",
            "covers/../other-game/display.jpg",
            "covers/409fedc3-aee4-4e6d-a584-f9ef01fa9b5b/../../outside.jpg",
            "/absolute/display.jpg",
            "C:/absolute/display.jpg",
            "other-game/86ccfbac/display.jpg",
        ] {
            assert!(
                super::resolve_game_cover_path(root, uid, evil).is_none(),
                "accepted escaping cover path: {evil:?}"
            );
        }
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
