//! DeepSeek Harness Web UI design tokens, mapped 1:1 from
//! `packages/client/ui-theme/src/styles/design-platform.css`.
//!
//! Static palette (`--dsw-static-*`) plus semantic themes: a cold
//! neutral-bluish base kept deliberately quiet, with a small reserved
//! accent vocabulary — DeepSeek blue for brand/actions, gray-blue for
//! hints, green for success/liveness, amber for attention, red for
//! errors. Minimal, not monotone.

use ratatui::style::Color;
use serde_json::Value;

// --- static palette -------------------------------------------------------

#[allow(dead_code)]
pub const DEEPSEEK_50: Color = Color::Rgb(237, 243, 254);
#[allow(dead_code)]
pub const DEEPSEEK_100: Color = Color::Rgb(228, 237, 253);
#[allow(dead_code)]
pub const DEEPSEEK_200: Color = Color::Rgb(211, 226, 255);
#[allow(dead_code)]
pub const DEEPSEEK_300: Color = Color::Rgb(183, 200, 254);
#[allow(dead_code)]
pub const DEEPSEEK_400: Color = Color::Rgb(103, 158, 254);
#[allow(dead_code)]
pub const DEEPSEEK_450: Color = Color::Rgb(86, 134, 254);
#[allow(dead_code)]
pub const DEEPSEEK_500: Color = Color::Rgb(65, 118, 230);
#[allow(dead_code)]
pub const DEEPSEEK_600: Color = Color::Rgb(72, 104, 178);
#[allow(dead_code)]
pub const DEEPSEEK_800: Color = Color::Rgb(52, 65, 91);
#[allow(dead_code)]
pub const DEEPSEEK_900: Color = Color::Rgb(40, 49, 66);

#[allow(dead_code)]
pub const BLUISH_00: Color = Color::Rgb(255, 255, 255);
#[allow(dead_code)]
pub const BLUISH_50: Color = Color::Rgb(249, 250, 251);
#[allow(dead_code)]
pub const BLUISH_60: Color = Color::Rgb(245, 246, 247);
#[allow(dead_code)]
pub const BLUISH_75: Color = Color::Rgb(241, 243, 245);
#[allow(dead_code)]
pub const BLUISH_100: Color = Color::Rgb(235, 238, 242);
#[allow(dead_code)]
pub const BLUISH_150: Color = Color::Rgb(233, 236, 242);
#[allow(dead_code)]
pub const BLUISH_200: Color = Color::Rgb(225, 229, 238);
#[allow(dead_code)]
pub const BLUISH_300: Color = Color::Rgb(207, 211, 214);
#[allow(dead_code)]
pub const BLUISH_400: Color = Color::Rgb(173, 178, 184);
#[allow(dead_code)]
pub const BLUISH_500: Color = Color::Rgb(151, 157, 166);
#[allow(dead_code)]
pub const BLUISH_600: Color = Color::Rgb(129, 133, 140);
#[allow(dead_code)]
pub const BLUISH_700: Color = Color::Rgb(97, 102, 107);
#[allow(dead_code)]
pub const BLUISH_750: Color = Color::Rgb(67, 69, 74);
#[allow(dead_code)]
pub const BLUISH_800: Color = Color::Rgb(53, 54, 56);
#[allow(dead_code)]
pub const BLUISH_850: Color = Color::Rgb(44, 44, 46);
#[allow(dead_code)]
pub const BLUISH_875: Color = Color::Rgb(35, 35, 36);
#[allow(dead_code)]
pub const BLUISH_900: Color = Color::Rgb(27, 27, 28);
#[allow(dead_code)]
pub const BLUISH_950: Color = Color::Rgb(21, 21, 23);
#[allow(dead_code)]
pub const BLUISH_1000: Color = Color::Rgb(15, 17, 21);

