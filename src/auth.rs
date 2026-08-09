//! Windows Credential Manager 凭据边界。
//!
//! `tpi auth set <provider>` 将 token 写入 Windows Credential Manager；
//! 配置只保存 credential label（§18.4）。环境变量可以作为显式覆盖。

use keyring::{Entry, Error};

/// keyring service 名。
const SERVICE: &str = "tpi";

/// 写入凭据（§18.4：Windows Credential Manager）。
pub fn auth_set(provider: &str, token: &str) -> Result<(), String> {
    validate_provider(provider)?;
    if token.is_empty() {
        return Err("token 为空".into());
    }
    let entry =
        Entry::new(SERVICE, provider).map_err(|e| format!("创建 keyring entry 失败: {e}"))?;
    entry
        .set_password(token)
        .map_err(|e| format!("写入凭据失败: {e}"))?;
    Ok(())
}

/// 删除凭据。
pub fn auth_clear(provider: &str) -> Result<(), String> {
    validate_provider(provider)?;
    let entry =
        Entry::new(SERVICE, provider).map_err(|e| format!("创建 keyring entry 失败: {e}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(error) => Err(format!("删除凭据失败: {error}")),
    }
}

/// 读取凭据；不存在为 `Ok(None)`，keyring 故障保留为错误。
pub fn auth_get(provider: &str) -> Result<Option<String>, String> {
    validate_provider(provider)?;
    let entry =
        Entry::new(SERVICE, provider).map_err(|e| format!("创建 keyring entry 失败: {e}"))?;
    match entry.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(Error::NoEntry) => Ok(None),
        Err(error) => Err(format!("读取凭据失败: {error}")),
    }
}

fn validate_provider(provider: &str) -> Result<(), String> {
    if provider.trim().is_empty() {
        return Err("provider 不能为空".into());
    }
    if provider.chars().any(char::is_control) {
        return Err("provider 不能包含控制字符".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_provider;

    #[test]
    fn provider_label_must_be_nonempty_and_printable() {
        assert!(validate_provider("opencode-go").is_ok());
        assert!(validate_provider("").is_err());
        assert!(validate_provider("   ").is_err());
        assert!(validate_provider("bad\nlabel").is_err());
    }
}
