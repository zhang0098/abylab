//! Built-in TUI localization.
//!
//! ACP and plugin payloads remain authored by their owner. This module only
//! localizes client-owned chrome and built-in commands.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Locale {
    En,
    /// abylab ships Chinese-first; `/lang en` switches to English.
    #[default]
    Zh,
}

impl Locale {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "en" | "en-us" | "en_us" => Some(Self::En),
            "zh" | "zh-cn" | "zh_cn" | "cn" => Some(Self::Zh),
            _ => None,
        }
    }

    pub fn alternate(self) -> Self {
        match self {
            Self::En => Self::Zh,
            Self::Zh => Self::En,
        }
    }

    pub fn tr(self, en: &'static str, zh: &'static str) -> &'static str {
        match self {
            Self::En => en,
            Self::Zh => zh,
        }
    }

    pub fn command_desc(self, name: &str, fallback: &'static str) -> &'static str {
        if self == Self::En {
            return fallback;
        }
        match name {
            "help" => "显示帮助和使用提示",
            "keys" => "查看键盘快捷键",
            "new" => "开始一个新会话",
            "resume" => "恢复当前工作区之前的会话",
            "delete" => "永久删除当前会话及其持久化文件（随后新开会话）",
            "compact" => "把较早的历史压缩成摘要",
            "goal" => "设置或控制长期目标（自动分轮推进）",
            "clear" => "清空对话滚动区",
            "model" => "实时切换模型",
            "effort" => "设置当前会话的推理强度",
            "permission" => "选择权限预设 · shift+tab 切换",
            "image" => "发送本地图片（png/jpeg/webp/gif）",
            "clip" => "附加剪贴板图片（macOS/Linux）",
            "theme" => "切换明暗模式或主题包",
            "vim" => "切换 vim 模态编辑（默认关闭）",
            "status" => "状态、模型和实时用量统计",
            "lang" => "切换界面语言",
            "login" => "保存 API key 到 aby 主目录",
            "logout" => "删除已保存的 API key",
            "skill" => "按名字调用技能（内置命令同名时也能用）",
            "quit" => "退出 abylab",
            _ => fallback,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub language: Locale,
    /// Last selected model id (`/model`, `--model` overrides it per run).
    pub model: Option<String>,
    /// Last selected reasoning effort (`/effort`).
    pub effort: Option<String>,
    /// Last selected permission preset (`/permission`, shift+tab).
    pub permission: Option<String>,
    /// Appearance mode (`/theme dark|light`, ctrl+t): `dark` or `light`.
    pub theme: Option<String>,
    /// Active palette pack id (`/theme <pack>`).
    pub palette: Option<String>,
    /// What plain Enter does while the agent is busy (`queue` or `steer`).
    /// The accelerated ctrl+enter chord always uses the other one. Stored as a
    /// string so an unknown value costs the default, never the whole file.
    pub enter: Option<String>,
}

/// What plain Enter does while a turn is running.
///
/// This is deepseek-harness's `busyEnter` preference (`ui-conversation`): the
/// chord pair is always `Enter` = this, `ctrl+enter` = the other one, and an
/// idle session sends either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EnterBehavior {
    /// Queue behind the active turn; the FIFO drains when it ends (default).
    #[default]
    Queue,
    /// Steer into the active turn at its next step boundary.
    Steer,
}

impl EnterBehavior {
    pub fn as_str(self) -> &'static str {
        match self {
            EnterBehavior::Queue => "queue",
            EnterBehavior::Steer => "steer",
        }
    }

    /// Parse a persisted value; `None` is unknown.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "queue" | "q" => Some(EnterBehavior::Queue),
            "steer" | "s" => Some(EnterBehavior::Steer),
            _ => None,
        }
    }

    /// The mode the accelerated chord uses.
    pub fn flipped(self) -> Self {
        match self {
            EnterBehavior::Queue => EnterBehavior::Steer,
            EnterBehavior::Steer => EnterBehavior::Queue,
        }
    }

    /// The verb this mode contributes to a hint line.
    pub fn label(self, locale: Locale) -> &'static str {
        match self {
            EnterBehavior::Queue => locale.tr("queue", "排队"),
            EnterBehavior::Steer => locale.tr("steer", "插话"),
        }
    }

    /// Read the persisted preference, defaulting an absent or unknown value.
    pub fn from_settings(settings: &UiSettings) -> Self {
        settings
            .enter
            .as_deref()
            .and_then(Self::parse)
            .unwrap_or_default()
    }
}

impl UiSettings {
    /// Load `$ABYLAB_HOME/settings.json`; absent or malformed files fall back
    /// to defaults (settings are a convenience, never a requirement).
    pub fn load(home: &str) -> Self {
        std::fs::read_to_string(crate::runtime::settings_path(home))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}