#[allow(dead_code)]
pub const RED_400: Color = Color::Rgb(242, 90, 90);
#[allow(dead_code)]
pub const RED_500: Color = Color::Rgb(239, 68, 68);
#[allow(dead_code)]
pub const RED_600: Color = Color::Rgb(236, 19, 19);
#[allow(dead_code)]
pub const GREEN_400: Color = Color::Rgb(78, 209, 126);
#[allow(dead_code)]
pub const GREEN_500: Color = Color::Rgb(34, 197, 94);
#[allow(dead_code)]
pub const AMBER_400: Color = Color::Rgb(247, 173, 49);
#[allow(dead_code)]
pub const AMBER_500: Color = Color::Rgb(245, 158, 11);
#[allow(dead_code)]
pub const AMBER_600: Color = Color::Rgb(221, 134, 41);

/// Blue-gray (slate) hint tones — gray first, a cool blue undertone;
/// clearly quieter than the DeepSeek blues.
pub const SLATE_400: Color = Color::Rgb(108, 122, 150);
pub const SLATE_600: Color = Color::Rgb(84, 96, 120);

// --- semantic theme -------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Dark => "dark",
            Mode::Light => "light",
        }
    }
}

/// Closed token names for protocol 0 palettes (`tuiTheme.register` / Cordis theme update).
pub const TOKEN_NAMES: &[&str] = &[
    "bg",
    "surface",
    "panel",
    "fg",
    "fg_secondary",
    "fg_tertiary",
    "caption",
    "brand",
    "brand_soft",
    "bubble_bg",
    "bubble_fg",
    "border",
    "code_bg",
    "ok",
    "warn",
    "err",
    "hint",
    "chip_bg",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TokenMap {
    bg: Color,
    surface: Color,
    panel: Color,
    fg: Color,
    fg_secondary: Color,
    fg_tertiary: Color,
    caption: Color,
    brand: Color,
    brand_soft: Color,
    bubble_bg: Color,
    bubble_fg: Color,
    border: Color,
    code_bg: Color,
    ok: Color,
    warn: Color,
    err: Color,
    hint: Color,
    chip_bg: Color,
}

const DEFAULT_DARK: TokenMap = TokenMap {
    bg: BLUISH_1000,
    surface: BLUISH_950,
    panel: BLUISH_900,
    fg: BLUISH_50,
    fg_secondary: BLUISH_300,
    fg_tertiary: BLUISH_500,
    caption: BLUISH_600,
    brand: DEEPSEEK_450,
    brand_soft: DEEPSEEK_400,
    bubble_bg: BLUISH_900,
    bubble_fg: BLUISH_75,
    border: BLUISH_850,
    code_bg: BLUISH_950,
    ok: GREEN_400,
    warn: AMBER_400,
    err: RED_400,
    hint: SLATE_400,
    chip_bg: BLUISH_850,
};

const DEFAULT_LIGHT: TokenMap = TokenMap {
    bg: BLUISH_00,
    surface: BLUISH_50,
    panel: BLUISH_60,
    fg: BLUISH_1000,
    fg_secondary: BLUISH_750,
    fg_tertiary: BLUISH_700,
    caption: BLUISH_400,
    brand: DEEPSEEK_500,
    brand_soft: DEEPSEEK_450,
    bubble_bg: BLUISH_75,
    bubble_fg: BLUISH_1000,
    border: BLUISH_200,
    code_bg: BLUISH_60,
    ok: GREEN_500,
    warn: AMBER_600,
    err: RED_600,
    hint: SLATE_600,
    chip_bg: BLUISH_100,
};

/// Why a protocol-0 palette was rejected. Wrong `protocol` is not an error:
/// the compositor ignores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteError(pub String);

impl std::fmt::Display for PaletteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Pack a fresh install opens on: One, dark. The built-in `default` pack —
/// the DeepSeek tokens — stays in the gallery and selectable like the rest.
pub const DEFAULT_PACK: &str = "one";

/// A named dark/light token pack. Built-in `default` plus the Martty gallery.
#[derive(Clone, Debug, PartialEq)]
pub struct PalettePack {
    pub id: String,
    pub label: String,
    dark: TokenMap,
    light: TokenMap,
}

impl PalettePack {
    pub fn builtin_default() -> Self {
        Self {
            id: "default".into(),
            label: "Default".into(),
            dark: DEFAULT_DARK,
            light: DEFAULT_LIGHT,
        }
    }

    /// The Martty theme gallery, embedded verbatim from `npm/lib/palettes`.
    /// `default` is covered by [`Self::builtin_default`]; the rest are static
    /// packs selectable via the `/theme` picker without any plugin.
    pub fn builtin_gallery() -> Vec<Self> {
        const PACKS: &[(&str, &str)] = &[
            ("ayu", include_str!("palettes/ayu.json")),
            ("catppuccin", include_str!("palettes/catppuccin.json")),
            ("everforest", include_str!("palettes/everforest.json")),
            ("iceberg", include_str!("palettes/iceberg.json")),
            ("kanagawa", include_str!("palettes/kanagawa.json")),
            ("one", include_str!("palettes/one.json")),
            ("solarized", include_str!("palettes/solarized.json")),
            ("tomorrow", include_str!("palettes/tomorrow.json")),
        ];
        PACKS
            .iter()
            .filter_map(|(_, raw)| {
                serde_json::from_str::<Value>(raw)
                    .ok()
                    .and_then(|value| Self::from_json(&value).ok())
            })
            .collect()
    }

    /// Current-mode `Theme` for this pack. Toggle stays inside these maps.
    pub fn theme(&self, mode: Mode) -> Theme {
        Theme::from_maps(mode, self.dark, self.light)
    }

    /// Parse a palette object (`id` / `label` / complete `dark`+`light` maps).
    pub fn from_json(v: &Value) -> Result<Self, PaletteError> {
        let obj = v
            .as_object()
            .ok_or_else(|| PaletteError("palette must be an object".into()))?;
        for key in obj.keys() {
            if !matches!(key.as_str(), "id" | "label" | "dark" | "light") {
                return Err(PaletteError(format!("unknown palette field {key}")));
            }
        }
        let id = obj.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            return Err(PaletteError("palette id must be a non-empty string".into()));
        }
        let label = obj.get("label").and_then(Value::as_str).unwrap_or("");
        if label.is_empty() {
            return Err(PaletteError(
                "palette label must be a non-empty string".into(),
            ));
        }
        let dark = lifted_chip(parse_token_map(obj.get("dark").unwrap_or(&Value::Null))?);
        let light = lifted_chip(parse_token_map(obj.get("light").unwrap_or(&Value::Null))?);
        Ok(Self {
            id: id.to_string(),
            label: label.to_string(),
            dark,
            light,
        })
    }
}

