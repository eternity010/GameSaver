use reqwest::blocking::{Client, multipart};
use reqwest::blocking::Response;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

const API_BASE: &str = "https://pan.baidu.com";
const UPLOAD_API_BASE: &str = "https://d.pcs.baidu.com";
const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
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
struct UploadServer { server: Option<String> }

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
struct MetaItem { dlink: Option<String> }

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

pub struct BaiduNetdiskClient {
    client: Client,
    token: BaiduToken,
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
        let (mut token_path, token) = read_token(app_data_dir)?;
        let mut client = Self::new(token)?;
        if !token_needs_refresh(client.token.expires_at) {
            return Ok(client);
        }

        let has_credentials = app_key.is_some_and(|value| !value.trim().is_empty())
            && secret_key.is_some_and(|value| !value.trim().is_empty());
        if !has_credentials {
            if client.token.expires_at.is_some_and(|expires_at| expires_at <= now_millis()) {
                return Err("百度网盘授权已过期，请先配置 AppKey 和 SecretKey 后重新授权".to_string());
            }
            return Ok(client);
        }

        let lock = TOKEN_REFRESH_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().map_err(|_| "百度网盘 Token 刷新锁不可用".to_string())?;
        let (latest_path, latest_token) = read_token(app_data_dir)?;
        if !token_needs_refresh(latest_token.expires_at) {
            return Self::new(latest_token);
        }
        token_path.clone_from(&latest_path);
        client = Self::new(latest_token)?;
        client.refresh_access_token(
            &token_path,
            app_key.expect("credentials checked above"),
            secret_key.expect("credentials checked above"),
        )?;
        Ok(client)
    }

    pub fn connection_status(app_data_dir: &Path) -> BaiduConnectionStatus {
        let path = token_paths(app_data_dir).into_iter().find(|path| path.is_file());
        let token = path.as_ref().and_then(|path| fs::read(path).ok()).and_then(|raw| serde_json::from_slice::<BaiduToken>(&raw).ok());
        let expires_at = token.as_ref().and_then(|token| token.expires_at);
        BaiduConnectionStatus {
            authorized: token.as_ref().is_some_and(|token| !token.access_token.trim().is_empty()),
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
        let _guard = lock.lock().map_err(|_| "百度网盘 Token 刷新锁不可用".to_string())?;
        save_token_at(&app_data_dir.join(TOKEN_FILE_NAME), token)
    }

    pub fn new(token: BaiduToken) -> Result<Self, String> {
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
        Ok(Self { client, token })
    }

    fn refresh_access_token(
        &mut self,
        token_path: &Path,
        app_key: &str,
        secret_key: &str,
    ) -> Result<(), String> {
        let refresh_token = self.token.refresh_token.as_deref().filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "百度网盘授权即将过期，但没有 refresh token，请重新授权".to_string())?;
        let response = self.client
            .get(OAUTH_TOKEN_URL)
            .query(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", app_key),
                ("client_secret", secret_key),
            ])
            .send()
            .map_err(|error| format!("请求百度 Token 刷新失败：{error}"))?;
        let status = response.status();
        let body = response.text().map_err(|error| format!("读取百度 Token 刷新响应失败：{error}"))?;
        let value = serde_json::from_str::<serde_json::Value>(&body)
            .map_err(|error| format!("百度 Token 刷新返回非 JSON：HTTP {status}，{error}"))?;
        let parsed = serde_json::from_value::<OAuthTokenResponse>(value.clone())
            .map_err(|error| format!("百度 Token 刷新响应格式无效：{error}"))?;
        if let Some(error_code) = parsed.error.as_deref() {
            let description = parsed.error_description.as_deref().unwrap_or("未知授权错误");
            return Err(format!("百度 Token 自动刷新失败：{description} ({error_code})"));
        }
        if !status.is_success() {
            return Err(format!("百度 Token 自动刷新失败：HTTP {status}"));
        }
        let access_token = parsed.access_token.filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "百度 Token 刷新响应缺少 access_token".to_string())?;
        let next_token = BaiduToken {
            access_token,
            expires_at: parsed.expires_in.map(|seconds| now_millis().saturating_add(seconds.saturating_mul(1000))).or(self.token.expires_at),
            refresh_token: parsed.refresh_token.filter(|value| !value.trim().is_empty()).or_else(|| self.token.refresh_token.clone()),
        };
        save_token_at(token_path, next_token.clone())?;
        self.token = next_token;
        Ok(())
    }

    pub fn list(&self, remote_dir: &str) -> Result<Vec<RemoteFile>, String> {
        let url = format!("{API_BASE}/rest/2.0/xpan/file");
        let token = self.token.access_token.clone();
        let mut start = 0usize;
        let mut result = Vec::new();
        loop {
            let response = self.send_with_retry(|client| client.get(&url)
                .query(&[("method", "list"), ("dir", remote_dir), ("order", "name"), ("start", &start.to_string()), ("limit", "1000"), ("web", "1"), ("folder", "0"), ("access_token", token.as_str())])
                .send(), "请求百度网盘文件列表")?;
            let body: FileListResponse = parse_json(response, "读取百度网盘文件列表")?;
            let page = body.list.unwrap_or_default();
            let page_len = page.len();
            result.extend(page.into_iter().map(|item| RemoteFile {
                path: item.path,
                fs_id: item.fs_id,
                size: item.size,
                md5: item.md5,
                is_dir: item.isdir != 0,
                server_mtime: item.server_mtime,
            }));
            if page_len == 0 || body.has_more != Some(1) {
                break;
            }
            start = start.saturating_add(page_len);
        }
        Ok(result)
    }

    pub fn quota(&self) -> Result<BaiduQuota, String> {
        let token = self.token.access_token.clone();
        let response = self.send_with_retry(|client| client.get(format!("{API_BASE}/api/quota"))
            .query(&[("access_token", token.as_str()), ("checkfree", "1"), ("checkexpire", "1")])
            .send(), "查询百度网盘空间")?;
        let body: QuotaResponse = parse_json(response, "读取百度网盘空间")?;
        Ok(BaiduQuota { total: body.total, used: body.used, free: body.free, expires_soon: body.expire })
    }

    pub fn delete_file(&self, remote_path: &str) -> Result<(), String> {
        let filelist = serde_json::to_string(&[remote_path]).map_err(|error| format!("生成百度删除请求失败：{error}"))?;
        let token = self.token.access_token.clone();
        let response = self.send_with_retry(|client| client.post(format!("{API_BASE}/rest/2.0/xpan/file"))
            .query(&[("method", "filemanager"), ("opera", "delete"), ("access_token", token.as_str())])
            .form(&[("async", "0"), ("filelist", filelist.as_str())])
            .send(), "删除百度网盘本体包")?;
        let _: serde_json::Value = parse_json(response, "删除百度网盘本体包")?;
        Ok(())
    }

    pub fn ensure_directory(&self, remote_dir: &str) -> Result<(), String> {
        let mut current = String::new();
        for component in remote_dir.split('/').filter(|component| !component.is_empty()) {
            current.push('/');
            current.push_str(component);
            match self.list(&current) {
                Ok(_) => continue,
                Err(error) if !error.contains("(-9)") && !error.contains("(-8)") => return Err(error),
                Err(_) => {}
            }
            let url = format!("{API_BASE}/rest/2.0/xpan/file");
            let token = self.token.access_token.clone();
            let response = self.send_with_retry(|client| client.post(&url)
                .query(&[("method", "create"), ("access_token", token.as_str())])
                .form(&[("path", current.as_str()), ("isdir", "1"), ("rtype", "3")])
                .send(), "创建百度网盘目录")?;
            let body = response.text().map_err(|err| format!("创建百度网盘目录读取响应失败：{err}"))?;
            let value = serde_json::from_str::<serde_json::Value>(&body)
                .map_err(|err| format!("创建百度网盘目录返回非 JSON：{err}"))?;
            if value.get("errno").and_then(serde_json::Value::as_i64) == Some(-8) {
                continue;
            }
            let _: serde_json::Value = parse_value(value, "创建百度网盘目录")?;
        }
        Ok(())
    }

    pub fn upload_file(&self, local_path: &Path, remote_path: &str, on_progress: impl Fn(u8, &str) -> bool) -> Result<RemoteFile, String> {
        let metadata = fs::metadata(local_path).map_err(|err| format!("读取待上传本体包失败：{err}"))?;
        let total_size = metadata.len();
        if total_size == 0 { return Err("不能上传空的本体包".to_string()); }
        let block_md5 = block_md5_list(local_path, total_size)?;
        let block_list = serde_json::to_string(&block_md5).map_err(|err| format!("生成百度分片清单失败：{err}"))?;
        let url = format!("{API_BASE}/rest/2.0/xpan/file");
        let token = self.token.access_token.clone();
        let precreate: PrecreateResponse = parse_json(self.send_with_retry(|client| client.post(&url)
            .query(&[("method", "precreate"), ("access_token", token.as_str())])
            .form(&[("path", remote_path), ("size", &total_size.to_string()), ("isdir", "0"), ("autoinit", "1"), ("block_list", &block_list), ("rtype", "3")])
            .send(), "请求百度预创建")?, "百度预创建")?;
        let upload_id = precreate.uploadid.ok_or_else(|| "百度预创建未返回 uploadid".to_string())?;
        let url = format!("{UPLOAD_API_BASE}/rest/2.0/pcs/file");
        let token = self.token.access_token.clone();
        let located: LocateResponse = parse_json(self.send_with_retry(|client| client.get(&url)
            .query(&[("method", "locateupload"), ("appid", "250528"), ("access_token", token.as_str()), ("path", remote_path), ("uploadid", upload_id.as_str()), ("upload_version", "2.0")])
            .send(), "定位百度上传服务器")?, "定位百度上传服务器")?;
        let host = located.servers.unwrap_or_default().into_iter().chain(located.bak_servers.unwrap_or_default()).find_map(|item| item.server).or(located.host).ok_or_else(|| "百度未返回可用上传服务器".to_string())?;
        let host = if host.starts_with("http") { host } else { format!("https://{host}") };
        let mut file = fs::File::open(local_path).map_err(|err| format!("打开待上传本体包失败：{err}"))?;
        let chunk_count = block_md5.len();
        for index in 0..chunk_count {
            let length = (total_size.saturating_sub(index as u64 * CHUNK_SIZE)).min(CHUNK_SIZE) as usize;
            let mut bytes = vec![0u8; length];
            file.read_exact(&mut bytes).map_err(|err| format!("读取本体包分片失败：{err}"))?;
            let upload_url = format!("{host}/rest/2.0/pcs/superfile2");
            let token = self.token.access_token.clone();
            let response = self.send_with_retry(|client| {
                let form = multipart::Form::new().part("file", multipart::Part::bytes(bytes.clone()).file_name("package.zip"));
                client.post(&upload_url)
                    .query(&[("method", "upload"), ("access_token", token.as_str()), ("type", "tmpfile"), ("path", remote_path), ("uploadid", upload_id.as_str()), ("upload_version", "2.0"), ("partseq", &index.to_string())])
                    .multipart(form).send()
            }, &format!("上传百度本体包分片 {}/{}", index + 1, chunk_count))?;
            let _: serde_json::Value = parse_json(response, "上传百度本体包分片")?;
            if !on_progress(10 + (((index + 1) * 80) / chunk_count.max(1)) as u8, &format!("正在上传本体包分片 {}/{}", index + 1, chunk_count)) {
                return Err("任务已取消".to_string());
            }
        }
        let url = format!("{API_BASE}/rest/2.0/xpan/file");
        let token = self.token.access_token.clone();
        let created: CreatedFileResponse = parse_json(self.send_with_retry(|client| client.post(&url)
            .query(&[("method", "create"), ("access_token", token.as_str())])
            .form(&[("path", remote_path), ("size", &total_size.to_string()), ("isdir", "0"), ("uploadid", upload_id.as_str()), ("block_list", &block_list), ("rtype", "3"), ("is_revision", "1")])
            .send(), "提交百度本体包")?, "提交百度本体包")?;
        on_progress(100, "本体包上传完成");
        Ok(RemoteFile { path: remote_path.to_string(), fs_id: created.fs_id, size: created.size.max(total_size), md5: created.md5, is_dir: false, server_mtime: None })
    }

    pub fn download_file(&self, remote: &RemoteFile, target_path: &Path, on_progress: impl Fn(u8, &str) -> bool) -> Result<(), String> {
        let fsids = serde_json::to_string(&[remote.fs_id]).map_err(|err| format!("生成百度下载请求失败：{err}"))?;
        let url = format!("{API_BASE}/rest/2.0/xpan/multimedia");
        let token = self.token.access_token.clone();
        let metadata: MetaResponse = parse_json(self.send_with_retry(|client| client.get(&url)
            .query(&[("method", "filemetas"), ("access_token", token.as_str()), ("fsids", fsids.as_str()), ("dlink", "1")])
            .send(), "请求百度本体包下载地址")?, "读取百度本体包下载地址")?;
        let dlink = metadata.list.and_then(|list| list.into_iter().next()).and_then(|item| item.dlink).ok_or_else(|| "百度未返回本体包下载地址".to_string())?;
        let token = self.token.access_token.clone();
        let mut response = self.send_with_retry(|client| client.get(&dlink)
            .query(&[("access_token", token.as_str())])
            .send(), "下载百度本体包")?;
        if !response.status().is_success() { return Err(format!("下载百度本体包失败：HTTP {}", response.status())); }
        let parent = target_path.parent().ok_or_else(|| "本体包下载路径无父目录".to_string())?;
        fs::create_dir_all(parent).map_err(|err| format!("创建本体包下载目录失败：{err}"))?;
        let temporary = target_path.with_extension("download.tmp");
        let mut output = fs::File::create(&temporary).map_err(|err| format!("创建本体包下载临时文件失败：{err}"))?;
        let mut written = 0u64;
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let read = response.read(&mut buffer).map_err(|err| format!("读取百度本体包失败：{err}"))?;
            if read == 0 { break; }
            output.write_all(&buffer[..read]).map_err(|err| format!("写入本体包下载文件失败：{err}"))?;
            written = written.saturating_add(read as u64);
            if !on_progress(5 + ((written.min(remote.size) * 90 / remote.size.max(1)) as u8), &format!("正在下载本体包 {} / {} MB", written / 1024 / 1024, remote.size / 1024 / 1024)) {
                let _ = fs::remove_file(&temporary);
                return Err("任务已取消".to_string());
            }
        }
        output.sync_all().map_err(|err| format!("刷新本体包下载文件失败：{err}"))?;
        if written != remote.size { let _ = fs::remove_file(&temporary); return Err(format!("本体包下载大小不匹配：{} / {}", written, remote.size)); }
        fs::rename(&temporary, target_path).map_err(|err| format!("提交本体包下载文件失败：{err}"))?;
        on_progress(100, "本体包下载完成");
        Ok(())
    }

    fn send_with_retry<F>(&self, mut request: F, operation: &str) -> Result<Response, String>
    where
        F: FnMut(&Client) -> Result<Response, reqwest::Error>,
    {
        let mut last_error = None;
        for attempt in 0..MAX_REQUEST_ATTEMPTS {
            match request(&self.client) {
                Ok(response) if response.status().is_success() || !is_retryable_status(response.status()) => return Ok(response),
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
        Err(format!("{operation}失败：{}", last_error.unwrap_or_else(|| "未知网络错误".to_string())))
    }
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
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
    let Some(path) = token_paths(app_data_dir).into_iter().find(|path| path.is_file()) else {
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
    let parent = path.parent().ok_or_else(|| "百度 Token 路径无父目录".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("创建百度 Token 目录失败：{error}"))?;
    let bytes = serde_json::to_vec_pretty(&token).map_err(|error| format!("序列化百度 Token 失败：{error}"))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = parent.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple()));
    let backup = parent.join(format!(".{name}.bak-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temporary).map_err(|error| format!("创建百度 Token 临时文件失败：{error}"))?;
        file.write_all(&bytes).map_err(|error| format!("写入百度 Token 临时文件失败：{error}"))?;
        file.sync_all().map_err(|error| format!("刷新百度 Token 临时文件失败：{error}"))?;
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
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|duration| duration.as_millis() as u64).unwrap_or(0)
}

fn block_md5_list(path: &Path, total_size: u64) -> Result<Vec<String>, String> {
    let mut file = fs::File::open(path).map_err(|err| format!("打开本体包失败：{err}"))?;
    let mut result = Vec::new();
    let mut remaining = total_size;
    let mut buffer = vec![0u8; CHUNK_SIZE as usize];
    while remaining > 0 {
        let length = remaining.min(CHUNK_SIZE) as usize;
        file.read_exact(&mut buffer[..length]).map_err(|err| format!("读取本体包分片失败：{err}"))?;
        result.push(md5_hex(&buffer[..length]));
        remaining -= length as u64;
    }
    Ok(result)
}

fn parse_json<T: for<'de> Deserialize<'de>>(response: reqwest::blocking::Response, operation: &str) -> Result<T, String> {
    let status = response.status();
    let body = response.text().map_err(|err| format!("{operation}读取响应失败：{err}"))?;
    let value = serde_json::from_str::<serde_json::Value>(&body).map_err(|_| format!("{operation}返回非 JSON：HTTP {status}"))?;
    parse_value(value, operation)
}

fn parse_value<T: for<'de> Deserialize<'de>>(value: serde_json::Value, operation: &str) -> Result<T, String> {
    if let Some(errno) = value.get("errno").and_then(serde_json::Value::as_i64).filter(|value| *value != 0) {
        let message = value.get("errmsg").and_then(serde_json::Value::as_str).or_else(|| value.get("error_msg").and_then(serde_json::Value::as_str)).unwrap_or("未知错误");
        return Err(format!("{operation}失败：{message} ({errno})"));
    }
    if let Some(error_code) = value.get("error_code").and_then(serde_json::Value::as_i64) {
        let message = value.get("error_description").and_then(serde_json::Value::as_str).or_else(|| value.get("error_msg").and_then(serde_json::Value::as_str)).unwrap_or("未知错误");
        return Err(format!("{operation}失败：{message} ({error_code})"));
    }
    serde_json::from_value(value).map_err(|err| format!("{operation}响应格式无效：{err}"))
}

fn md5_hex(input: &[u8]) -> String {
    let mut message = input.to_vec();
    let bit_length = (message.len() as u64).saturating_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 { message.push(0); }
    message.extend_from_slice(&bit_length.to_le_bytes());
    let mut state = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
    let shifts = [7u32, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let constants = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];
    for chunk in message.chunks_exact(64) {
        let mut words = [0u32; 16];
        for (index, word) in words.iter_mut().enumerate() { *word = u32::from_le_bytes(chunk[index * 4..index * 4 + 4].try_into().unwrap()); }
        let original = state;
        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        for index in 0..64 {
            let (function, word_index, shift) = if index < 16 { ((b & c) | ((!b) & d), index, shifts[index % 4]) } else if index < 32 { ((d & b) | ((!d) & c), (5 * index + 1) % 16, shifts[4 + index % 4]) } else if index < 48 { (b ^ c ^ d, (3 * index + 5) % 16, shifts[8 + index % 4]) } else { (c ^ (b | (!d)), (7 * index) % 16, shifts[12 + index % 4]) };
            let next = a.wrapping_add(function).wrapping_add(constants[index]).wrapping_add(words[word_index]).rotate_left(shift);
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
    state.iter().flat_map(|word| word.to_le_bytes()).map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::{md5_hex, token_needs_refresh, BaiduNetdiskClient, BaiduToken};

    #[test]
    fn client_requires_access_token() {
        let result = BaiduNetdiskClient::new(super::BaiduToken { access_token: String::new(), expires_at: None, refresh_token: None });
        assert!(result.is_err());
    }

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"GameSaver"), "f28205fc3f14a3b8bfa43c894db2a24b");
    }

    #[test]
    fn token_refresh_window_is_five_minutes() {
        let now = super::now_millis();
        assert!(!token_needs_refresh(Some(now.saturating_add(5 * 60 * 1000 + 1))));
        assert!(token_needs_refresh(Some(now.saturating_add(5 * 60 * 1000 - 1))));
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
