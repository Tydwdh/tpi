//! Windows Credential Manager 凭据边界。
//!
//! `tpi auth set <provider>` 将 token 写入 Windows Credential Manager；
//! 配置只保存 credential label（§18.4）。环境变量可以作为显式覆盖。

use keyring::Entry;

/// keyring service 名。
const SERVICE: &str = "tpi";

/// 写入凭据（§18.4：Windows Credential Manager）。
pub fn auth_set(provider: &str, token: &str) -> Result<(), String> {
    let entry =
        Entry::new(SERVICE, provider).map_err(|e| format!("创建 keyring entry 失败: {e}"))?;
    entry
        .set_password(token)
        .map_err(|e| format!("写入凭据失败: {e}"))?;
    Ok(())
}

/// 删除凭据。
pub fn auth_clear(provider: &str) -> Result<(), String> {
    let entry =
        Entry::new(SERVICE, provider).map_err(|e| format!("创建 keyring entry 失败: {e}"))?;
    entry
        .delete_credential()
        .map_err(|e| format!("删除凭据失败: {e}"))?;
    Ok(())
}

/// 读取凭据（无则 None；用于 provider 配置未提供 env 时的回退）。
pub fn auth_get(provider: &str) -> Option<String> {
    let entry = Entry::new(SERVICE, provider).ok()?;
    entry.get_password().ok()
}