/// How far a chip is lifted toward the pack's text colour when the pack hands
/// us `chip_bg == panel`. The built-in pack's own panel→chip step is 18/255;
/// this lands the gallery packs at 15–24, so the wash reads the same there.
const CHIP_LIFT: f32 = 0.12;

/// `chip_bg` is the background of a selected row (the picker highlight) drawn
/// *on* the panel layer. Every gallery pack ships `chip_bg` equal to its
/// `panel`, which highlights nothing — the row paints the same colour as the
/// popup under it. Lift the chip toward the text colour so it reads as a wash.
/// The gallery JSONs are embedded verbatim from `npm/lib/palettes`, so the
/// nudge belongs here rather than in the files.
fn lifted_chip(mut tokens: TokenMap) -> TokenMap {
    if tokens.chip_bg == tokens.panel {
        tokens.chip_bg = blend(tokens.panel, tokens.fg, CHIP_LIFT);
    }
    tokens
}

/// Linear blend `a → b` by `t` (`0.0` keeps `a`, `1.0` is `b`), per channel.
fn blend(a: Color, b: Color, t: f32) -> Color {
    let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else {
        return a;
    };
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
}

fn parse_token_map(v: &Value) -> Result<TokenMap, PaletteError> {
    let map = v
        .as_object()
        .ok_or_else(|| PaletteError("token map must be an object".into()))?;
    for key in map.keys() {
        if !TOKEN_NAMES.iter().any(|n| n == key) {
            return Err(PaletteError(format!("unknown token {key}")));
        }
    }
    Ok(TokenMap {
        bg: color_field(map, "bg")?,
        surface: color_field(map, "surface")?,
        panel: color_field(map, "panel")?,
        fg: color_field(map, "fg")?,
        fg_secondary: color_field(map, "fg_secondary")?,
        fg_tertiary: color_field(map, "fg_tertiary")?,
        caption: color_field(map, "caption")?,
        brand: color_field(map, "brand")?,
        brand_soft: color_field(map, "brand_soft")?,
        bubble_bg: color_field(map, "bubble_bg")?,
        bubble_fg: color_field(map, "bubble_fg")?,
        border: color_field(map, "border")?,
        code_bg: color_field(map, "code_bg")?,
        ok: color_field(map, "ok")?,
        warn: color_field(map, "warn")?,
        err: color_field(map, "err")?,
        hint: color_field(map, "hint")?,
        chip_bg: color_field(map, "chip_bg")?,
    })
}

