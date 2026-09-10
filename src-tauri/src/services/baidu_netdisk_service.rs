use reqwest::blocking::Response;
use reqwest::blocking::{multipart, Client};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufWriter, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    time::Duration,
};

const API_BASE: &str = "https://pan.baidu.com";
const UPLOAD_API_BASE: &str = "https://d.pcs.baidu.com";
const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
const UPLOAD_CONCURRENCY: usize = 3;
const MAX_REQUEST_ATTEMPTS: usize = 3;
const TOKEN_REFRESH_LEEWAY_MS: u64 = 5 * 60 * 1000;
const TOKEN_FILE_NAME: &str = "baidu-netdisk-token.json";
const OAUTH_TOKEN_URL: &str = "https://openapi.baidu.com/oauth/2.0/token";

static TOKEN_REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaiduToken {
    #[serde(alias = "accessToken", alias = "access_token")]
    pub access_token: String,
    #[serde(alias = "expiresAt", alias = "expires_at")]
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(alias = "refresh_token")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaiduConnectionStatus {
    pub authorized: bool,
    pub token_path: Option<String>,
    pub expires_at: Option<u64>,
    pub expired: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFile {
    pub path: String,
    pub fs_id: u64,
    pub size: u64,
    pub md5: Option<String>,
    pub is_dir: bool,
    pub server_mtime: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RemoteFilePage {
    pub files: Vec<RemoteFile>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaiduQuota {
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub expires_soon: bool,
}

#[derive(Debug, Deserialize)]
struct PrecreateResponse {
    uploadid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LocateResponse {
    servers: Option<Vec<UploadServer>>,
    bak_servers: Option<Vec<UploadServer>>,
    host: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UploadServer {
    server: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FileListResponse {
    list: Option<Vec<FileListItem>>,
    has_more: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct FileListItem {
    #[serde(default)]
    path: String,
    #[serde(default)]
    fs_id: u64,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    md5: Option<String>,
    #[serde(default)]
    isdir: u8,
    #[serde(default)]
    server_mtime: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CreatedFileResponse {
    #[serde(default)]
    fs_id: u64,
    #[serde(default)]
    md5: Option<String>,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct QuotaResponse {
    #[serde(default)]
    total: u64,
    #[serde(default)]
    used: u64,
    #[serde(default)]
    free: u64,
    #[serde(default)]
    expire: bool,
}

#[derive(Debug, Deserialize)]
struct MetaResponse {
    list: Option<Vec<MetaItem>>,
}

#[derive(Debug, Deserialize)]
struct MetaItem {
    dlink: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

/// 在 access token 失效时重新走 OAuth 刷新所需的凭据。
///
/// 故意不实现 `Debug`，避免 AppKey/SecretKey 被格式化进日志或错误信息。
#[derive(Clone)]
struct RefreshContext {
    token_path: PathBuf,
    app_key: String,
    secret_key: String,
}

pub struct BaiduNetdiskClient {
    client: Client,
    /// access token 可在运行期刷新，因此用 `Mutex` 包裹以支持 `&self` 访问。
    token: Mutex<BaiduToken>,
    app_data_dir: PathBuf,
    refresh_context: Option<RefreshContext>,
}

impl BaiduNetdiskClient {
    pub fn load_from_app_data(app_data_dir: &Path) -> Result<Self, String> {
        Self::load_from_app_data_with_credentials(app_data_dir, None, None)
    }

    pub fn load_from_app_data_with_credentials(
        app_data_dir: &Path,
        app_key: Option<&str>,
        secret_key: Option<&str>,
    ) -> Result<Self, String> {
        let (token_path, token) = read_token(app_data_dir)?;
        let refresh_context = match (app_key, secret_key) {
            (Some(app_key), Some(secret_key))
                if !app_key.trim().is_empty() && !secret_key.trim().is_empty() =>
            {
                Some(RefreshContext {
                    token_path,
                    app_key: app_key.to_string(),
                    secret_key: secret_key.to_string(),
                })
            }
            _ => None,
        };
        let client = Self::from_parts(token, app_data_dir.to_path_buf(), refresh_context)?;
        if !client.token_is_stale() || client.refresh_token(false)? {
            return Ok(client);
        }
        // 没有刷新凭据，且磁盘上也没有更新的授权：只有在已完全过期时才报错，
        // 距离过期尚有时间时保持原来的「可用」判断。
        if client.access_token_expired() {
            return Err("百度网盘授权已过期，请先配置 AppKey 和 SecretKey 后重新授权".to_string());
        }
        Ok(client)
    }

    pub fn connection_status(app_data_dir: &Path) -> BaiduConnectionStatus {
        let path = token_paths(app_data_dir)
            .into_iter()
            .find(|path| path.is_file());
        let token = path
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|raw| serde_json::from_slice::<BaiduToken>(&raw).ok());
        let expires_at = token.as_ref().and_then(|token| token.expires_at);
        BaiduConnectionStatus {
            authorized: token
                .as_ref()
                .is_some_and(|token| !token.access_token.trim().is_empty()),
            token_path: path.map(|path| path.to_string_lossy().to_string()),
            expires_at,
            expired: expires_at.is_some_and(|value| value <= now_millis()),
            refresh_error: None,
        }
    }

    pub fn connection_status_with_credentials(
        app_data_dir: &Path,
        app_key: Option<&str>,
        secret_key: Option<&str>,
    ) -> BaiduConnectionStatus {
        let mut status = Self::connection_status(app_data_dir);
        if !status.authorized || app_key.is_none() || secret_key.is_none() {
            return status;
        }
        match Self::load_from_app_data_with_credentials(app_data_dir, app_key, secret_key) {
            Ok(_) => Self::connection_status(app_data_dir),
            Err(error) => {
                status.refresh_error = Some(error);
                status
            }
        }
    }

    pub fn save_token(app_data_dir: &Path, token: BaiduToken) -> Result<(), String> {
        let lock = TOKEN_REFRESH_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock
            .lock()
            .map_err(|_| "百度网盘 Token 刷新锁不可用".to_string())?;
        save_token_at(&app_data_dir.join(TOKEN_FILE_NAME), token)
    }

    /// 仅用于单元测试：构造一个不具备刷新能力、不落盘的客户端。
    #[cfg(test)]
    pub fn new(token: BaiduToken) -> Result<Self, String> {
        Self::from_parts(token, PathBuf::new(), None)
    }

    fn from_parts(
        token: BaiduToken,
        app_data_dir: PathBuf,
        refresh_context: Option<RefreshContext>,
    ) -> Result<Self, String> {
        if token.access_token.trim().is_empty() {
            return Err("百度网盘授权信息缺少 access token".to_string());
        }
        let client = Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(120))
            .user_agent("pan.baidu.com")
            .build()
            .map_err(|err| format!("创建百度网盘网络客户端失败：{err}"))?;
        Ok(Self {
            client,
            token: Mutex::new(token),
            app_data_dir,
            refresh_context,
        })
    }

    fn access_token(&self) -> Result<String, String> {
        Ok(self
            .token
            .lock()
            .map_err(|_| "百度网盘授权状态不可用".to_string())?
            .access_token
            .clone())
    }

    /// access token 是否已进入刷新窗口（距过期不足 5 分钟）或已过期。
    fn token_is_stale(&self) -> bool {
        self.token
            .lock()
            .map(|token| token_needs_refresh(token.expires_at))
            .unwrap_or(true)
    }

    fn access_token_expired(&self) -> bool {
        self.token
            .lock()
            .map(|token| {
                token
                    .expires_at
                    .is_some_and(|expires_at| expires_at <= now_millis())
            })
            .unwrap_or(true)
    }

    fn store_token(&self, token: BaiduToken) -> Result<(), String> {
        *self
            .token
            .lock()
            .map_err(|_| "百度网盘授权状态不可用".to_string())? = token;
        Ok(())
    }

    /// 刷新 access token，返回 `true` 表示授权已可用。
    ///
    /// - 未进入刷新窗口且 `force` 为假时直接返回 `true`（无需刷新）。
    /// - 持锁后会重读磁盘，别的进程已刷新并落盘时直接沿用，避免重复刷新。
    /// - 没有刷新凭据且磁盘上也没有更新时返回 `false`。
    /// - `force` 用于服务端明确回报鉴权失败的场景，此时跳过「即将过期」的判断。
    fn refresh_token(&self, force: bool) -> Result<bool, String> {
        if !force && !self.token_is_stale() {
            return Ok(true);
        }
        let lock = TOKEN_REFRESH_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock
            .lock()
            .map_err(|_| "百度网盘 Token 刷新锁不可用".to_string())?;

        let current = self.access_token()?;
        if let Ok((_, disk_token)) = read_token(&self.app_data_dir) {
            if !disk_token.access_token.trim().is_empty() && disk_token.access_token != current {
                self.store_token(disk_token)?;
                return Ok(true);
            }
        }
        if !force && !self.token_is_stale() {
            return Ok(true);
        }
        let Some(context) = self.refresh_context.clone() else {
            return Ok(false);
        };
        self.refresh_access_token(&context)?;
        Ok(true)
    }

    fn refresh_access_token(&self, context: &RefreshContext) -> Result<(), String> {
        let (refresh_token, fallback_expires_at, fallback_refresh_token) = {
            let token = self
                .token
                .lock()
                .map_err(|_| "百度网盘授权状态不可用".to_string())?;
            let refresh_token = token
                .refresh_token
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    "百度网盘授权即将过期，但没有 refresh token，请重新授权".to_string()
                })?
                .to_string();
            (refresh_token, token.expires_at, token.refresh_token.clone())
        };
        let response = self
            .client
            .get(OAUTH_TOKEN_URL)
            .query(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
                ("client_id", context.app_key.as_str()),
                ("client_secret", context.secret_key.as_str()),
            ])
            .send()
            .map_err(|error| format!("请求百度 Token 刷新失败：{error}"))?;
        let status = response.status();
        let body = response
            .text()
            .map_err(|error| format!("读取百度 Token 刷新响应失败：{error}"))?;
        let value = serde_json::from_str::<serde_json::Value>(&body)
            .map_err(|error| format!("百度 Token 刷新返回非 JSON：HTTP {status}，{error}"))?;
        let parsed = serde_json::from_value::<OAuthTokenResponse>(value.clone())
            .map_err(|error| format!("百度 Token 刷新响应格式无效：{error}"))?;
        if let Some(error_code) = parsed.error.as_deref() {
            let description = parsed
                .error_description
                .as_deref()
                .unwrap_or("未知授权错误");
            return Err(format!(
                "百度 Token 自动刷新失败：{description} ({error_code})"
            ));
        }
        if !status.is_success() {
            return Err(format!("百度 Token 自动刷新失败：HTTP {status}"));
        }
        let access_token = parsed
            .access_token
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "百度 Token 刷新响应缺少 access_token".to_string())?;
        let next_token = BaiduToken {
            access_token,
            expires_at: parsed
                .expires_in
                .map(|seconds| now_millis().saturating_add(seconds.saturating_mul(1000)))
                .or(fallback_expires_at),
            refresh_token: parsed
                .refresh_token
                .filter(|value| !value.trim().is_empty())
                .or(fallback_refresh_token),
        };
        save_token_at(&context.token_path, next_token.clone())?;
        self.store_token(next_token)
    }

    pub fn list(&self, remote_dir: &str) -> Result<Vec<RemoteFile>, String> {
        let mut start = 0usize;
        let mut result = Vec::new();
        loop {
            let page = self.list_page(remote_dir, start, 1000)?;
            let page_len = page.files.len();
            result.extend(page.files);
            if page_len == 0 || !page.has_more {
                break;
            }
            start = start.saturating_add(page_len);
        }
        Ok(result)
    }

    pub fn list_page(
        &self,
        remote_dir: &str,
        start: usize,
        limit: usize,
    ) -> Result<RemoteFilePage, String> {
        let url = format!("{API_BASE}/rest/2.0/xpan/file");
        let limit = limit.clamp(1, 1000);
        let body: FileListResponse = self.request_json(
            |client, token| {
                client
                    .get(&url)
                    .query(&[
                        ("method", "list"),
                        ("dir", remote_dir),
                        ("order", "name"),
                        ("start", &start.to_string()),
                        ("limit", &limit.to_string()),
                        ("web", "1"),
                        ("folder", "0"),
                        ("access_token", token),
                    ])
                    .send()
            },
            "读取百度网盘文件列表",
        )?;
        Ok(RemoteFilePage {
            files: body
                .list
                .unwrap_or_default()
                .into_iter()
                .map(|item| RemoteFile {
                    path: item.path,
                    fs_id: item.fs_id,
                    size: item.size,
                    md5: item.md5,
                    is_dir: item.isdir != 0,
                    server_mtime: item.server_mtime,
                })
                .collect(),
            has_more: body.has_more == Some(1),
        })
    }

    pub fn quota(&self) -> Result<BaiduQuota, String> {
        let body: QuotaResponse = self.request_json(
            |client, token| {
                client
                    .get(format!("{API_BASE}/api/quota"))
                    .query(&[
                        ("access_token", token),
                        ("checkfree", "1"),
                        ("checkexpire", "1"),
                    ])
                    .send()
            },
            "读取百度网盘空间",
        )?;
        Ok(BaiduQuota {
            total: body.total,
            used: body.used,
            free: body.free,
            expires_soon: body.expire,
        })
    }

    pub fn delete_file(&self, remote_path: &str) -> Result<(), String> {
        let filelist = serde_json::to_string(&[remote_path])
            .map_err(|error| format!("生成百度删除请求失败：{error}"))?;
        let _: serde_json::Value = self.request_json(
            |client, token| {
                client
                    .post(format!("{API_BASE}/rest/2.0/xpan/file"))
                    .query(&[
                        ("method", "filemanager"),
                        ("opera", "delete"),
                        ("access_token", token),
                    ])
                    .form(&[("async", "0"), ("filelist", filelist.as_str())])
                    .send()
            },
            "删除百度网盘本体包",
        )?;
        Ok(())
    }

    pub fn ensure_directory(&self, remote_dir: &str) -> Result<(), String> {
        let mut current = String::new();
        for component in remote_dir
            .split('/')
            .filter(|component| !component.is_empty())
        {
            current.push('/');
            current.push_str(component);
            match self.list(&current) {
                Ok(_) => continue,
                Err(error) if !error.contains("(-9)") && !error.contains("(-8)") => {
                    return Err(error)
                }
                Err(_) => {}
            }
            let url = format!("{API_BASE}/rest/2.0/xpan/file");
            let value = self.request_json_value(
                |client, token| {
                    client
                        .post(&url)
                        .query(&[("method", "create"), ("access_token", token)])
                        .form(&[("path", current.as_str()), ("isdir", "1"), ("rtype", "3")])
                        .send()
                },
                "创建百度网盘目录",
            )?;
            if value.get("errno").and_then(serde_json::Value::as_i64) == Some(-8) {
                continue;
            }
            let _: serde_json::Value = parse_value(value, "创建百度网盘目录")?;
        }
        Ok(())
    }

    pub fn upload_file(
        &self,
        local_path: &Path,
        remote_path: &str,
        on_progress: impl Fn(u8, &str) -> bool + Send,
    ) -> Result<RemoteFile, String> {
        let metadata =
            fs::metadata(local_path).map_err(|err| format!("读取待上传本体包失败：{err}"))?;
        let total_size = metadata.len();
        if total_size == 0 {
            return Err("不能上传空的本体包".to_string());
        }
        let block_md5 = block_md5_list(local_path, total_size)?;
        let block_list = serde_json::to_string(&block_md5)
            .map_err(|err| format!("生成百度分片清单失败：{err}"))?;
        let url = format!("{API_BASE}/rest/2.0/xpan/file");
        let precreate: PrecreateResponse = self.request_json(
            |client, token| {
                client
                    .post(&url)
                    .query(&[("method", "precreate"), ("access_token", token)])
                    .form(&[
                        ("path", remote_path),
                        ("size", &total_size.to_string()),
                        ("isdir", "0"),
                        ("autoinit", "1"),
                        ("block_list", &block_list),
                        ("rtype", "3"),
                    ])
                    .send()
            },
            "百度预创建",
        )?;
        let upload_id = precreate
            .uploadid
            .ok_or_else(|| "百度预创建未返回 uploadid".to_string())?;
        let url = format!("{UPLOAD_API_BASE}/rest/2.0/pcs/file");
        let located: LocateResponse = self.request_json(
            |client, token| {
                client
                    .get(&url)
                    .query(&[
                        ("method", "locateupload"),
                        ("appid", "250528"),
                        ("access_token", token),
                        ("path", remote_path),
                        ("uploadid", upload_id.as_str()),
                        ("upload_version", "2.0"),
                    ])
                    .send()
            },
            "定位百度上传服务器",
        )?;
        let host = located
            .servers
            .unwrap_or_default()
            .into_iter()
            .chain(located.bak_servers.unwrap_or_default())
            .find_map(|item| item.server)
            .or(located.host)
            .ok_or_else(|| "百度未返回可用上传服务器".to_string())?;
        let host = if host.starts_with("http") {
            host
        } else {
            format!("https://{host}")
        };
        let chunk_count = block_md5.len();
        let concurrency = UPLOAD_CONCURRENCY.min(chunk_count);
        let next_chunk = AtomicUsize::new(0);
        let completed_count = AtomicUsize::new(0);
        let aborted = AtomicBool::new(false);
        let first_error = Mutex::new(None::<String>);
        let progress_callback = Mutex::new(on_progress);

        std::thread::scope(|s| {
            for _ in 0..concurrency {
                s.spawn(|| {
                    let mut file = match fs::File::open(local_path) {
                        Ok(f) => f,
                        Err(err) => {
                            aborted.store(true, Ordering::Relaxed);
                            let mut err_guard = first_error.lock().unwrap();
                            if err_guard.is_none() {
                                *err_guard = Some(format!("打开待上传本体包失败：{err}"));
                            }
                            return;
                        }
                    };
                    while !aborted.load(Ordering::Relaxed) {
                        let index = next_chunk.fetch_add(1, Ordering::SeqCst);
                        if index >= chunk_count {
                            break;
                        }
                        let offset = index as u64 * CHUNK_SIZE;
                        let length = (total_size.saturating_sub(offset)).min(CHUNK_SIZE) as usize;
                        if let Err(err) = file.seek(std::io::SeekFrom::Start(offset)) {
                            aborted.store(true, Ordering::Relaxed);
                            let mut err_guard = first_error.lock().unwrap();
                            if err_guard.is_none() {
                                *err_guard = Some(format!("定位本体包分片偏移失败：{err}"));
                            }
                            break;
                        }
                        let mut bytes = vec![0u8; length];
                        if let Err(err) = file.read_exact(&mut bytes) {
                            aborted.store(true, Ordering::Relaxed);
                            let mut err_guard = first_error.lock().unwrap();
                            if err_guard.is_none() {
                                *err_guard = Some(format!("读取本体包分片失败：{err}"));
                            }
                            break;
                        }

                        let upload_url = format!("{host}/rest/2.0/pcs/superfile2");
                        let operation = format!("上传百度本体包分片 {}/{}", index + 1, chunk_count);
                        let upload_result: Result<serde_json::Value, String> = self.request_json(
                            |client, token| {
                                let form = multipart::Form::new().part(
                                    "file",
                                    multipart::Part::bytes(bytes.clone()).file_name("package.zip"),
                                );
                                client
                                    .post(&upload_url)
                                    .query(&[
                                        ("method", "upload"),
                                        ("access_token", token),
                                        ("type", "tmpfile"),
                                        ("path", remote_path),
                                        ("uploadid", upload_id.as_str()),
                                        ("upload_version", "2.0"),
                                        ("partseq", &index.to_string()),
                                    ])
                                    .multipart(form)
                                    .send()
                            },
                            &operation,
                        );

                        if let Err(err) = upload_result {
                            aborted.store(true, Ordering::Relaxed);
                            let mut err_guard = first_error.lock().unwrap();
                            if err_guard.is_none() {
                                *err_guard = Some(err);
                            }
                            break;
                        }

                        let finished = completed_count.fetch_add(1, Ordering::SeqCst) + 1;
                        let progress_pct = 10 + ((finished * 80) / chunk_count.max(1)) as u8;
                        let progress_msg =
                            format!("正在上传本体包分片 {}/{}", finished, chunk_count);
                        let keep_going = match progress_callback.lock() {
                            Ok(cb) => cb(progress_pct, &progress_msg),
                            Err(_) => false,
                        };
                        if !keep_going {
                            aborted.store(true, Ordering::Relaxed);
                            let mut err_guard = first_error.lock().unwrap();
                            if err_guard.is_none() {
                                *err_guard = Some("任务已取消".to_string());
                            }
                            break;
                        }
                    }
                });
            }
        });

        if let Some(err) = first_error.into_inner().unwrap() {
            return Err(err);
        }
        let url = format!("{API_BASE}/rest/2.0/xpan/file");
        let created: CreatedFileResponse = self.request_json(
            |client, token| {
                client
                    .post(&url)
                    .query(&[("method", "create"), ("access_token", token)])
                    .form(&[
                        ("path", remote_path),
                        ("size", &total_size.to_string()),
                        ("isdir", "0"),
                        ("uploadid", upload_id.as_str()),
                        ("block_list", &block_list),
                        ("rtype", "3"),
                        ("is_revision", "1"),
                    ])
                    .send()
            },
            "提交百度本体包",
        )?;
        if let Ok(cb) = progress_callback.lock() {
            cb(100, "本体包上传完成");
        }
        Ok(RemoteFile {
            path: remote_path.to_string(),
            fs_id: created.fs_id,
            size: created.size.max(total_size),
            md5: created.md5,
            is_dir: false,
            server_mtime: None,
        })
    }

    pub fn download_file(
        &self,
        remote: &RemoteFile,
        target_path: &Path,
        on_progress: impl Fn(u8, &str) -> bool,
    ) -> Result<String, String> {
        let fsids = serde_json::to_string(&[remote.fs_id])
            .map_err(|err| format!("生成百度下载请求失败：{err}"))?;
        let url = format!("{API_BASE}/rest/2.0/xpan/multimedia");
        let metadata: MetaResponse = self.request_json(
            |client, token| {
                client
                    .get(&url)
                    .query(&[
                        ("method", "filemetas"),
                        ("access_token", token),
                        ("fsids", fsids.as_str()),
                        ("dlink", "1"),
                    ])
                    .send()
            },
            "读取百度本体包下载地址",
        )?;
        let dlink = metadata
            .list
            .and_then(|list| list.into_iter().next())
            .and_then(|item| item.dlink)
            .ok_or_else(|| "百度未返回本体包下载地址".to_string())?;
        let mut response = self.send_raw(
            |client, token| client.get(&dlink).query(&[("access_token", token)]).send(),
            "下载百度本体包",
        )?;
        if !response.status().is_success() {
            return Err(format!("下载百度本体包失败：HTTP {}", response.status()));
        }
        let parent = target_path
            .parent()
            .ok_or_else(|| "本体包下载路径无父目录".to_string())?;
        fs::create_dir_all(parent).map_err(|err| format!("创建本体包下载目录失败：{err}"))?;
        let temporary = target_path.with_extension("download.tmp");
        let file = fs::File::create(&temporary)
            .map_err(|err| format!("创建本体包下载临时文件失败：{err}"))?;
        let mut output = BufWriter::with_capacity(4 * 1024 * 1024, file);
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let read = response
                .read(&mut buffer)
                .map_err(|err| format!("读取百度本体包失败：{err}"))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|err| format!("写入本体包下载文件失败：{err}"))?;
            hasher.update(&buffer[..read]);
            written = written.saturating_add(read as u64);
            if !on_progress(
                5 + ((written.min(remote.size) * 90 / remote.size.max(1)) as u8),
                &format!(
                    "正在下载本体包 {} / {} MB",
                    written / 1024 / 1024,
                    remote.size / 1024 / 1024
                ),
            ) {
                drop(output);
                let _ = fs::remove_file(&temporary);
                return Err("任务已取消".to_string());
            }
        }
        output
            .flush()
            .map_err(|err| format!("刷新本体包下载文件失败：{err}"))?;
        output
            .get_ref()
            .sync_all()
            .map_err(|err| format!("同步本体包下载文件失败：{err}"))?;
        drop(output);
        if written != remote.size {
            let _ = fs::remove_file(&temporary);
            return Err(format!(
                "本体包下载大小不匹配：{} / {}",
                written, remote.size
            ));
        }
        fs::rename(&temporary, target_path)
            .map_err(|err| format!("提交本体包下载文件失败：{err}"))?;
        let sha256_hex = hex::encode(hasher.finalize());
        on_progress(100, "本体包下载完成");
        Ok(sha256_hex)
    }

    fn send_with_retry<F>(
        &self,
        mut request: F,
        token: &str,
        operation: &str,
    ) -> Result<Response, String>
    where
        F: FnMut(&Client, &str) -> Result<Response, reqwest::Error>,
    {
        let mut last_error = None;
        for attempt in 0..MAX_REQUEST_ATTEMPTS {
            match request(&self.client, token) {
                Ok(response)
                    if response.status().is_success()
                        || !is_retryable_status(response.status()) =>
                {
                    return Ok(response)
                }
                Ok(response) => {
                    if attempt + 1 == MAX_REQUEST_ATTEMPTS {
                        return Ok(response);
                    }
                    last_error = Some(format!("HTTP {}", response.status()));
                }
                Err(error) => {
                    if attempt + 1 == MAX_REQUEST_ATTEMPTS {
                        return Err(format!("{operation}失败：{error}"));
                    }
                    last_error = Some(error.to_string());
                }
            }
            std::thread::sleep(Duration::from_millis(250 * (attempt as u64 + 1)));
        }
        Err(format!(
            "{operation}失败：{}",
            last_error.unwrap_or_else(|| "未知网络错误".to_string())
        ))
    }

    /// 发送请求并返回 JSON 响应体（不做 errno 校验）。
    ///
    /// 遇到鉴权失败（HTTP 401 或百度返回的鉴权类 `errno`）时刷新授权后重放一次，
    /// 让长任务不会因为 access token 中途到期而整体失败。
    fn request_json_value<F>(
        &self,
        mut build: F,
        operation: &str,
    ) -> Result<serde_json::Value, String>
    where
        F: FnMut(&Client, &str) -> Result<Response, reqwest::Error>,
    {
        let mut refreshed = false;
        loop {
            let token = self.access_token()?;
            let response = self.send_with_retry(&mut build, &token, operation)?;
            let status = response.status();
            let body = response
                .text()
                .map_err(|error| format!("{operation}读取响应失败：{error}"))?;
            let value = serde_json::from_str::<serde_json::Value>(&body)
                .map_err(|_| format!("{operation}返回非 JSON：HTTP {status}"))?;
            if !refreshed && is_auth_failure(status, &value) {
                refreshed = true;
                if matches!(self.refresh_token(true), Ok(true)) {
                    continue;
                }
            }
            return Ok(value);
        }
    }

    fn request_json<T, F>(&self, build: F, operation: &str) -> Result<T, String>
    where
        T: for<'de> Deserialize<'de>,
        F: FnMut(&Client, &str) -> Result<Response, reqwest::Error>,
    {
        let value = self.request_json_value(build, operation)?;
        parse_value(value, operation)
    }

    /// 发送不解析 JSON 的请求（例如直链下载）；遇到 HTTP 401 会刷新授权后重放一次。
    fn send_raw<F>(&self, mut build: F, operation: &str) -> Result<Response, String>
    where
        F: FnMut(&Client, &str) -> Result<Response, reqwest::Error>,
    {
        let mut refreshed = false;
        loop {
            let token = self.access_token()?;
            let response = self.send_with_retry(&mut build, &token, operation)?;
            if response.status().as_u16() == 401 && !refreshed {
                refreshed = true;
                if matches!(self.refresh_token(true), Ok(true)) {
                    continue;
                }
            }
            return Ok(response);
        }
    }
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
}

/// 判断响应是否表示「授权失效」——这类失败只有在刷新 token 后重放才可能成功。
///
/// 百度网盘的接口在 token 失效时不一定返回 401：常见的是 HTTP 200 配合
/// `errno` 为 `-6`（无权限）、`110`（token 无效）或 `111`（token 已过期）。
fn is_auth_failure(status: reqwest::StatusCode, value: &serde_json::Value) -> bool {
    if status.as_u16() == 401 {
        return true;
    }
    if matches!(
        value.get("errno").and_then(serde_json::Value::as_i64),
        Some(-6) | Some(110) | Some(111)
    ) {
        return true;
    }
    matches!(
        value.get("error").and_then(serde_json::Value::as_str),
        Some("invalid_token") | Some("expired_token")
    )
}

fn token_paths(app_data_dir: &Path) -> Vec<std::path::PathBuf> {
    let mut paths = vec![app_data_dir.join(TOKEN_FILE_NAME)];
    if let Some(parent) = app_data_dir.parent() {
        let legacy = parent.join("com.gamesaver.desktop").join(TOKEN_FILE_NAME);
        if !paths.iter().any(|path| path == &legacy) {
            paths.push(legacy);
        }
    }
    paths
}

fn read_token(app_data_dir: &Path) -> Result<(PathBuf, BaiduToken), String> {
    let Some(path) = token_paths(app_data_dir)
        .into_iter()
        .find(|path| path.is_file())
    else {
        return Err("未找到百度网盘授权信息，请先完成百度网盘授权".to_string());
    };
    let raw = fs::read(&path).map_err(|err| format!("读取百度网盘授权信息失败：{err}"))?;
    let token = serde_json::from_slice::<BaiduToken>(&raw)
        .map_err(|err| format!("解析百度网盘授权信息失败：{err}"))?;
    Ok((path, token))
}

fn token_needs_refresh(expires_at: Option<u64>) -> bool {
    expires_at.is_some_and(|value| value <= now_millis().saturating_add(TOKEN_REFRESH_LEEWAY_MS))
}

fn save_token_at(path: &Path, token: BaiduToken) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "百度 Token 路径无父目录".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("创建百度 Token 目录失败：{error}"))?;
    let bytes = serde_json::to_vec_pretty(&token)
        .map_err(|error| format!("序列化百度 Token 失败：{error}"))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = parent.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple()));
    let backup = parent.join(format!(".{name}.bak-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temporary)
            .map_err(|error| format!("创建百度 Token 临时文件失败：{error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("写入百度 Token 临时文件失败：{error}"))?;
        file.sync_all()
            .map_err(|error| format!("刷新百度 Token 临时文件失败：{error}"))?;
        let had_token = path.exists();
        if had_token {
            fs::rename(path, &backup).map_err(|error| format!("暂存百度 Token 失败：{error}"))?;
        }
        if let Err(error) = fs::rename(&temporary, path) {
            if had_token {
                let _ = fs::rename(&backup, path);
            }
            return Err(format!("提交百度 Token 失败：{error}"));
        }
        if had_token {
            let _ = fs::remove_file(&backup);
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn block_md5_list(path: &Path, total_size: u64) -> Result<Vec<String>, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("打开本体包失败：{err}"))?;
    let mut result = Vec::new();
    let mut remaining = total_size;
    let mut buffer = vec![0u8; CHUNK_SIZE as usize];
    while remaining > 0 {
        let length = remaining.min(CHUNK_SIZE) as usize;
        file.read_exact(&mut buffer[..length])
            .map_err(|err| format!("读取本体包分片失败：{err}"))?;
        result.push(md5_hex(&buffer[..length]));
        remaining -= length as u64;
    }
    Ok(result)
}

fn parse_value<T: for<'de> Deserialize<'de>>(
    value: serde_json::Value,
    operation: &str,
) -> Result<T, String> {
    if let Some(errno) = value
        .get("errno")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value != 0)
    {
        let message = value
            .get("errmsg")
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("error_msg").and_then(serde_json::Value::as_str))
            .unwrap_or("未知错误");
        return Err(format!("{operation}失败：{message} ({errno})"));
    }
    if let Some(error_code) = value
        .get("error_code")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value != 0)
    {
        let message = value
            .get("error_description")
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("error_msg").and_then(serde_json::Value::as_str))
            .unwrap_or("未知错误");
        return Err(format!("{operation}失败：{message} ({error_code})"));
    }
    serde_json::from_value(value).map_err(|err| format!("{operation}响应格式无效：{err}"))
}

