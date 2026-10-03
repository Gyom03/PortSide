// Popup layout, hit-testing and Direct2D rendering. Everything is laid out in DIPs.

use std::collections::HashSet;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::{BOOL, HSTRING, Result, w};
use windows_numerics::Vector2;

use crate::scan::Snapshot;

pub const WIDTH: f32 = 380.0;
pub const ROW_H: f32 = 46.0;
const PAD: f32 = 16.0;
const HEADER_H: f32 = 102.0;
const LIST_TOP: f32 = HEADER_H + 1.0;
const LIST_PAD: f32 = 6.0;
const MAX_ROWS: usize = 8;
const EMPTY_H: f32 = 92.0;
const FOOTER_H: f32 = 64.0;
const BTN_H: f32 = 38.0;
const CANCEL_X: (f32, f32) = (12.0, 104.0);
const STOP_X: (f32, f32) = (112.0, WIDTH - 12.0);
const PORT_X: (f32, f32) = (46.0, 104.0);
const TITLE_X: f32 = 110.0;
const MEM_W: f32 = 74.0;

const BG: u32 = 0x151619;
const HOVER: u32 = 0x1e2024;
const SEP: u32 = 0x25272c;
const TEXT: u32 = 0xeceef1;
const TEXT2: u32 = 0xb6bac2;
const MUTED: u32 = 0x8a8f99;
const DIM: u32 = 0x5d626c;
const TRACK: u32 = 0x26282d;
const OTHER: u32 = 0x4b4f58;
const RED: u32 = 0xe5484d;
const RED_HI: u32 = 0xf05a5f;
const RED_DOWN: u32 = 0xc93c41;
const AMBER: u32 = 0xf2a65a;
const BTN: u32 = 0x23252a;
const BTN_HI: u32 = 0x2c2f35;
const CHECK: u32 = 0x4a4e57;
const CHECK_HI: u32 = 0x6b707b;
const SCROLL: u32 = 0x3a3d44;
const PORT_COLORS: [u32; 6] = [0x5ccfc4, 0xa98bfa, 0x74b3ff, 0xf5a76c, 0xef7fbf, 0x9ed56c];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Hit {
    None,
    Row(usize),
    Port(usize),
    Cancel,
    Stop,
}

pub struct State<'a> {
    pub snap: &'a Snapshot,
    pub selected: &'a HashSet<u32>,
    pub hover: Hit,
    pub pressed: Hit,
    pub scroll: f32,
    pub note: Option<&'a str>,
}

struct Fonts {
    big: IDWriteTextFormat,
    unit: IDWriteTextFormat,
    label: IDWriteTextFormat,
    legend: IDWriteTextFormat,
    legend_value: IDWriteTextFormat,
    title: IDWriteTextFormat,
    detail: IDWriteTextFormat,
    port: IDWriteTextFormat,
    mem: IDWriteTextFormat,
    button: IDWriteTextFormat,
}

struct Line {
    layout: IDWriteTextLayout,
    width: f32,
    baseline: f32,
}

pub struct Ui {
    factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    target: Option<(ID2D1HwndRenderTarget, ID2D1SolidColorBrush)>,
    round: ID2D1StrokeStyle,
    fonts: Fonts,
    dpi: f32,
}

fn color(hex: u32, a: f32) -> D2D1_COLOR_F {
    let c = |s: u32| ((hex >> s) & 0xff) as f32 / 255.0;
    D2D1_COLOR_F { r: c(16), g: c(8), b: c(0), a }
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: rect(x, y, w, h), radiusX: r, radiusY: r }
}

fn pt(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

pub fn fmt_bytes(b: u64) -> (String, &'static str) {
    let mb = b as f64 / 1048576.0;
    if mb < 999.5 {
        return (format!("{mb:.0}"), "MB");
    }
    let gb = b as f64 / 1073741824.0;
    let s = if gb < 9.995 {
        format!("{gb:.2}")
    } else if gb < 99.95 {
        format!("{gb:.1}")
    } else {
        format!("{gb:.0}")
    };
    (s, "GB")
}

fn bytes(b: u64) -> String {
    let (n, u) = fmt_bytes(b);
    format!("{n} {u}")
}

fn plural(n: usize) -> &'static str {
    if n > 1 { "s" } else { "" }
}