fn color_field(map: &serde_json::Map<String, Value>, key: &str) -> Result<Color, PaletteError> {
    let Some(v) = map.get(key) else {
        return Err(PaletteError(format!("missing token {key}")));
    };
    let Some(s) = v.as_str() else {
        return Err(PaletteError(format!("token {key} must be #RRGGBB")));
    };
    parse_hex(s).ok_or_else(|| PaletteError(format!("token {key} must be #RRGGBB")))
}

fn parse_hex(s: &str) -> Option<Color> {
    let b = s.as_bytes();
    if b.len() != 7 || b[0] != b'#' {
        return None;
    }
    let r = u8::from_str_radix(std::str::from_utf8(&b[1..3]).ok()?, 16).ok()?;
    let g = u8::from_str_radix(std::str::from_utf8(&b[3..5]).ok()?, 16).ok()?;
    let bl = u8::from_str_radix(std::str::from_utf8(&b[5..7]).ok()?, 16).ok()?;
    Some(Color::Rgb(r, g, bl))
}

/// Semantic colors — a cold monochrome remap of the Web UI neutral-bluish
/// scale (the alias slot names are kept for reference).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub mode: Mode,
    /// `--dsw-alias-bg-base`
    pub bg: Color,
    /// `--dsw-alias-bg-layer-1`
    #[allow(dead_code)]
    pub surface: Color,
    /// `--dsw-alias-bg-layer-2` (panels, tool cards)
    pub panel: Color,
    /// `--dsw-alias-label-primary`
    pub fg: Color,
    /// `--dsw-alias-label-secondary`
    pub fg_secondary: Color,
    /// `--dsw-alias-label-tertiary`
    pub fg_tertiary: Color,
    /// `--dsw-alias-label-caption`
    pub caption: Color,
    /// `--dsw-alias-brand-primary-new-color…` — the DeepSeek blue accent
    pub brand: Color,
    /// `--dsw-alias-state-business-primary`
    pub brand_soft: Color,
    /// `--dsw-specific-bubble` (user message bubble)
    pub bubble_bg: Color,
    /// text on the bubble
    pub bubble_fg: Color,
    /// borders (`--dsw-alias-border-l2/l3` approximated on the layer stack)
    pub border: Color,
    /// `--dsw-alias-markdown-code-block`
    pub code_bg: Color,
    /// `--dsw-alias-state-success-primary` / secondary
    pub ok: Color,
    /// `--dsw-alias-state-warn-primary` / label
    pub warn: Color,
    /// `--dsw-alias-state-error-primary`
    pub err: Color,
    /// Gray-blue hint text (tip banner, informational chips) — quieter
    /// than `brand_soft`, warmer than the neutral grays.
    pub hint: Color,
    /// selection/status chip background (picker row highlight)
    pub chip_bg: Color,
    dark: TokenMap,
    light: TokenMap,
}

