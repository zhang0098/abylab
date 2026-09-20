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
            "resume" => "恢复当前工作区的持久会话",
            "compact" => "把较早的历史压缩成摘要",
            "goal" => "设置或控制长期目标（自动分轮推进）",
            "clear" => "清空对话滚动区",
            "model" => "实时切换模型",
            "effort" => "设置当前会话的推理强度",
            "permission" => "选择权限预设 · shift+tab 轮换",
            "plan" => "切换 Host 计划模式",
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

    /// The usage hint a new session opens with (`App::push_session_tip`).
    /// `index` cycles the set — one hint per session, not a live rotation.
    pub fn session_tip(self, index: usize) -> &'static str {
        const EN: [&str; 7] = [
            "esc interrupts a running turn — your draft survives",
            "enter queues a follow-up; ctrl+enter steers the active turn now",
            "click a tool to expand it · wheel always scrolls the conversation",
            "token usage + cache hit rate live in /status · it also names the session",
            "answers render markdown: headings, code, links, and images",
            "@ mentions a workspace file · the file browser filters as you type",
            "/new starts a fresh session · /theme switches packs · ctrl+t toggles dark/light",
        ];
        const ZH: [&str; 7] = [
            "esc 可中断当前轮次，草稿会保留",
            "enter 会排队后续消息；ctrl+enter 立即 steer 当前轮次",
            "点击工具可展开 · 滚轮始终滚动对话",
            "token 用量和缓存命中率见 /status · 会话身份也在那里",
            "回答支持 Markdown：标题、代码、链接和图片",
            "@ 可引用工作区文件 · 输入时文件浏览器实时过滤",
            "/new 新建会话 · /theme 切换主题包 · ctrl+t 切换明暗模式",
        ];
        match self {
            Self::En => EN[index % EN.len()],
            Self::Zh => ZH[index % ZH.len()],
        }
    }
}

pub const TIP_COUNT: usize = 7;

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