impl Ui {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let round = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_FLAT,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    miterLimit: 10.0,
                    dashStyle: D2D1_DASH_STYLE_SOLID,
                    dashOffset: 0.0,
                },
                None,
            )?;

            let mut coll: Option<IDWriteFontCollection> = None;
            dwrite.GetSystemFontCollection(&mut coll, false)?;
            let has = |name: &str| {
                let (mut i, mut ok) = (0u32, BOOL(0));
                coll.as_ref().is_some_and(|c| c.FindFamilyName(&HSTRING::from(name), &mut i, &mut ok).is_ok() && ok.as_bool())
            };
            let pick = |a: &str, b: &str| if has(a) { a.to_string() } else { b.to_string() };
            let text = pick("Segoe UI Variable Text", "Segoe UI");
            let display = pick("Segoe UI Variable Display", "Segoe UI");
            let mono = pick("Cascadia Mono", "Consolas");

            let f = |family: &str, weight: DWRITE_FONT_WEIGHT, size: f32| -> Result<IDWriteTextFormat> {
                let tf = dwrite.CreateTextFormat(
                    &HSTRING::from(family),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("en-us"),
                )?;
                tf.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                let sign = dwrite.CreateEllipsisTrimmingSign(&tf)?;
                let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
                tf.SetTrimming(&trim, &sign)?;
                Ok(tf)
            };
            let fonts = Fonts {
                big: f(&display, DWRITE_FONT_WEIGHT_SEMI_BOLD, 32.0)?,
                unit: f(&text, DWRITE_FONT_WEIGHT_NORMAL, 15.0)?,
                label: f(&text, DWRITE_FONT_WEIGHT_NORMAL, 12.5)?,
                legend: f(&text, DWRITE_FONT_WEIGHT_NORMAL, 11.5)?,
                legend_value: f(&text, DWRITE_FONT_WEIGHT_SEMI_BOLD, 11.5)?,
                title: f(&text, DWRITE_FONT_WEIGHT_SEMI_BOLD, 13.5)?,
                detail: f(&text, DWRITE_FONT_WEIGHT_NORMAL, 12.0)?,
                port: f(&mono, DWRITE_FONT_WEIGHT_SEMI_BOLD, 13.0)?,
                mem: f(&mono, DWRITE_FONT_WEIGHT_NORMAL, 12.5)?,
                button: f(&text, DWRITE_FONT_WEIGHT_SEMI_BOLD, 13.0)?,
            };
            Ok(Ui { factory, dwrite, target: None, round, fonts, dpi: 96.0 })
        }
    }

    // ---------------------------------------------------------------- layout

    fn list_h(n: usize) -> f32 {
        if n == 0 { EMPTY_H } else { n.min(MAX_ROWS) as f32 * ROW_H + 2.0 * LIST_PAD }
    }

    fn footer_top(n: usize) -> f32 {
        LIST_TOP + Self::list_h(n) + 1.0
    }

    pub fn height(&self, n: usize) -> f32 {
        Self::footer_top(n) + FOOTER_H
    }

    pub fn max_scroll(&self, n: usize) -> f32 {
        (n as f32 * ROW_H + 2.0 * LIST_PAD - Self::list_h(n)).max(0.0)
    }

    pub fn scale(&self) -> f32 {
        self.dpi / 96.0
    }

    pub fn set_dpi(&mut self, dpi: f32) {
        self.dpi = dpi;
        if let Some((rt, _)) = &self.target {
            unsafe { rt.SetDpi(dpi, dpi) };
        }
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if let Some((rt, _)) = &self.target {
            unsafe {
                let _ = rt.Resize(&D2D_SIZE_U { width: w, height: h });
            }
        }
    }

    pub fn hit(&self, x: f32, y: f32, n: usize, scroll: f32) -> Hit {
        let lh = Self::list_h(n);
        if n > 0 && y >= LIST_TOP && y < LIST_TOP + lh && (8.0..WIDTH - 8.0).contains(&x) {
            let yy = y - LIST_TOP - LIST_PAD + scroll;
            if yy >= 0.0 && ((yy / ROW_H) as usize) < n {
                let i = (yy / ROW_H) as usize;
                return if (PORT_X.0..PORT_X.1).contains(&x) { Hit::Port(i) } else { Hit::Row(i) };
            }
        }
        let by = Self::footer_top(n) + (FOOTER_H - BTN_H) / 2.0;
        if (by..by + BTN_H).contains(&y) {
            if (CANCEL_X.0..CANCEL_X.1).contains(&x) {
                return Hit::Cancel;
            }
            if (STOP_X.0..STOP_X.1).contains(&x) {
                return Hit::Stop;
            }
        }
        Hit::None
    }

    // ---------------------------------------------------------------- drawing

    fn line(&self, s: &str, fmt: &IDWriteTextFormat, max_w: f32) -> Option<Line> {
        let wide: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let layout = self.dwrite.CreateTextLayout(&wide, fmt, max_w.max(1.0), 100.0).ok()?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m).ok()?;
            let mut lm = [DWRITE_LINE_METRICS::default(); 1];
            let mut count = 0;
            layout.GetLineMetrics(Some(&mut lm), &mut count).ok()?;
            Some(Line { layout, width: m.width.min(max_w), baseline: lm[0].baseline })
        }
    }

    /// Draws a line of text with its baseline at `base`.
    fn put(&self, rt: &ID2D1HwndRenderTarget, brush: &ID2D1SolidColorBrush, l: &Line, x: f32, base: f32, c: u32) {
        unsafe {
            brush.SetColor(&color(c, 1.0));
            rt.DrawTextLayout(pt(x, base - l.baseline), &l.layout, brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
        }
    }

    fn fill(&self, rt: &ID2D1HwndRenderTarget, brush: &ID2D1SolidColorBrush, r: D2D1_ROUNDED_RECT, c: u32, a: f32) {
        unsafe {
            brush.SetColor(&color(c, a));
            rt.FillRoundedRectangle(&r, brush);
        }
    }

    fn ensure_target(&mut self, hwnd: HWND) -> Option<(ID2D1HwndRenderTarget, ID2D1SolidColorBrush)> {
        if self.target.is_none() {
            unsafe {
                let mut rc = RECT::default();
                GetClientRect(hwnd, &mut rc).ok()?;
                let props = D2D1_RENDER_TARGET_PROPERTIES {
                    // Software rendering: no GPU device, a few MB less, instant for a UI this size.
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                    dpiX: self.dpi,
                    dpiY: self.dpi,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                };
                let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd,
                    pixelSize: D2D_SIZE_U { width: (rc.right - rc.left) as u32, height: (rc.bottom - rc.top) as u32 },
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                };
                let rt = self.factory.CreateHwndRenderTarget(&props, &hprops).ok()?;
                rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                let brush = rt.CreateSolidColorBrush(&color(TEXT, 1.0), None).ok()?;
                self.target = Some((rt, brush));
            }
        }
        self.target.clone()
    }

    pub fn render(&mut self, hwnd: HWND, st: &State) {
        let Some((rt, brush)) = self.ensure_target(hwnd) else { return };
        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&color(BG, 1.0)));
            self.draw_header(&rt, &brush, st);
            self.draw_list(&rt, &brush, st);
            self.draw_footer(&rt, &brush, st);
            if let Err(e) = rt.EndDraw(None, None) {
                if e.code() == D2DERR_RECREATE_TARGET {
                    self.target = None;
                }
            }
        }
    }

    fn draw_header(&self, rt: &ID2D1HwndRenderTarget, brush: &ID2D1SolidColorBrush, st: &State) {
        let servers = &st.snap.servers;
        let n = servers.len();
        let total: u64 = servers.iter().map(|s| s.mem).sum();
        let sel: Vec<_> = servers.iter().filter(|s| st.selected.contains(&s.pid)).collect();
        let sel_mem: u64 = sel.iter().map(|s| s.mem).sum();

        let (num, unit) = fmt_bytes(if sel.is_empty() { total } else { sel_mem });
        let base = 46.0;
        if let (Some(big), Some(u)) = (self.line(&num, &self.fonts.big, 200.0), self.line(unit, &self.fonts.unit, 60.0)) {
            self.put(rt, brush, &big, PAD - 1.0, base, TEXT);
            self.put(rt, brush, &u, PAD + big.width + 5.0, base, MUTED);
        }
        let (label, c) = match st.note {
            Some(note) => (note.to_string(), RED_HI),
            None if !sel.is_empty() => (format!("freed by stopping {} server{}", sel.len(), plural(sel.len())), MUTED),
            None if n > 0 => (format!("used by {n} server{}", plural(n)), MUTED),
            None => ("no active servers".to_string(), MUTED),
        };
        if let Some(l) = self.line(&label, &self.fonts.label, 220.0) {
            self.put(rt, brush, &l, WIDTH - PAD - l.width, base - 3.0, c);
        }

        // Memory bar: each server in its color, then other apps, then free memory.
        let (bx, by, bw, bh) = (PAD, 60.0, WIDTH - 2.0 * PAD, 6.0);
        self.fill(rt, brush, rrect(bx, by, bw, bh, 3.0), TRACK, 1.0);
        let mem_total = st.snap.mem_total.max(1) as f32;
        let used = st.snap.mem_total.saturating_sub(st.snap.mem_avail);
        let mut x = bx;
        for s in servers {
            let w = (s.mem as f32 / mem_total * bw).max(2.0);
            let a = if sel.is_empty() || st.selected.contains(&s.pid) { 1.0 } else { 0.3 };
            self.fill(rt, brush, rrect(x, by, w, bh, 2.0), PORT_COLORS[s.ports[0].port as usize % 6], a);
            x += w + 2.0;
        }
        let other = used.saturating_sub(total);
        let ow = (other as f32 / mem_total * bw).min(bx + bw - x);
        if ow > 1.0 {
            self.fill(rt, brush, rrect(x, by, ow, bh, 2.0), OTHER, 1.0);
        }

        // Legend, spread over the full width (shorter wording if it would not fit)
        let lb = 88.0;
        let total_gb = format!("{:.0} GB", st.snap.mem_total as f64 / 1073741824.0);
        let free = format!("{} / {total_gb}", fmt_bytes(st.snap.mem_avail).0);
        let build = |other_label: &'static str| -> Option<(Vec<(u32, Line, Line)>, f32)> {
            let items = [(TEXT2, "Servers", bytes(total)), (OTHER, other_label, bytes(other)), (TRACK, "Free", free.clone())];
            let mut out = Vec::new();
            let mut w = 0.0;
            for (c, name, value) in items {
                let n = self.line(name, &self.fonts.legend, 120.0)?;
                let v = self.line(&value, &self.fonts.legend_value, 120.0)?;
                w += 12.0 + n.width + 4.0 + v.width;
                out.push((c, n, v));
            }
            Some((out, w))
        };
        let avail = WIDTH - 2.0 * PAD;
        let legend = build("Other apps").filter(|(_, w)| *w + 24.0 <= avail).or_else(|| build("Other"));
        if let Some((items, w)) = legend {
            let gap = ((avail - w) / 2.0).min(18.0);
            let mut x = PAD;
            for (c, n, v) in items {
                self.fill(rt, brush, rrect(x, lb - 7.5, 8.0, 8.0, 2.0), c, 1.0);
                x += 12.0;
                self.put(rt, brush, &n, x, lb, MUTED);
                x += n.width + 4.0;
                self.put(rt, brush, &v, x, lb, TEXT2);
                x += v.width + gap;
            }
        }
        unsafe {
            brush.SetColor(&color(SEP, 1.0));
            rt.FillRectangle(&rect(0.0, HEADER_H, WIDTH, 1.0), brush);
        }
    }

    fn draw_list(&self, rt: &ID2D1HwndRenderTarget, brush: &ID2D1SolidColorBrush, st: &State) {
        let servers = &st.snap.servers;
        let n = servers.len();
        let lh = Self::list_h(n);
        if n == 0 {
            if let Some(l) = self.line("No local HTTP servers", &self.fonts.label, WIDTH) {
                self.put(rt, brush, &l, (WIDTH - l.width) / 2.0, LIST_TOP + lh / 2.0 + 4.5, MUTED);
            }
            return;
        }
        unsafe { rt.PushAxisAlignedClip(&rect(0.0, LIST_TOP, WIDTH, lh), D2D1_ANTIALIAS_MODE_ALIASED) };
        for (i, s) in servers.iter().enumerate() {
            let y = LIST_TOP + LIST_PAD + i as f32 * ROW_H - st.scroll;
            if y + ROW_H < LIST_TOP || y > LIST_TOP + lh {
                continue;
            }
            let selected = st.selected.contains(&s.pid);
            let hovered = matches!(st.hover, Hit::Row(j) | Hit::Port(j) if j == i);
            let base = y + ROW_H / 2.0 + 4.5;

            if selected {
                self.fill(rt, brush, rrect(8.0, y + 1.0, WIDTH - 16.0, ROW_H - 2.0, 8.0), RED, if hovered { 0.16 } else { 0.11 });
            } else if hovered {
                self.fill(rt, brush, rrect(8.0, y + 1.0, WIDTH - 16.0, ROW_H - 2.0, 8.0), HOVER, 1.0);
            }

            // Checkbox
            let (cx, cy, cs) = (19.0, y + (ROW_H - 18.0) / 2.0, 18.0);
            unsafe {
                if selected {
                    self.fill(rt, brush, rrect(cx, cy, cs, cs, 5.0), RED, 1.0);
                    brush.SetColor(&color(0xffffff, 1.0));
                    rt.DrawLine(pt(cx + 4.8, cy + 9.4), pt(cx + 7.8, cy + 12.4), brush, 2.0, &self.round);
                    rt.DrawLine(pt(cx + 7.8, cy + 12.4), pt(cx + 13.4, cy + 6.0), brush, 2.0, &self.round);
                } else {
                    brush.SetColor(&color(if hovered { CHECK_HI } else { CHECK }, 1.0));
                    rt.DrawRoundedRectangle(&rrect(cx + 0.75, cy + 0.75, cs - 1.5, cs - 1.5, 4.5), brush, 1.5, None);
                }
            }

            // Port (click opens it in the browser)
            let first = s.ports[0];
            let pc = PORT_COLORS[first.port as usize % 6];
            if let Some(l) = self.line(&format!(":{}", first.port), &self.fonts.port, PORT_X.1 - PORT_X.0) {
                let x = PORT_X.0 + 4.0;
                self.put(rt, brush, &l, x, base, pc);
                if st.hover == Hit::Port(i) {
                    unsafe {
                        brush.SetColor(&color(pc, 0.8));
                        rt.FillRectangle(&rect(x, base + 2.5, l.width, 1.0), brush);
                    }
                }
            }

            // Memory, right-aligned
            let mem_right = WIDTH - 22.0;
            if let Some(l) = self.line(&bytes(s.mem), &self.fonts.mem, MEM_W) {
                let c = if s.mem >= 1 << 30 { AMBER } else { TEXT2 };
                self.put(rt, brush, &l, mem_right - l.width, base, c);
            }

            // Name + detail
            let mut parts: Vec<String> = Vec::new();
            if !s.detail.is_empty() {
                parts.push(s.detail.clone());
            }
            parts.extend(s.ports.iter().skip(1).map(|p| format!(":{}", p.port)));
            if first.tls {
                parts.push("https".into());
            }
            let detail = parts.join(" · ");
            let avail = mem_right - MEM_W - 8.0 - TITLE_X;
            let title_max = if detail.is_empty() { avail } else { avail * 0.72 };
            if let Some(t) = self.line(&s.name, &self.fonts.title, title_max) {
                self.put(rt, brush, &t, TITLE_X, base, TEXT);
                let dx = TITLE_X + t.width + 7.0;
                if !detail.is_empty() && avail - (dx - TITLE_X) > 16.0 {
                    if let Some(d) = self.line(&detail, &self.fonts.detail, avail - (dx - TITLE_X)) {
                        self.put(rt, brush, &d, dx, base, MUTED);
                    }
                }
            }
        }
        unsafe { rt.PopAxisAlignedClip() };

        let max = self.max_scroll(n);
        if max > 0.0 {
            let content = lh + max;
            let th = lh * lh / content;
            let ty = LIST_TOP + st.scroll / max * (lh - th);
            self.fill(rt, brush, rrect(WIDTH - 6.0, ty + 3.0, 3.0, th - 6.0, 1.5), SCROLL, 1.0);
        }
    }

    fn draw_footer(&self, rt: &ID2D1HwndRenderTarget, brush: &ID2D1SolidColorBrush, st: &State) {
        let n = st.snap.servers.len();
        let top = Self::footer_top(n);
        unsafe {
            brush.SetColor(&color(SEP, 1.0));
            rt.FillRectangle(&rect(0.0, top - 1.0, WIDTH, 1.0), brush);
        }
        let by = top + (FOOTER_H - BTN_H) / 2.0;
        let base = by + BTN_H / 2.0 + 4.5;

        let c = if st.hover == Hit::Cancel { BTN_HI } else { BTN };
        self.fill(rt, brush, rrect(CANCEL_X.0, by, CANCEL_X.1 - CANCEL_X.0, BTN_H, 8.0), c, 1.0);
        if let Some(l) = self.line("Cancel", &self.fonts.button, 90.0) {
            self.put(rt, brush, &l, (CANCEL_X.0 + CANCEL_X.1 - l.width) / 2.0, base, TEXT2);
        }

        let sel: Vec<_> = st.snap.servers.iter().filter(|s| st.selected.contains(&s.pid)).collect();
        let (bg, fg, label) = if sel.is_empty() {
            (BTN, DIM, "Stop".to_string())
        } else {
            let mem: u64 = sel.iter().map(|s| s.mem).sum();
            let bg = match (st.hover, st.pressed) {
                (Hit::Stop, Hit::Stop) => RED_DOWN,
                (Hit::Stop, _) => RED_HI,
                _ => RED,
            };
            (bg, 0xffffff, format!("Stop {} server{} · free {}", sel.len(), plural(sel.len()), bytes(mem)))
        };
        self.fill(rt, brush, rrect(STOP_X.0, by, STOP_X.1 - STOP_X.0, BTN_H, 8.0), bg, 1.0);
        if let Some(l) = self.line(&label, &self.fonts.button, STOP_X.1 - STOP_X.0 - 16.0) {
            self.put(rt, brush, &l, (STOP_X.0 + STOP_X.1 - l.width) / 2.0, base, fg);
        }
    }
}