impl Theme {
    fn from_maps(mode: Mode, dark: TokenMap, light: TokenMap) -> Self {
        let t = match mode {
            Mode::Dark => dark,
            Mode::Light => light,
        };
        Theme {
            mode,
            bg: t.bg,
            surface: t.surface,
            panel: t.panel,
            fg: t.fg,
            fg_secondary: t.fg_secondary,
            fg_tertiary: t.fg_tertiary,
            caption: t.caption,
            brand: t.brand,
            brand_soft: t.brand_soft,
            bubble_bg: t.bubble_bg,
            bubble_fg: t.bubble_fg,
            border: t.border,
            code_bg: t.code_bg,
            ok: t.ok,
            warn: t.warn,
            err: t.err,
            hint: t.hint,
            chip_bg: t.chip_bg,
            dark,
            light,
        }
    }

    pub fn dark() -> Self {
        Self::from_maps(Mode::Dark, DEFAULT_DARK, DEFAULT_LIGHT)
    }

    pub fn light() -> Self {
        Self::from_maps(Mode::Light, DEFAULT_DARK, DEFAULT_LIGHT)
    }

    pub fn toggled(&self) -> Self {
        match self.mode {
            Mode::Dark => Self::from_maps(Mode::Light, self.dark, self.light),
            Mode::Light => Self::from_maps(Mode::Dark, self.dark, self.light),
        }
    }

    pub fn with_mode(&self, mode: Mode) -> Self {
        Self::from_maps(mode, self.dark, self.light)
    }

    /// Success accent used for finished tool glyphs and the idle dot.
    pub fn ok_soft(&self) -> Color {
        self.ok
    }

    /// Warn accent for chips, queue markers, and cautionary notices.
    pub fn warn_soft(&self) -> Color {
        self.warn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accent invariant: the brand accent is the DeepSeek blue — gray body,
    /// blue accents (主体灰色，蓝色点缀).
    #[test]
    fn brand_is_deepseek_blue() {
        assert_eq!(Theme::dark().brand, DEEPSEEK_450);
        assert_eq!(Theme::light().brand, DEEPSEEK_500);
    }

    /// Neutral surfaces stay grayscale; the reserved accent vocabulary
    /// (brand blue · gray-blue hint · green ok · amber warn · red err) is
    /// deliberately colored — minimal, not monotone.
    #[test]
    fn neutrals_stay_gray_and_accents_stay_colored() {
        fn is_gray(c: Color) -> bool {
            match c {
                Color::Rgb(r, g, b) => {
                    let (lo, hi) = (r.min(g).min(b), r.max(g).max(b));
                    hi - lo <= 20 // the neutral-bluish scale has a slight cool tint
                }
                _ => false,
            }
        }
        for t in [Theme::dark(), Theme::light()] {
            for c in [
                t.bg,
                t.surface,
                t.panel,
                t.fg,
                t.fg_secondary,
                t.fg_tertiary,
                t.caption,
                t.bubble_bg,
                t.bubble_fg,
                t.border,
                t.code_bg,
                t.chip_bg,
            ] {
                assert!(is_gray(c), "expected grayscale, got {c:?}");
            }
            for c in [t.brand, t.brand_soft, t.hint, t.ok, t.warn, t.err] {
                assert!(!is_gray(c), "accents must be colored, got {c:?}");
            }
        }
    }

    fn one_json() -> serde_json::Value {
        serde_json::from_str(include_str!("palettes/one.json")).unwrap()
    }

    #[test]
    fn gallery_fixture_parses_both_modes() {
        let pack = PalettePack::from_json(&one_json()).expect("one fixture");
        assert_eq!(pack.id, "one");
        assert_eq!(pack.label, "One");
        let dark = pack.theme(Mode::Dark);
        let light = pack.theme(Mode::Light);
        assert_eq!(dark.mode, Mode::Dark);
        assert_eq!(light.mode, Mode::Light);
        assert_eq!(dark.brand, Color::Rgb(97, 175, 239)); // #61AFEF
        assert_eq!(light.brand, Color::Rgb(47, 90, 243)); // #2F5AF3
        assert_eq!(dark.bg, Color::Rgb(40, 44, 52)); // #282C34
        assert_eq!(light.bg, Color::Rgb(248, 248, 248)); // #F8F8F8
    }

    #[test]
    fn missing_token_extra_token_and_bad_hex_are_errors() {
        let mut missing = one_json();
        missing["dark"].as_object_mut().unwrap().remove("brand");
        assert!(PalettePack::from_json(&missing).is_err());

        let mut extra = one_json();
        extra["dark"]
            .as_object_mut()
            .unwrap()
            .insert("neon".into(), serde_json::json!("#FFFFFF"));
        assert!(PalettePack::from_json(&extra).is_err());

        for bad in ["#61AFE", "61AFEF", "#61AFEF00", "#GGGGGG", "#61afef0", ""] {
            let mut hex = one_json();
            hex["dark"]["brand"] = serde_json::json!(bad);
            assert!(
                PalettePack::from_json(&hex).is_err(),
                "expected rejection for {bad}"
            );
        }
    }

    #[test]
    fn toggled_pack_stays_the_same_pack() {
        let pack = PalettePack::from_json(&one_json()).unwrap();
        let dark = pack.theme(Mode::Dark);
        let light = dark.toggled();
        assert_eq!(light.mode, Mode::Light);
        assert_eq!(light.brand, Color::Rgb(47, 90, 243));
        assert_ne!(light.brand, DEEPSEEK_450);
        assert_ne!(light.brand, DEEPSEEK_500);
        let back = light.toggled();
        assert_eq!(back.mode, Mode::Dark);
        assert_eq!(back.brand, Color::Rgb(97, 175, 239));
    }

    #[test]
    fn default_toggled_still_uses_deepseek_blue() {
        assert_eq!(Theme::dark().toggled().brand, DEEPSEEK_500);
        assert_eq!(Theme::light().toggled().brand, DEEPSEEK_450);
        assert_eq!(Theme::dark().toggled().brand, Theme::light().brand);
    }

    /// The Martty gallery ships as embedded protocol-0 packs: every file
    /// must parse and expose both modes — a broken pack must never ship.
    #[test]
    fn gallery_packs_parse_with_ids_and_both_modes() {
        let packs = PalettePack::builtin_gallery();
        let ids: Vec<&str> = packs.iter().map(|pack| pack.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "ayu",
                "catppuccin",
                "everforest",
                "iceberg",
                "kanagawa",
                "one",
                "solarized",
                "tomorrow"
            ]
        );
        assert!(!ids.contains(&"default"), "default stays builtin_default");
        for pack in &packs {
            let dark = pack.theme(Mode::Dark);
            let light = pack.theme(Mode::Light);
            assert_ne!(dark.bg, Color::Reset, "{} dark bg resolves", pack.id);
            assert_ne!(light.bg, Color::Reset, "{} light bg resolves", pack.id);
        }
        let ayu = packs.iter().find(|pack| pack.id == "ayu").unwrap();
        assert_eq!(ayu.label, "Ayu");
        assert_eq!(ayu.theme(Mode::Dark).bg, Color::Rgb(11, 14, 20));
    }