fn md5_transform(state: &mut [u32; 4], block: &[u8; 64]) {
    let shifts = [7u32, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let constants = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut words = [0u32; 16];
    for (index, word) in words.iter_mut().enumerate() {
        *word = u32::from_le_bytes(block[index * 4..index * 4 + 4].try_into().unwrap());
    }
    let original = *state;
    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    for index in 0..64 {
        let (function, word_index, shift) = if index < 16 {
            ((b & c) | ((!b) & d), index, shifts[index % 4])
        } else if index < 32 {
            (
                (d & b) | ((!d) & c),
                (5 * index + 1) % 16,
                shifts[4 + index % 4],
            )
        } else if index < 48 {
            (b ^ c ^ d, (3 * index + 5) % 16, shifts[8 + index % 4])
        } else {
            (c ^ (b | (!d)), (7 * index) % 16, shifts[12 + index % 4])
        };
        let next = a
            .wrapping_add(function)
            .wrapping_add(constants[index])
            .wrapping_add(words[word_index])
            .rotate_left(shift);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(next);
    }
    state[0] = original[0].wrapping_add(a);
    state[1] = original[1].wrapping_add(b);
    state[2] = original[2].wrapping_add(c);
    state[3] = original[3].wrapping_add(d);
}

fn md5_hex(input: &[u8]) -> String {
    let mut state = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
    let bit_length = (input.len() as u64).saturating_mul(8);
    for chunk in input.chunks_exact(64) {
        md5_transform(&mut state, chunk.try_into().unwrap());
    }
    let rem = &input[(input.len() / 64) * 64..];
    let mut tail = [0u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;
    if rem.len() < 56 {
        tail[56..64].copy_from_slice(&bit_length.to_le_bytes());
        md5_transform(&mut state, tail[..64].try_into().unwrap());
    } else {
        tail[120..128].copy_from_slice(&bit_length.to_le_bytes());
        md5_transform(&mut state, tail[..64].try_into().unwrap());
        md5_transform(&mut state, tail[64..128].try_into().unwrap());
    }
    state
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{is_auth_failure, md5_hex, token_needs_refresh, BaiduNetdiskClient, BaiduToken};
    use std::fs;

    /// 建一个独立的双层临时目录，避免命中 legacy 路径下真实存在的 token。
    fn test_token_dir() -> (std::path::PathBuf, std::path::PathBuf) {
        let base =
            std::env::temp_dir().join(format!("gamesaver-token-test-{}", uuid::Uuid::new_v4()));
        let app_data = base.join("app-data");
        fs::create_dir_all(&app_data).expect("create temp token dir");
        (base, app_data)
    }

    fn token(access: &str, expires_at: Option<u64>) -> BaiduToken {
        BaiduToken {
            access_token: access.to_string(),
            expires_at,
            refresh_token: Some("refresh".to_string()),
        }
    }

    #[test]
    fn auth_failure_detection_covers_http_and_errno() {
        let ok = reqwest::StatusCode::OK;
        for errno in [-6i64, 110, 111] {
            assert!(
                is_auth_failure(ok, &serde_json::json!({ "errno": errno })),
                "errno {errno} 应判为鉴权失败"
            );
        }
        for error in ["invalid_token", "expired_token"] {
            assert!(
                is_auth_failure(ok, &serde_json::json!({ "error": error })),
                "{error} 应判为鉴权失败"
            );
        }
        assert!(is_auth_failure(
            reqwest::StatusCode::UNAUTHORIZED,
            &serde_json::json!({ "errno": 0 })
        ));
        // 普通业务错误不应触发刷新重放。
        assert!(!is_auth_failure(ok, &serde_json::json!({ "errno": 0 })));
        assert!(!is_auth_failure(ok, &serde_json::json!({ "errno": -9 })));
        assert!(!is_auth_failure(ok, &serde_json::json!({})));
    }

    #[test]
    fn refresh_without_credentials_reports_unavailable() {
        let (base, app_data) = test_token_dir();
        let client = BaiduNetdiskClient::from_parts(
            token("stale", Some(super::now_millis().saturating_sub(1))),
            app_data,
            None,
        )
        .expect("client");
        assert!(
            !client.refresh_token(true).expect("refresh call"),
            "没有凭据且磁盘无更新时应报告无法刷新"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn refresh_picks_up_token_written_by_another_process() {
        let (base, app_data) = test_token_dir();
        let path = app_data.join(super::TOKEN_FILE_NAME);
        let stale = token(
            "stale-token",
            Some(super::now_millis().saturating_sub(1000)),
        );
        super::save_token_at(&path, stale.clone()).expect("seed token");
        let client = BaiduNetdiskClient::from_parts(stale, app_data, None).expect("client");

        // 模拟另一个进程刷新后落盘。
        super::save_token_at(
            &path,
            token(
                "fresh-token",
                Some(super::now_millis().saturating_add(3_600_000)),
            ),
        )
        .expect("write refreshed token");

        assert!(client.refresh_token(true).expect("refresh call"));
        assert_eq!(client.access_token().expect("access token"), "fresh-token");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn client_requires_access_token() {
        let result = BaiduNetdiskClient::new(super::BaiduToken {
            access_token: String::new(),
            expires_at: None,
            refresh_token: None,
        });
        assert!(result.is_err());
    }

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            md5_hex(b"abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            md5_hex(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
        assert_eq!(
            md5_hex(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
        assert_eq!(md5_hex(b"GameSaver"), "f28205fc3f14a3b8bfa43c894db2a24b");
    }

    #[test]
    fn token_refresh_window_is_five_minutes() {
        let now = super::now_millis();
        assert!(!token_needs_refresh(Some(
            now.saturating_add(5 * 60 * 1000 + 1)
        )));
        assert!(token_needs_refresh(Some(
            now.saturating_add(5 * 60 * 1000 - 1)
        )));
        assert!(token_needs_refresh(Some(now.saturating_sub(1))));
        assert!(!token_needs_refresh(None));
    }

    #[test]
    fn token_accepts_legacy_and_current_field_names() {
        let token = serde_json::from_str::<BaiduToken>(
            r#"{"accessToken":"access","expiresAt":123,"refreshToken":"refresh"}"#,
        )
        .expect("legacy token fields should deserialize");
        assert_eq!(token.access_token, "access");
        assert_eq!(token.expires_at, Some(123));
        assert_eq!(token.refresh_token.as_deref(), Some("refresh"));

        let token = serde_json::from_str::<BaiduToken>(
            r#"{"access_token":"access","expires_at":123,"refresh_token":"refresh"}"#,
        )
        .expect("current token fields should deserialize");
        assert_eq!(token.access_token, "access");
        assert_eq!(token.expires_at, Some(123));
        assert_eq!(token.refresh_token.as_deref(), Some("refresh"));
    }
}