    /// The picker paints its selected row in `chip_bg` on a `panel` block. A
    /// pack that shipped the two colours equal would highlight nothing, so
    /// every pack — built-in, gallery, both modes — must lift the chip, and by
    /// a wash-sized step rather than a slab.
    #[test]
    fn every_pack_has_a_chip_that_stands_out_from_its_panel() {
        let mut packs = vec![PalettePack::builtin_default()];
        packs.extend(PalettePack::builtin_gallery());
        for pack in packs {
            for mode in [Mode::Dark, Mode::Light] {
                let theme = pack.theme(mode);
                let (Color::Rgb(pr, pg, pb), Color::Rgb(cr, cg, cb)) = (theme.panel, theme.chip_bg)
                else {
                    panic!("{} {mode:?}: palettes resolve to RGB", pack.id);
                };
                let step = [pr, pg, pb]
                    .iter()
                    .zip([cr, cg, cb])
                    .map(|(panel, chip)| panel.abs_diff(chip))
                    .max()
                    .unwrap_or_default();
                assert!(
                    step >= 10,
                    "{} {mode:?}: chip is a no-op, step {step}",
                    pack.id
                );
                assert!(
                    step <= 60,
                    "{} {mode:?}: chip is a slab, step {step}",
                    pack.id
                );
            }
        }
    }
}
